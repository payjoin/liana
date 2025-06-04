use std::{
    collections::HashMap,
    error::Error,
    sync::{self, Arc},
};

use liana::descriptors;

use payjoin::{
    bitcoin::{psbt::Input, secp256k1, FeeRate, OutPoint, Sequence, TxIn},
    persist::OptionalTransitionOutcome,
    receive::{
        v2::{
            replay_receiver_event_log, Receiver, ReceiverSessionEvent, ReceiverState,
            ReceiverWithContext, UncheckedProposal,
        },
        InputPair,
    },
};

use crate::{
    database::{Coin, CoinStatus, DatabaseConnection, DatabaseInterface},
    payjoin::{
        db::SessionMetadata,
        helpers::{finalize_psbt, post_request, OHTTP_RELAY},
    },
};

use super::{
    db::{ReceiverPersister, SessionWrapper},
    types::PayjoinStatus,
};

fn handle_directory_proposal(
    proposal: Receiver<UncheckedProposal>,
    persister: &ReceiverPersister,
    db_conn: &mut Box<dyn DatabaseConnection>,
    descs: &[descriptors::SinglePathLianaDesc],
    secp: &secp256k1::Secp256k1<secp256k1::VerifyOnly>,
) -> Result<(), Box<dyn Error>> {
    let coins = db_conn.coins(&[CoinStatus::Confirmed], &[]);

    let mut candidate_inputs_map = HashMap::<OutPoint, (Coin, TxIn, Input)>::new();
    for (outpoint, coin) in coins.iter() {
        let txs = db_conn.list_wallet_transactions(&[outpoint.txid]);
        let (db_tx, _, _) = txs.first().unwrap();

        let tx = db_tx.clone();

        let txout = tx.tx_out(outpoint.vout as usize)?.clone();

        let receiver_derived_desc = if coin.is_change {
            descs[1].derive(coin.derivation_index, secp)
        } else {
            descs[0].derive(coin.derivation_index, secp)
        };

        let txin = TxIn {
            previous_output: outpoint.clone(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            ..Default::default()
        };

        let mut psbtin = Input {
            non_witness_utxo: Some(tx.clone()),
            witness_utxo: Some(txout.clone()),
            ..Default::default()
        };

        receiver_derived_desc.update_psbt_in(&mut psbtin);

        candidate_inputs_map.insert(outpoint.clone(), (*coin, txin, psbtin));
    }

    let candidate_inputs = candidate_inputs_map
        .values()
        .map(|(_, txin, psbtin)| InputPair::new(txin.clone(), psbtin.clone()).unwrap());

    // in a payment processor where the sender could go offline, this is where you schedule to broadcast the original_tx
    let _to_broadcast_in_failure_case = proposal.extract_tx_to_schedule_broadcast();

    // Receive Check 1: Can Broadcast
    let proposal = proposal
        .check_broadcast_suitability(None, |_| Ok(true))
        .save(persister)?;

    // Receive Check 2: receiver can't sign for proposal inputs
    let proposal = proposal
        .check_inputs_not_owned(|_input| Ok(false))
        .save(persister)?;

    // Receive Check 3: have we seen this input before? More of a check for non-interactive i.e. payment processor receivers.
    let proposal = proposal
        .check_no_inputs_seen_before(|_| Ok(false))
        .save(persister)?;

    // Receive Check 4: identify receiver outputs
    let proposal = proposal
        .identify_receiver_outputs(|_| Ok(true))
        .save(persister)?;

    // Receive Check 5: commit outputs
    let proposal = proposal.commit_outputs().save(persister)?;

    let selected_input = proposal.try_preserving_privacy(candidate_inputs)?;

    proposal
        .contribute_inputs(vec![selected_input])
        .map_err(|e| format!("Failed to contribute inputs: {e:?}"))?
        .commit_inputs()
        .save(persister)?;

    Ok(())
}

fn poll_fallback(
    receiver: Receiver<ReceiverWithContext>,
    persister: &ReceiverPersister,
) -> Result<Receiver<UncheckedProposal>, Box<dyn Error>> {
    let mut receiver = receiver;
    let (req, context) = receiver
        .extract_req(OHTTP_RELAY)
        .expect("Failed to extract request");
    match post_request(req.clone()) {
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
                Ok(OptionalTransitionOutcome::Progress(next_state)) => Ok(next_state),
                Ok(OptionalTransitionOutcome::Stasis(_current_state)) => Err("NoResults".into()),
                Err(e) => return Err(e.into()),
            }
        }
        Err(e) => Err(e.into()),
    }
}

