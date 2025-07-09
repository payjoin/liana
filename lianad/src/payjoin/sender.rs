use crate::database::DatabaseInterface;

use crate::payjoin::db::SessionMetadata;
use crate::payjoin::helpers::post_request;

use std::collections::HashMap;
use std::error::Error;
use std::sync::{self, Arc};

use payjoin::bitcoin::Psbt;
use payjoin::persist::OptionalTransitionOutcome;
use payjoin::send::v2::{replay_event_log, SendSession, V2GetContext};
use payjoin::send::v2::{Sender, WithReplyKey};

use super::db::SenderPersister;
use super::helpers::OHTTP_RELAY;
use super::types::PayjoinStatus;

fn get_proposed_payjoin_psbt(
    context: Sender<V2GetContext>,
    persister: &SenderPersister,
    // TODO: replace with specific error
) -> Result<Option<Psbt>, Box<dyn Error>> {
    let (req, ctx) = context.extract_req(OHTTP_RELAY)?;
    match post_request(req) {
        Ok(resp) => {
            let res = context
                .process_response(resp.bytes().expect("Failed to read response").as_ref(), ctx)
                .save(persister);
            match res {
                Ok(OptionalTransitionOutcome::Progress(psbt)) => {
                    log::info!("[Payjoin] ProposalReceived!");
                    Ok(Some(psbt))
                }
                Ok(OptionalTransitionOutcome::Stasis(_current_state)) => {
                    log::info!("[Payjoin] No response yet.");
                    Ok(None)
                }
                Err(e) => {
                    log::error!("{:?}", e);
                    Err(format!("Response error: {}", e).into())
                }
            }
        }
        Err(e) => Err(Box::new(e)),
    }
}

fn post_orginal_proposal(
    sender: Sender<WithReplyKey>,
    persister: &SenderPersister,
) -> Result<(), Box<dyn Error>> {
    let (req, ctx) = sender.extract_v2(OHTTP_RELAY)?;
    match post_request(req) {
        Ok(resp) => {
            log::info!("[Payjoin] Posted original proposal...");
            sender
                .process_response(resp.bytes().expect("Failed to read response").as_ref(), ctx)
                .save(persister)?;
            Ok(())
        }
        Err(e) => Err(Box::new(e)),
    }
}

fn process_sender_session(
    state: SendSession,
    persister: &SenderPersister,
) -> Result<Option<Psbt>, Box<dyn Error>> {
    match state {
        SendSession::WithReplyKey(sender) => {
            log::info!("[Payjoin] SenderState::WithReplyKey");
            match post_orginal_proposal(sender, persister) {
                Ok(_) => {}
                Err(err) => log::warn!("post_orginal_proposal(): {}", err),
            }
            Ok(None)
        }
        SendSession::V2GetContext(context) => {
            log::info!("[Payjoin] SenderState::V2GetContext");
            get_proposed_payjoin_psbt(context, persister)
        }
        SendSession::ProposalReceived(psbt) => {
            log::info!(
                "[Payjoin] SenderState::ProposalReceived: {}",
                psbt.to_string()
            );
            Ok(Some(psbt.clone()))
        }
        _ => Err("Unexpected sender state".into()),
    }
}

pub fn payjoin_sender_check(db: &sync::Arc<sync::Mutex<dyn DatabaseInterface>>) {
    let mut db_conn = db.connection();
    for (session_id, session) in db_conn.payjoin_get_all_sender_sessions() {
        let SessionMetadata {
            status,
            maybe_txid,
            maybe_psbt,
            maybe_bip21,
        } = session.metadata.clone();

        // No need to check Completed
        if status == PayjoinStatus::Completed {
            continue;
        }

        log::info!("[Payjoin] {:?}: bip21={:?}", status, maybe_bip21);

        let persister = SenderPersister::from_id(Arc::new(db.clone()), session_id.clone());

        let (state, _) = replay_event_log(&persister)
            .map_err(|e| format!("Failed to replay sender event log: {:?}", e))
            .unwrap();

        match status {
            PayjoinStatus::Pending => match process_sender_session(state, &persister) {
                Ok(_) => persister.update_metadata(
                    Some(PayjoinStatus::WaitingReceiver),
                    maybe_txid,
                    maybe_psbt,
                    maybe_bip21,
                ),
                Err(e) => log::warn!("process_sender_session(): {}", e),
            },
            PayjoinStatus::WaitingReceiver => match process_sender_session(state, &persister) {
                Ok(maybe_psbt) => {
                    if let Some(mut new_psbt) = maybe_psbt {
                        if let Some(txid) = maybe_txid {
                            if let Some(psbt) = db_conn.spend_tx(&txid) {
                                // TODO(arturgontijo): PDK removes fields that we need in the GUI to properly sign the inputs
                                let mut input_fields_to_restore = HashMap::new();
                                for (index, txin) in psbt.unsigned_tx.input.iter().enumerate() {
                                    let mut input_without_sigs = psbt.inputs[index].clone();
                                    input_without_sigs.partial_sigs = Default::default();
                                    input_fields_to_restore
                                        .insert(txin.previous_output, input_without_sigs);
                                }
                                log::info!(
                                    "[Payjoin] Deleting original Payjoin psbt (txid={txid})"
                                );
                                db_conn.delete_spend(&txid);

                                // TODO(arturgontijo): Restoring witness_scripts and bip32_derivation so GUI can sign them
                                for (index, psbtin) in new_psbt.inputs.iter_mut().enumerate() {
                                    let outpoint =
                                        &new_psbt.unsigned_tx.input[index].previous_output;
                                    if let Some(input) = input_fields_to_restore.get(outpoint) {
                                        *psbtin = input.clone();
                                    }
                                }

                                let new_txid = new_psbt.unsigned_tx.compute_txid();
                                log::info!(
                                    "[Payjoin] Updating Payjoin psbt: {} -> {}",
                                    txid,
                                    new_txid,
                                );
                                db_conn.store_spend(&new_psbt);

                                persister.update_metadata(
                                    Some(PayjoinStatus::Completed),
                                    Some(new_txid),
                                    Some(new_psbt),
                                    maybe_bip21,
                                );
                            }
                        }
                    }
                }
                Err(e) => log::warn!("payjoin_sender_check(): {}", e),
            },
            _ => {}
        }
    }
}
