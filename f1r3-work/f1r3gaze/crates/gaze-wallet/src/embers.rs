//! The Embers wallet API (the same service F1R3Sky uses): balance and
//! history, and transfers by prepare → check → sign → send.

use crate::address::Address;
use crate::contract::{Limits, check_transfer};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use gaze_net::{Http, HttpRequest};
use gaze_shard::deploy::{public_key, sign_bytes};
use k256::ecdsa::SigningKey;
use serde_json::{Value, json};

/// One transfer as Embers reports it. `Serialize` is for `wallet history
/// --json`; the field names are Embers', and differ from the rchain dialect's
/// rows, which come from the node rather than an index.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Transfer {
    pub id: String,
    pub timestamp: String,
    pub from: String,
    pub to: String,
    pub amount: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WalletState {
    pub balance: u64,
    pub transfers: Vec<Transfer>,
}

#[derive(Clone)]
pub struct Embers {
    pub base: String,
    http: Http,
    pub limits: Limits,
}

impl Embers {
    pub fn new(base: &str, http: Http, limits: Limits) -> Embers {
        Embers {
            base: base.trim_end_matches('/').to_string(),
            http,
            limits,
        }
    }

    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let r = self
            .http
            .send(&HttpRequest {
                url: format!("{}/api{path}", self.base),
                method: method.into(),
                headers: vec![("content-type".into(), "application/json".into()), ("accept".into(), "application/json".into())],
                body: body.map(|b| b.to_string().into_bytes()).unwrap_or_default(),
            })
            .map_err(|e| format!("Embers: {e}"))?;
        let v: Value = serde_json::from_slice(&r.body).unwrap_or(Value::String(String::from_utf8_lossy(&r.body).into()));
        if (200..300).contains(&r.status) {
            Ok(v)
        } else {
            Err(format!("Embers {path}: HTTP {}: {}", r.status, v))
        }
    }

    pub fn state(&self, a: &Address) -> Result<WalletState, String> {
        let v = self.call("GET", &format!("/wallets/{a}/state"), None)?;
        let s = |x: &Value, k: &str| x.get(k).and_then(|y| y.as_str()).unwrap_or("").to_string();
        Ok(WalletState {
            balance: s(&v, "balance").parse().map_err(|_| "Embers: bad balance")?,
            transfers: v
                .get("transfers")
                .and_then(|t| t.as_array())
                .map(|a| {
                    a.iter()
                        .map(|t| Transfer {
                            id: s(t, "id"),
                            timestamp: s(t, "timestamp"),
                            from: s(t, "from"),
                            to: s(t, "to"),
                            amount: s(t, "amount"),
                            description: t.get("description").and_then(|d| d.as_str()).map(str::to_string),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    /// Transfer `amount` from the key's account to `to`. The prepared
    /// contract is checked before it is signed; nothing is sent otherwise.
    /// Returns the deploy id.
    pub fn transfer(&self, key: &SigningKey, to: &Address, amount: i64, description: Option<&str>) -> Result<String, String> {
        if amount <= 0 {
            return Err("the amount must be positive".into());
        }
        let from = Address::from_key(key.verifying_key());
        if &from == to {
            return Err("destination and source are the same wallet".into());
        }
        let prepare_request = json!({
            "from": from.as_str(),
            "to": to.as_str(),
            "amount": amount.to_string(),
            "description": description,
        });
        let prep = self.call("POST", "/wallets/transfer/prepare", Some(&prepare_request))?;
        let response = prep.get("response").cloned().ok_or("Embers: no response in prepare")?;
        let token = prep.get("token").and_then(|t| t.as_str()).ok_or("Embers: no token in prepare")?.to_string();
        let contract_b64 = response.get("contract").and_then(|c| c.as_str()).ok_or("Embers: no contract")?;
        let contract = B64.decode(contract_b64).map_err(|_| "Embers: contract is not base64")?;
        check_transfer(&contract, &from, to, amount, description, &self.limits)?;
        let sig = sign_bytes(key, &contract)?;
        let body = json!({
            "prepare_request": prepare_request,
            "prepare_response": response,
            "request": {
                "contract": contract_b64,
                "sig": B64.encode(&sig),
                "sig_algorithm": "secp256k1",
                "deployer": B64.encode(public_key(key)),
            },
            "token": token,
        });
        let sent = self.call("POST", "/wallets/transfer/send", Some(&body))?;
        Ok(sent
            .get("deploy_id")
            .and_then(|d| d.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| gaze_net::hex(&sig)))
    }
}
