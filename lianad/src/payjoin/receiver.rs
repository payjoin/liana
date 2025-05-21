use std::{collections::HashMap, str::FromStr};

use liana::descriptors;

use payjoin::{
    bitcoin::{psbt::Input, secp256k1, FeeRate, OutPoint, Psbt, Sequence, TxIn},
    receive::{v2::UncheckedProposal, InputPair},
    Url,
};

use crate::{
    database::{Coin, CoinStatus, DatabaseConnection},
    payjoin::helpers::{finalize_psbt, post_request, proposal_dummy_checks_bypass, OHTTP_RELAY},
};

use super::types::PayjoinReceiverStatus;

pub fn process_proposal_psbt(
    proposal: &UncheckedProposal,
    db_conn: &mut Box<dyn DatabaseConnection>,
    descs: &[descriptors::SinglePathLianaDesc],
    secp: &secp256k1::Secp256k1<secp256k1::VerifyOnly>,
) -> Result<Psbt, ()> {
    // descs must always have 2 descriptors
    assert_eq!(descs.len(), 2);
    let coins = db_conn.coins(&[CoinStatus::Confirmed], &[]);

    let proposal = proposal_dummy_checks_bypass(&proposal);
    let proposal = proposal.commit_outputs();

    let mut candidate_inputs_map = HashMap::<OutPoint, (Coin, TxIn, Input)>::new();
    for (outpoint, coin) in coins.iter() {
        let txs = db_conn.list_wallet_transactions(&[outpoint.txid]);
        let (db_tx, _, _) = txs.first().unwrap();

        let tx = db_tx.clone();

        let txout = tx.tx_out(outpoint.vout as usize).unwrap().clone();

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

    let selected_input = proposal.try_preserving_privacy(candidate_inputs).unwrap();

    let proposal = proposal.contribute_inputs(vec![selected_input]).unwrap();
    let proposal = proposal.commit_inputs();

    let proposal = proposal
        .finalize_proposal(
            |psbt| Ok(psbt.clone()),
            None,
            Some(FeeRate::from_sat_per_vb(150).unwrap()),
        )
        .expect("Failed to finalize proposal");

    let mut psbt = proposal.psbt().clone();

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

pub fn payjoin_receiver_check(
    db_conn: &mut Box<dyn DatabaseConnection>,
    descs: &[descriptors::SinglePathLianaDesc],
    secp: &secp256k1::Secp256k1<secp256k1::VerifyOnly>,
) {
    let mut payjoin_receivers = db_conn.get_all_payjoin_receivers();
    for (address, _, status, receiver, psbt) in payjoin_receivers.iter_mut() {
        match status {
            PayjoinReceiverStatus::Pending => {
                log::info!("[Payjoin] {:?}: {}", status, receiver.pj_uri());
                let (req, ctx) = receiver
                    .extract_req(OHTTP_RELAY)
                    .expect("Failed to extract request");
                match post_request(req.clone()) {
                    Ok(resp) => {
                        let proposal = receiver
                            .process_res(
                                resp.bytes().expect("Failed to read response").as_ref(),
                                ctx,
                            )
                            .expect("Failed to process response");
                        if let Some(proposal) = proposal {
                            log::info!("[Payjoin] receiver got a proposal...");

                            let new_psbt =
                                process_proposal_psbt(&proposal, db_conn, descs, secp).unwrap();

                            // DB logic
                            log::info!("[Payjoin] Inserting PSBT and STATUS...");
                            db_conn.update_payjoin_receiver_status(
                                &address,
                                new_psbt.unsigned_tx.compute_txid(),
                                PayjoinReceiverStatus::Signing,
                                new_psbt.to_string(),
                            );
                            db_conn.store_spend(&new_psbt);

                            log::info!("[Payjoin] Payjoin PSBT in DB!");
                        } else {
                            log::info!("empty proposal")
                        }
                    }
                    Err(err) => {
                        log::warn!("[Payjoin] payjoin_receiver_check(): {} -> {}", req.url, err)
                    }
                }
            }
            PayjoinReceiverStatus::Signing => {
                log::info!("[Payjoin] {:?}: {}", status, receiver.pj_uri());
                let psbt = match Psbt::from_str(psbt) {
                    Ok(psbt) => psbt,
                    Err(err) => {
                        log::error!(
                            "[Payjoin] payjoin_receiver_check(psbt_from): len={} -> {}",
                            psbt.len(),
                            err
                        );
                        continue;
                    }
                };
                let txid = psbt.unsigned_tx.compute_txid();
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
                            let (req, ctx) = receiver
                                .extract_req(
                                    Url::from_str(OHTTP_RELAY).expect("Invalid OHTTP relay"),
                                )
                                .expect("Failed to extract request");
                            match post_request(req.clone()) {
                                Ok(resp) => {
                                    let proposal = receiver
                                        .process_res(
                                            resp.bytes().expect("Failed to read response").as_ref(),
                                            ctx,
                                        )
                                        .expect("Failed to process response");
                                    if let Some(proposal) = proposal {
                                        log::info!("[Payjoin] receiver got a proposal...");

                                        let mut psbt = psbt.clone();
                                        finalize_psbt(&mut psbt, secp);

                                        let proposal = proposal_dummy_checks_bypass(&proposal);
                                        let proposal = proposal.commit_outputs();
                                        let proposal = proposal.commit_inputs();

                                        let mut proposal = proposal
                                            .finalize_proposal(
                                                |_| Ok(psbt.clone()),
                                                None,
                                                Some(FeeRate::from_sat_per_vb(150).unwrap()),
                                            )
                                            .expect("Failed to finalize proposal");

                                        let (req, ctx) = proposal
                                            .extract_req(
                                                Url::from_str(OHTTP_RELAY)
                                                    .expect("Invalid OHTTP relay"),
                                            )
                                            .expect("Failed to extract request");

                                        // Respond to sender
                                        log::info!("[Payjoin] receiver responding to sender...");
                                        match post_request(req.clone()) {
                                            Ok(resp) => {
                                                let _proposal = receiver
                                                    .process_res(resp.bytes().expect("Failed to read response").as_ref(), ctx)
                                                    .expect("Failed to process response");
                                                // Update status of receiver
                                                db_conn.update_payjoin_receiver_status(
                                                    &address,
                                                    psbt.unsigned_tx.compute_txid(),
                                                    PayjoinReceiverStatus::Completed,
                                                    psbt.to_string(),
                                                );
                                            },
                                            Err(err) => log::error!(
                                                "[Payjoin] payjoin_receiver_check(respond_to_sender): {} -> {}",
                                                req.url, err
                                            ),
                                        }
                                    } else {
                                        log::info!("empty proposal")
                                    }
                                }
                                Err(err) => log::warn!(
                                    "[Payjoin] payjoin_receiver_check(Signed): {} -> {}",
                                    req.url,
                                    err
                                ),
                            }
                        }
                    }
                    None => {}
                }
            }
            _ => {}
        }
    }
}
