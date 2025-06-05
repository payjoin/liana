use std::{error::Error, time::Duration};

use miniscript::{
    bitcoin::{secp256k1, Psbt, ScriptBuf, TxOut},
    psbt::PsbtExt,
};

use payjoin::{bitcoin::Amount, IntoUrl, OhttpKeys};
use reqwest::{header::ACCEPT, Proxy};

pub const OHTTP_RELAY: &str = "https://pj.bobspacebkk.com";

pub fn http_agent() -> reqwest::blocking::Client {
    reqwest::blocking::Client::new()
}

pub fn fetch_ohttp_keys(
    ohttp_relay: impl IntoUrl,
    payjoin_directory: impl IntoUrl,
) -> Result<OhttpKeys, Box<dyn Error + Send + Sync>> {
    let ohttp_keys_url = payjoin_directory
        .into_url()?
        .join("/.well-known/ohttp-gateway")?;
    let proxy = Proxy::all(ohttp_relay.into_url()?.as_str())?;
    let client = reqwest::blocking::Client::builder().proxy(proxy).build()?;
    let res = client
        .get(ohttp_keys_url)
        .header(ACCEPT, "application/ohttp-keys")
        .send()?;
    parse_ohttp_keys_response(res)
}

fn parse_ohttp_keys_response(
    res: reqwest::blocking::Response,
) -> Result<OhttpKeys, Box<dyn Error + Send + Sync>> {
    if !res.status().is_success() {
        return Err(format!("UnexpectedStatusCode: {}", res.status()).into());
    }

    let body = res.bytes().unwrap().to_vec();
    match OhttpKeys::decode(&body) {
        Ok(ohttp_keys) => Ok(ohttp_keys),
        Err(err) => Err(format!("InvalidOhttpKeys: {}", err).into()),
    }
}

pub fn post_request(req: payjoin::Request) -> Result<reqwest::blocking::Response, Box<dyn Error>> {
    let http = http_agent();
    match http
        .post(req.url)
        .header("Content-Type", req.content_type)
        .body(req.body)
        .timeout(Duration::from_secs(10))
        .send()
    {
        Ok(r) => Ok(r),
        Err(err) => Err(format!("Failed to post_reques(): {}", err).into()),
    }
}

pub fn finalize_psbt(psbt: &mut Psbt, secp: &secp256k1::Secp256k1<secp256k1::VerifyOnly>) {
    let mut witness_utxo_to_clean = vec![];
    let mut inputs_to_finalize = vec![];
    for (index, input) in psbt.inputs.iter_mut().enumerate() {
        if input.witness_utxo.is_none() {
            // Sender's wallet cleans this up (from original PSBT) but we need it to finalize_inp_mut() bellow
            input.witness_utxo = Some(TxOut {
                value: Amount::ZERO,
                script_pubkey: ScriptBuf::default(),
            });
            witness_utxo_to_clean.push(index);
            continue;
        }
        if input.final_script_sig.is_some()
            || input.final_script_witness.is_some()
            || input.partial_sigs.is_empty()
        {
            continue;
        }
        inputs_to_finalize.push(index);
    }

    for index in inputs_to_finalize {
        match psbt.finalize_inp_mut(secp, index) {
            Ok(_) => log::info!("[Payjoin] Finalizing input at: {}", index),
            Err(e) => log::warn!("[Payjoin] Failed to finalize input at: {} | {}", index, e),
        }
    }

    for index in witness_utxo_to_clean {
        psbt.inputs[index].witness_utxo = None;
    }
}
