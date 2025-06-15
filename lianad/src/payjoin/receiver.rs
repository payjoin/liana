use std::{
    collections::HashMap,
    error::Error,
    sync::{self, Arc},
};

use liana::descriptors;

use payjoin::{
    bitcoin::{
        consensus::encode::serialize_hex, psbt::Input, secp256k1, FeeRate, OutPoint, Sequence,
        TxIn, Weight,
    },
    persist::OptionalTransitionOutcome,
    receive::{
        v2::{
            replay_receiver_event_log, MaybeInputsOwned, MaybeInputsSeen, OutputsUnknown,
            PayjoinProposal, ProvisionalProposal, Receiver, ReceiverState, ReceiverWithContext,
            SessionHistory, UncheckedProposal, WantsInputs, WantsOutputs,
        },
        InputPair,
    },
};

use crate::{
    bitcoin::BitcoinInterface,
    database::{Coin, CoinStatus, DatabaseConnection, DatabaseInterface},
    payjoin::{
        db::SessionMetadata,
        helpers::{finalize_psbt, post_request, OHTTP_RELAY},
    },
};

use super::{db::ReceiverPersister, types::PayjoinStatus};

fn read_from_directory(
    receiver: Receiver<ReceiverWithContext>,
    persister: &ReceiverPersister,
    db_conn: &mut Box<dyn DatabaseConnection>,
    bit: &mut sync::Arc<sync::Mutex<dyn BitcoinInterface>>,
    desc: &descriptors::LianaDescriptor,
    secp: &secp256k1::Secp256k1<secp256k1::VerifyOnly>,
) -> Result<(), Box<dyn Error>> {
    let mut receiver = receiver;
    let (req, context) = receiver
        .extract_req(OHTTP_RELAY)
        .expect("Failed to extract request");
    let proposal = match post_request(req.clone()) {
        Ok(ohttp_response) => {
            let state_transition = receiver
                .process_res(
                    ohttp_response
                        .bytes()
                        .expect("Failed to read response")
                        .as_ref(),
                    context,
                )
                .save(persister);
            match state_transition {
                Ok(OptionalTransitionOutcome::Progress(next_state)) => next_state,
                Ok(OptionalTransitionOutcome::Stasis(_current_state)) => {
                    return Err("NoResults".into())
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(e) => return Err(e),
    };
    check_proposal(proposal, persister, db_conn, bit, desc, secp)
}

fn check_proposal(
    proposal: Receiver<UncheckedProposal>,
    persister: &ReceiverPersister,
    db_conn: &mut Box<dyn DatabaseConnection>,
    bit: &mut sync::Arc<sync::Mutex<dyn BitcoinInterface>>,
    desc: &descriptors::LianaDescriptor,
    secp: &secp256k1::Secp256k1<secp256k1::VerifyOnly>,
) -> Result<(), Box<dyn Error>> {
    // Receive Check 1: Can Broadcast
    let proposal = proposal
        .check_broadcast_suitability(None, |tx| {
            let result = bit.test_mempool_accept(vec![serialize_hex(tx)]);
            match result.first().cloned() {
                Some(can_broadcast) => Ok(can_broadcast),
                None => Ok(false),
            }
        })
        .save(persister)?;
    check_inputs_not_owned(proposal, persister, db_conn, desc, secp)
}

fn check_inputs_not_owned(
    proposal: Receiver<MaybeInputsOwned>,
    persister: &ReceiverPersister,
    db_conn: &mut Box<dyn DatabaseConnection>,
    desc: &descriptors::LianaDescriptor,
    secp: &secp256k1::Secp256k1<secp256k1::VerifyOnly>,
) -> Result<(), Box<dyn Error>> {
    let proposal = proposal
        .check_inputs_not_owned(|_| Ok(false))
        .save(persister)?;
    check_no_inputs_seen_before(proposal, persister, db_conn, desc, secp)
}

fn check_no_inputs_seen_before(
    proposal: Receiver<MaybeInputsSeen>,
    persister: &ReceiverPersister,
    db_conn: &mut Box<dyn DatabaseConnection>,
    desc: &descriptors::LianaDescriptor,
    secp: &secp256k1::Secp256k1<secp256k1::VerifyOnly>,
) -> Result<(), Box<dyn Error>> {
    let proposal = proposal
        .check_no_inputs_seen_before(|_| Ok(false))
        .save(persister)?;
    identify_receiver_outputs(proposal, persister, db_conn, desc, secp)
}

fn identify_receiver_outputs(
    proposal: Receiver<OutputsUnknown>,
    persister: &ReceiverPersister,
    db_conn: &mut Box<dyn DatabaseConnection>,
    desc: &descriptors::LianaDescriptor,
    secp: &secp256k1::Secp256k1<secp256k1::VerifyOnly>,
) -> Result<(), Box<dyn Error>> {
    let proposal = proposal
        .identify_receiver_outputs(|_| Ok(true))
        .save(persister)?;
    commit_outputs(proposal, persister, db_conn, desc, secp)
}

fn commit_outputs(
    proposal: Receiver<WantsOutputs>,
    persister: &ReceiverPersister,
    db_conn: &mut Box<dyn DatabaseConnection>,
    desc: &descriptors::LianaDescriptor,
    secp: &secp256k1::Secp256k1<secp256k1::VerifyOnly>,
) -> Result<(), Box<dyn Error>> {
    let proposal = proposal.commit_outputs().save(persister)?;
    contribute_inputs(proposal, persister, db_conn, desc, secp)
}

fn contribute_inputs(
    proposal: Receiver<WantsInputs>,
    persister: &ReceiverPersister,
    db_conn: &mut Box<dyn DatabaseConnection>,
    desc: &descriptors::LianaDescriptor,
    secp: &secp256k1::Secp256k1<secp256k1::VerifyOnly>,
) -> Result<(), Box<dyn Error>> {
    let coins = db_conn.coins(&[CoinStatus::Confirmed], &[]);

    let mut candidate_inputs_map = HashMap::<OutPoint, (Coin, TxIn, Input)>::new();
    for (outpoint, coin) in coins.iter() {
        let txs = db_conn.list_wallet_transactions(&[outpoint.txid]);
        let (db_tx, _, _) = txs.first().unwrap();

        let tx = db_tx.clone();

        let txout = tx.tx_out(outpoint.vout as usize)?.clone();

        let derived_desc = if coin.is_change {
            desc.change_descriptor().derive(coin.derivation_index, secp)
        } else {
            desc.receive_descriptor()
                .derive(coin.derivation_index, secp)
        };

        let txin = TxIn {
            previous_output: *outpoint,
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            ..Default::default()
        };

        let mut psbtin = Input {
            non_witness_utxo: Some(tx.clone()),
            witness_utxo: Some(txout.clone()),
            ..Default::default()
        };

        derived_desc.update_psbt_in(&mut psbtin);

        candidate_inputs_map.insert(*outpoint, (*coin, txin, psbtin));
    }

    let candidate_inputs = candidate_inputs_map
        .values()
        .map(|(_, txin, psbtin)| InputPair::new(txin.clone(), psbtin.clone()).unwrap());

    let selected_input = proposal.try_preserving_privacy(candidate_inputs).unwrap();

    proposal
        .contribute_inputs_with_weights(
            vec![selected_input],
            vec![Weight::from_wu_usize(desc.max_sat_weight(true))],
        )?
        .commit_inputs()
        .save(persister)?;

    let (_, history) = replay_receiver_event_log(persister.clone())
        .map_err(|e| format!("Failed to replay receiver event log: {:?}", e))
        .unwrap();

    let psbt = history.psbt_with_contributed_inputs().unwrap();
    let bip21 = history.pj_uri().unwrap().to_string();

    db_conn.store_spend(&psbt);
    log::info!("[Payjoin] PSBT in the DB...");

    persister.update_metadata(
        Some(PayjoinStatus::Signing),
        Some(psbt.unsigned_tx.compute_txid()),
        Some(psbt.clone()),
        Some(bip21.clone()),
    );

    Ok(())
}

fn finalize_proposal(
    proposal: Receiver<ProvisionalProposal>,
    persister: &ReceiverPersister,
    history: SessionHistory,
    db_conn: &mut Box<dyn DatabaseConnection>,
    secp: &secp256k1::Secp256k1<secp256k1::VerifyOnly>,
) -> Result<(), Box<dyn Error>> {
    if let Some(txid) = history.proposal_txid() {
        if let Some(psbt) = db_conn.spend_tx(&txid) {
            let mut is_signed = false;
            for psbtin in &psbt.inputs {
                if !psbtin.partial_sigs.is_empty() {
                    log::debug!("[Payjoin] PSBT is signed!");
                    is_signed = true;
                    break;
                }
            }

            if is_signed {
                let mut psbt = psbt.clone();
                finalize_psbt(&mut psbt, secp);

                let proposal = proposal
                    .finalize_proposal(
                        |_| Ok(psbt.clone()),
                        None,
                        Some(FeeRate::from_sat_per_vb(150).unwrap()),
                    )
                    .save(persister)?;

                send_payjoin_proposal(proposal, persister, history)?;
            }
        }
    }
    Ok(())
}

fn send_payjoin_proposal(
    mut proposal: Receiver<PayjoinProposal>,
    persister: &ReceiverPersister,
    history: SessionHistory,
) -> Result<(), Box<dyn Error>> {
    let (req, ctx) = proposal
        .extract_req(OHTTP_RELAY)
        .expect("Failed to extract request");

    let psbt = proposal.psbt().clone();
    let txid = psbt.unsigned_tx.compute_txid();

    // Respond to sender
    log::info!("[Payjoin] Receiver responding to sender...");
    match post_request(req) {
        Ok(resp) => {
            proposal
                .process_res(resp.bytes().expect("Failed to read response").as_ref(), ctx)
                .save(persister)?;

            let bip21 = history.pj_uri().unwrap();
            persister.update_metadata(
                Some(PayjoinStatus::Completed),
                Some(txid),
                Some(psbt),
                Some(bip21.to_string()),
            );
        }
        Err(err) => log::error!("[Payjoin] send_payjoin_proposal(): {}", err),
    }
    Ok(())
}

fn process_receiver_session(
    db: &sync::Arc<sync::Mutex<dyn DatabaseInterface>>,
    bit: &mut sync::Arc<sync::Mutex<dyn BitcoinInterface>>,
    desc: &descriptors::LianaDescriptor,
    secp: &secp256k1::Secp256k1<secp256k1::VerifyOnly>,
) -> Result<(), Box<dyn Error>> {
    let mut db_conn = db.connection();
    for (session_id, session) in db_conn.payjoin_get_all_receiver_sessions() {
        let SessionMetadata {
            status,
            maybe_bip21,
            ..
        } = session.metadata.clone();

        // No need to check Completed
        if status == PayjoinStatus::Completed {
            continue;
        }

        log::info!("[Payjoin] {:?}: bip21={:?}", status, maybe_bip21);

        let persister =
            ReceiverPersister::from_id(Arc::new(db.clone()), session_id.clone()).unwrap();

        let (state, history) = replay_receiver_event_log(persister.clone())
            .map_err(|e| format!("Failed to replay receiver event log: {:?}", e))
            .unwrap();

        match state {
            ReceiverState::WithContext(context) => {
                read_from_directory(context, &persister, &mut db_conn, bit, desc, secp)?;
            }
            ReceiverState::UncheckedProposal(proposal) => {
                check_proposal(proposal, &persister, &mut db_conn, bit, desc, secp)?;
            }
            ReceiverState::MaybeInputsOwned(proposal) => {
                check_inputs_not_owned(proposal, &persister, &mut db_conn, desc, secp)?;
            }
            ReceiverState::MaybeInputsSeen(proposal) => {
                check_no_inputs_seen_before(proposal, &persister, &mut db_conn, desc, secp)?;
            }
            ReceiverState::OutputsUnknown(proposal) => {
                identify_receiver_outputs(proposal, &persister, &mut db_conn, desc, secp)?;
            }
            ReceiverState::WantsOutputs(proposal) => {
                commit_outputs(proposal, &persister, &mut db_conn, desc, secp)?;
            }
            ReceiverState::WantsInputs(proposal) => {
                contribute_inputs(proposal, &persister, &mut db_conn, desc, secp)?
            }
            ReceiverState::ProvisionalProposal(proposal) => {
                finalize_proposal(proposal, &persister, history, &mut db_conn, secp)?
            }
            ReceiverState::PayjoinProposal(proposal) => {
                send_payjoin_proposal(proposal, &persister, history)?
            }
            _ => return Err(format!("Unexpected receiver state: {:?}", state).into()),
        }
    }
    Ok(())
}

pub fn payjoin_receiver_check(
    db: &sync::Arc<sync::Mutex<dyn DatabaseInterface>>,
    bit: &mut sync::Arc<sync::Mutex<dyn BitcoinInterface>>,
    desc: &descriptors::LianaDescriptor,
    secp: &secp256k1::Secp256k1<secp256k1::VerifyOnly>,
) {
    match process_receiver_session(db, bit, desc, secp) {
        Ok(_) => (),
        Err(e) => log::warn!("process_receiver_session(): {}", e),
    }
}
