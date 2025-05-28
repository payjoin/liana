use std::{
    collections::HashMap,
    error::Error,
    sync::{self, Arc},
};

use liana::descriptors;

use payjoin::{
    bitcoin::{psbt::Input, secp256k1, FeeRate, OutPoint, Psbt, Sequence, TxIn},
    persist::{NoopPersister, PersistedError, PersistedSucccessWithMaybeNoResults},
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
    payjoin::helpers::{finalize_psbt, post_request, OHTTP_RELAY},
};

use super::{
    db::{ReceiverPersister, SessionId, SessionWrapper},
    types::PayjoinStatus,
};

fn handle_directory_proposal(
    proposal: Receiver<UncheckedProposal>,
    persister: &ReceiverPersister,
    db_conn: &mut Box<dyn DatabaseConnection>,
    descs: &[descriptors::SinglePathLianaDesc],
    secp: &secp256k1::Secp256k1<secp256k1::VerifyOnly>,
) -> Result<Psbt, Box<dyn Error>> {
    // descs must always have 2 descriptors
    assert_eq!(descs.len(), 2);
    let coins = db_conn.coins(&[CoinStatus::Confirmed], &[]);

    let mut candidate_inputs_map = HashMap::<OutPoint, (Coin, TxIn, Input)>::new();
    for (outpoint, coin) in coins.iter() {
        let txs = db_conn.list_wallet_transactions(&[outpoint.txid]);
        let (db_tx, _, _) = txs.first().unwrap();

        let tx = db_tx.clone();

        let txout = tx.tx_out(outpoint.vout as usize)?.clone();

        let receiver_derived_desc = descs[0].derive(coin.derivation_index, secp);

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

    let proposal = proposal
        .contribute_inputs(vec![selected_input])
        .map_err(|e| format!("Failed to contribute inputs: {e:?}"))?
        .commit_inputs()
        .save(persister)?;

    // Extract
    let proposal = proposal.finalize_proposal(
        |psbt: &Psbt| Ok(psbt.clone()),
        Some(FeeRate::BROADCAST_MIN),
        Some(FeeRate::from_sat_per_vb_unchecked(2)),
    );

    // TODO(arturgontijo): Avoiding changing state, for now
    let noop_persister = NoopPersister::default();
    let payjoin_proposal = proposal.save(&noop_persister)?;

    let mut psbt = payjoin_proposal.psbt().clone();

    // TODO(arturgontijo): If we use a previous payjoin utxo it is breaking while broadcasting
    for (index, psbtin) in psbt.inputs.iter_mut().enumerate() {
        let outpoint = &psbt.unsigned_tx.input[index].previous_output;
        if let Some((coin, txin, input)) = candidate_inputs_map.get(outpoint) {
            *psbtin = input.clone();
            psbt.unsigned_tx.input[index] = txin.clone();
            let receiver_derived_desc = descs[0].derive(coin.derivation_index, secp);
            receiver_derived_desc.update_psbt_in(psbtin);
        }
    }

    Ok(psbt)
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
                Ok(PersistedSucccessWithMaybeNoResults::Success(next_state)) => Ok(next_state),
                Ok(PersistedSucccessWithMaybeNoResults::NoResults(_current_state)) => {
                    Err("NoResults".into())
                }
                Err(e) => match e {
                    PersistedError::BadInitInputs(e)
                    | PersistedError::Fatal(e)
                    | PersistedError::Transient(e) => Err(e.into()),
                    PersistedError::Storage(e) => Err(e.into()),
                },
            }
        }
        Err(e) => Err(e.into()),
    }
}