fn process_receiver_session(
    session: SessionWrapper<ReceiverSessionEvent>,
    state: ReceiverState,
    persister: ReceiverPersister,
    db_conn: &mut Box<dyn DatabaseConnection>,
    descs: &[descriptors::SinglePathLianaDesc],
    secp: &secp256k1::Secp256k1<secp256k1::VerifyOnly>,
) -> Result<(), Box<dyn Error>> {
    match state {
        ReceiverState::WithContext(receiver) => {
            log::info!("[Payjoin] ReceiverState::WithContext");
            if session.metadata.status == PayjoinStatus::Pending {
                match poll_fallback(receiver, &persister) {
                    Ok(proposal) => {
                        handle_directory_proposal(proposal, &persister, db_conn, descs, secp)?;

                        let (_, history) = replay_receiver_event_log(persister.clone())
                            .map_err(|e| format!("Failed to replay receiver event log: {:?}", e))
                            .unwrap();

                        let psbt = history.psbt_with_contributed_inputs().unwrap();
                        let bip21 = history.pj_uri().unwrap().to_string();

                        db_conn.store_spend(&psbt);
                        log::info!("[Payjoin] ReceiverState::WithContext: PSBT in the DB...");

                        persister.update_metada(
                            Some(PayjoinStatus::Signing),
                            Some(psbt.unsigned_tx.compute_txid()),
                            Some(psbt.clone()),
                            Some(bip21.clone()),
                        );
                    }
                    Err(_) => {}
                }
            }
            Ok(())
        }
        _ => return Err(format!("Unexpected receiver state: {:?}", state).into()),
    }
}

pub fn payjoin_receiver_check(
    db: &sync::Arc<sync::Mutex<dyn DatabaseInterface>>,
    descs: &[descriptors::SinglePathLianaDesc],
    secp: &secp256k1::Secp256k1<secp256k1::VerifyOnly>,
) {
    let mut db_conn = db.connection();
    for (session_id, session) in db_conn.payjoin_get_all_receiver_sessions() {
        let SessionMetadata {
            status,
            maybe_txid,
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

        let (state, _) = replay_receiver_event_log(persister.clone())
            .map_err(|e| format!("Failed to replay receiver event log: {:?}", e))
            .unwrap();

        match status {
            PayjoinStatus::Pending => {
                match process_receiver_session(session, state, persister, &mut db_conn, descs, secp)
                {
                    Ok(_) => (),
                    Err(e) => log::warn!("receiver_check(): {}", e),
                }
            }
            PayjoinStatus::Signing => {
                if let Some(txid) = maybe_txid {
                    match db_conn.spend_tx(&txid) {
                        Some(psbt) => {
                            let mut is_signed = false;
                            for psbtin in &psbt.inputs {
                                if !psbtin.partial_sigs.is_empty() {
                                    log::debug!("[Payjoin] PSBT is signed!");
                                    is_signed = true;
                                    break;
                                }
                            }

                            if is_signed {
                                match state {
                                    ReceiverState::ProvisionalProposal(proposal) => {
                                        let mut psbt = psbt.clone();
                                        finalize_psbt(&mut psbt, secp);

                                        let mut proposal = proposal
                                            .finalize_proposal(
                                                |_| Ok(psbt.clone()),
                                                None,
                                                Some(FeeRate::from_sat_per_vb(150).unwrap()),
                                            )
                                            .save(&persister)
                                            .unwrap();

                                        let (req, ctx) = proposal
                                            .extract_req(OHTTP_RELAY)
                                            .expect("Failed to extract request");

                                        // Respond to sender
                                        log::info!("[Payjoin] Receiver responding to sender...");
                                        match post_request(req.clone()) {
                                            Ok(resp) => {
                                                proposal
                                                    .process_res(resp.bytes().expect("Failed to read response").as_ref(), ctx)
                                                    .save(&persister)
                                                    .unwrap();

                                                persister.update_metada(Some(PayjoinStatus::Completed), maybe_txid, Some(psbt), maybe_bip21);
                                            },
                                            Err(err) => log::error!(
                                                "[Payjoin] payjoin_receiver_check(respond_to_sender): {} -> {}",
                                                req.url, err
                                            ),
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        None => {}
                    }
                }
            }
            _ => {}
        }
    }
}
