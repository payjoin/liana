use crate::database::DatabaseInterface;

use crate::payjoin::helpers::post_request;

use std::error::Error;
use std::sync::{self, Arc};

use payjoin::bitcoin::Psbt;
use payjoin::persist::PersistedSucccessWithMaybeNoResults;
use payjoin::send::v2::Sender;
use payjoin::send::v2::{replay_sender_event_log, SenderState, SenderWithReplyKey, V2GetContext};

use super::db::SenderPersister;
use super::helpers::OHTTP_RELAY;
use super::types::PayjoinStatus;

fn get_proposed_payjoin_psbt(
    context: Sender<V2GetContext>,
    persister: &SenderPersister,
) -> Result<Option<Psbt>, Box<dyn Error>> {
    let (req, ctx) = context.extract_req(OHTTP_RELAY)?;
    log::info!("[Payjoin] DEBUG: get_proposed_payjoin_psbt(): {}", req.url);
    match post_request(req) {
        Ok(resp) => {
            let res = context
                .process_response(
                    &resp.bytes().expect("Failed to read response").as_ref(),
                    ctx,
                )
                .save(persister);
            match res {
                Ok(PersistedSucccessWithMaybeNoResults::Success(proposal)) => {
                    let psbt = proposal.psbt();
                    log::info!("[Payjoin] Proposal received. PSBT: {}", psbt.to_string());
                    Ok(Some(psbt.clone()))
                }
                Ok(PersistedSucccessWithMaybeNoResults::NoResults(_current_state)) => {
                    log::info!("[Payjoin] No response yet.");
                    // context = current_state;
                    Ok(None)
                }
                Err(e) => {
                    log::error!("{:?}", e);
                    Err(format!("Response error: {}", e).into())
                }
            }
        }
        Err(e) => Err(e.into()),
    }
}

fn post_orginal_proposal(
    sender: Sender<SenderWithReplyKey>,
    persister: &SenderPersister,
) -> Result<(), Box<dyn Error>> {
    let (req, ctx) = sender.extract_v2(OHTTP_RELAY)?;
    log::info!("[Payjoin] DEBUG: post_orginal_proposal(): {}", req.url);
    match post_request(req) {
        Ok(resp) => {
            log::info!("[Payjoin] Posted original proposal...");
            sender
                .process_response(
                    &resp.bytes().expect("Failed to read response").as_ref(),
                    ctx,
                )
                .save(persister)?;
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

fn process_sender_session(
    state: SenderState,
    persister: &SenderPersister,
) -> Result<Option<Psbt>, Box<dyn Error>> {
    match state {
        SenderState::WithReplyKey(sender) => {
            log::info!("[Payjoin] SenderState::WithReplyKe");
            match post_orginal_proposal(sender, persister) {
                Ok(_) => {}
                Err(err) => log::warn!("post_orginal_proposal(): {}", err),
            }
            return Ok(None);
        }
        SenderState::V2GetContext(context) => {
            log::info!("[Payjoin] SenderState::V2GetContext");
            get_proposed_payjoin_psbt(context, persister)
        }
        SenderState::ProposalReceived(proposal) => {
            log::info!(
                "[Payjoin] SenderState::ProposalReceived: {}",
                proposal.psbt().to_string()
            );
            return Ok(None);
        }
        _ => return Err(format!("Unexpected sender state").into()),
    }
}

pub fn payjoin_sender_check(db: &sync::Arc<sync::Mutex<dyn DatabaseInterface>>) {
    let mut db_conn = db.connection();
    for (session_id, session) in db_conn.payjoin_get_all_sender_sessions() {
        log::info!("[Payjoin] {:?}: bip21={:?}", session.status, session.bip21);
        let persister = SenderPersister::from_id(Arc::new(db.clone()), session_id.clone()).unwrap();
        let (state, _) = replay_sender_event_log(persister.clone())
            .map_err(|e| format!("Failed to replay sender event log: {:?}", e))
            .unwrap();
        match session.status {
            PayjoinStatus::Pending => match process_sender_session(state, &persister) {
                Ok(_) => {
                    if let Some(mut session) = db_conn.payjoin_get_sender_session(&session_id) {
                        session.status = PayjoinStatus::WaitingReceiver;
                        db_conn.update_payjoin_sender_status(&session_id, session);
                    }
                }
                Err(e) => log::warn!("process_sender_session(): {}", e),
            },
            PayjoinStatus::WaitingReceiver => match process_sender_session(state, &persister) {
                Ok(maybe_psbt) => {
                    if let Some(new_psbt) = maybe_psbt {
                        if let Some(txid) = session.txid {
                            if let Some(mut session) =
                                db_conn.payjoin_get_sender_session(&session_id)
                            {
                                log::info!("Deleting original Payjoin psbt (txid={txid})");
                                db_conn.delete_spend(&txid);

                                let new_txid = new_psbt.unsigned_tx.compute_txid();
                                log::info!("Updating Payjoin psbt: {} -> {}", txid, new_txid,);
                                db_conn.store_spend(&new_psbt);

                                session.txid = Some(new_txid);
                                session.psbt = Some(new_psbt);
                                session.status = PayjoinStatus::Completed;
                                db_conn.update_payjoin_sender_status(&session_id, session);
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