fn process_receiver_session(
    session_id: SessionId,
    session: SessionWrapper<ReceiverSessionEvent>,
    state: ReceiverState,
    persister: ReceiverPersister,
    db_conn: &mut Box<dyn DatabaseConnection>,
    descs: &[descriptors::SinglePathLianaDesc],
    secp: &secp256k1::Secp256k1<secp256k1::VerifyOnly>,
) -> Result<(), Box<dyn Error>> {
    match state {
        ReceiverState::WithContext(receiver) => {
            let bip21 = receiver.pj_uri().to_string();
            log::info!("[Payjoin] ReceiverState::WithContext: {bip21}");
            if session.status == PayjoinStatus::Pending {
                match poll_fallback(receiver, &persister) {
                    Ok(proposal) => {
                        let psbt =
                            handle_directory_proposal(proposal, &persister, db_conn, descs, secp)?;
                        db_conn.store_spend(&psbt);
                        // TODO(arturgontijo): Need to refetch it to get latest events.
                        if let Some(mut session) = db_conn.payjoin_get_receiver_session(&session_id)
                        {
                            session.status = PayjoinStatus::Signing;
                            session.bip21 = Some(bip21);
                            session.txid = Some(psbt.unsigned_tx.compute_txid());
                            session.psbt = Some(psbt);
                            db_conn.update_payjoin_receiver_status(&session_id, session);
                        }
                        log::info!("ReceiverState::WithContext: PSBT in DB...");
                    }
                    Err(_) => {}
                }
            }
            Ok(())
        }
        ReceiverState::UncheckedProposal(_proposal) => {
            log::info!("ReceiverState::UncheckedProposal");
            Ok(())
        }
        ReceiverState::MaybeInputsOwned(_proposal) => {
            log::info!("ReceiverState::MaybeInputsOwned");
            Ok(())
        }
        ReceiverState::MaybeInputsSeen(_proposal) => {
            log::info!("ReceiverState::MaybeInputsSeen");
            Ok(())
        }
        ReceiverState::OutputsUnknown(_proposal) => {
            log::info!("ReceiverState::OutputsUnknown");
            Ok(())
        }
        ReceiverState::WantsOutputs(_proposal) => {
            log::info!("ReceiverState::WantsOutputs");
            Ok(())
        }
        ReceiverState::WantsInputs(_proposal) => {
            log::info!("ReceiverState::WantsInputs");
            Ok(())
        }
        ReceiverState::ProvisionalProposal(_proposal) => {
            log::info!("ReceiverState::ProvisionalProposal");
            Ok(())
        }
        ReceiverState::PayjoinProposal(_proposal) => {
            log::info!("ReceiverState::PayjoinProposal");
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
        log::info!("[Payjoin] {:?}: bip21={:?}", session.status, session.bip21);
        let persister =
            ReceiverPersister::from_id(Arc::new(db.clone()), session_id.clone()).unwrap();
        let (state, _) = replay_receiver_event_log(persister.clone())
            .map_err(|e| format!("Failed to replay receiver event log: {:?}", e))
            .unwrap();
        match session.status {
            PayjoinStatus::Pending => {
                match process_receiver_session(
                    session_id,
                    session,
                    state,
                    persister,
                    &mut db_conn,
                    descs,
                    secp,
                ) {
                    Ok(_) => (),
                    Err(e) => log::warn!("payjoin_receiver_check(): {}", e),
                }
            }
            PayjoinStatus::Signing => {
                if let Some(txid) = session.txid {
                    match db_conn.spend_tx(&txid) {
                        Some(psbt) => {
                            let mut is_signed = false;
                            for psbtin in &psbt.inputs {
                                if !psbtin.partial_sigs.is_empty() {
                                    log::info!("PSBT was signed!");
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
                                                Some(FeeRate::from_sat_per_vb(5).unwrap()),
                                            )
                                            .save(&persister)
                                            .unwrap();

                                        let (req, ctx) = proposal
                                            .extract_req(OHTTP_RELAY)
                                            .expect("Failed to extract request");

                                        // Respond to sender
                                        log::info!("[Payjoin] receiver responding to sender...");
                                        log::info!(
                                            "[Payjoin] DEBUG: post_psbt_proposal(): {}",
                                            req.url
                                        );
                                        match post_request(req.clone()) {
                                            Ok(resp) => {
                                                proposal
                                                    .process_res(resp.bytes().expect("Failed to read response").as_ref(), ctx)
                                                    .save(&persister)
                                                    .unwrap();

                                                // TODO(arturgontijo): Need to refetch it to get latest events.
                                                if let Some(mut session) = db_conn.payjoin_get_receiver_session(&session_id) {
                                                    // Update status of receiver
                                                    session.status = PayjoinStatus::Completed;
                                                    db_conn.update_payjoin_receiver_status(
                                                        &session_id,
                                                        session,
                                                    );
                                                }
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
