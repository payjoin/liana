use crate::{
    bitcoin::payjoin::helpers::post_request,
    database::{sqlite::PayjoinSenderStatus, DatabaseConnection},
};

use std::{convert::TryFrom, str::FromStr};

use payjoin::{
    bitcoin::FeeRate,
    persist::NoopPersister,
    send::v2::{Sender, SenderBuilder},
    Uri, UriExt, Url,
};

use super::helpers::OHTTP_RELAY;

pub fn payjoin_sender_check(db_conn: &mut Box<dyn DatabaseConnection>) {
    let ohttp_url = Url::from_str(OHTTP_RELAY).expect("Invalid OHTTP relay");
    let payjoin_senders = db_conn.get_all_payjoin_senders();
    for (bip21, txid, status, maybe_sender) in payjoin_senders {
        match status {
            PayjoinSenderStatus::Pending => {
                log::info!("[Payjoin] PayjoinSenderStatus: {:?} | {}", status, bip21);

                let psbt = db_conn.spend_tx(&txid).expect("Spend tx not found");

                let pj_uri = Uri::try_from(bip21.as_str())
                    .expect("Invalid BIP21")
                    .assume_checked()
                    .check_pj_supported()
                    .expect("Invalid PJ BIP21");

                let new_sender = SenderBuilder::new(psbt, pj_uri)
                    .build_recommended(FeeRate::BROADCAST_MIN)
                    .expect("Failed to build sender");

                // TODO: should just be able to load a sender from the db, and not use the NoopPersister.
                let storage_token = new_sender
                    .persist(&mut NoopPersister)
                    .expect("Failed to persist sender");

                let sender =
                    Sender::load(storage_token, &NoopPersister).expect("Failed to load sender");

                let (post_req, _) = sender
                    .extract_v2(ohttp_url.clone())
                    .expect("Failed to extract v2");
                // Send original PSBT to the receiver via the BIP77 directory
                match post_request(post_req.clone()) {
                    Ok(_) => {
                        log::info!("[Payjoin] Updating PSBT's STATUS...");
                        db_conn.update_payjoin_sender_status(
                            txid,
                            PayjoinSenderStatus::WaitingReceiver,
                            Some(sender),
                        );
                    }
                    Err(e) => log::warn!("Failed to POST original proposal: {:?}", e),
                }
            }
            PayjoinSenderStatus::WaitingReceiver => {
                log::info!(
                    "[Payjoin] PayjoinSenderStatus: {:?} | sender_is_set={}",
                    status,
                    maybe_sender.is_some()
                );
                if let Some(sender) = maybe_sender {
                    let (post_req, post_ctx) = sender
                        .extract_v2(ohttp_url.clone())
                        .expect("Failed to extract v2");

                    match post_request(post_req.clone()) {
                        Ok(resp) => {
                            let get_ctx = post_ctx
                                .process_response(
                                    resp.bytes().expect("Must be valid response").as_ref(),
                                )
                                .expect("Failed to process response");

                            // Read the response from the receiver via the BIP77 directory
                            let (get_req, ohttp_ctx) = get_ctx
                                .extract_req(OHTTP_RELAY)
                                .expect("Failed to extract get request");

                            let psbt = db_conn.spend_tx(&txid).expect("Spend tx not found");

                            // TODO(arturgontijo): PDK removes these fields but we need them so GUI can properly sign the inputs
                            let mut input_fields_to_restore = vec![];
                            for (index, input) in psbt.inputs.iter().enumerate() {
                                input_fields_to_restore.push((
                                    index,
                                    input.witness_script.clone(),
                                    input.bip32_derivation.clone(),
                                ));
                            }

                            match post_request(get_req.clone()) {
                                Ok(resp) => {
                                    log::info!("Payjoin sender got a final PSBT...");

                                    let mut psbt = match get_ctx.process_response(
                                        resp.bytes().expect("Failed to read response").as_ref(),
                                        ohttp_ctx,
                                    ) {
                                        Ok(Some(psbt)) => psbt,
                                        Ok(None) => {
                                            // nothing to do yet, no response
                                            log::warn!("Nothing to do yet, no response...");
                                            continue;
                                        }
                                        Err(e) => {
                                            log::warn!(
                                                "Failed to process payjoin sender response: {:?}",
                                                e
                                            );
                                            // TODO: handle error
                                            continue;
                                        }
                                    };

                                    // TODO(arturgontijo): Restoring witness_scripts and bip32_bip32_derivation so GUI can sign them
                                    for (index, witness_script, bip32_derivation) in
                                        input_fields_to_restore
                                    {
                                        psbt.inputs[index].witness_script = witness_script;
                                        psbt.inputs[index].bip32_derivation = bip32_derivation;
                                    }

                                    // Store updated Payjoin psbt
                                    log::info!(
                                        "Updated Payjoin psbt: {} -> {}",
                                        txid,
                                        psbt.unsigned_tx.compute_txid()
                                    );
                                    db_conn.store_spend(&psbt);

                                    log::info!("Deleting original Payjoin psbt (txid={})", txid);
                                    db_conn.delete_spend(&txid);

                                    // Mark the sender as completed
                                    db_conn.update_payjoin_sender_status(
                                        txid,
                                        PayjoinSenderStatus::Completed,
                                        None,
                                    );
                                }
                                Err(e) => log::warn!("Failed to get receiver's proposal: {:?}", e),
                            }
                        }
                        Err(e) => log::warn!("Failed to POST original proposal: {:?}", e),
                    }
                }
            }
            _ => {}
        }
    }
}
