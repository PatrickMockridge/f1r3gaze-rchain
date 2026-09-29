//! A client for one node's HTTP API (`f1r3node-rust` `node/src/rust/web`).

use crate::deploy::SignedDeploy;
use gaze_net::{Http, HttpRequest};
use serde_json::{Value, json};

#[derive(Clone)]
pub struct Node {
    pub base: String,
    http: Http,
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b':' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

impl Node {
    pub fn new(base: &str, http: Http) -> Node {
        Node {
            base: base.trim_end_matches('/').to_string(),
            http,
        }
    }

    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let r = self
            .http
            .send(&HttpRequest {
                url: format!("{}{}", self.base, path),
                method: method.into(),
                headers: vec![("content-type".into(), "application/json".into()), ("accept".into(), "application/json".into())],
                body: body.map(|b| b.to_string().into_bytes()).unwrap_or_default(),
            })
            .map_err(|e| format!("{}: {e}", self.base))?;
        let v: Value = serde_json::from_slice(&r.body).unwrap_or(Value::String(String::from_utf8_lossy(&r.body).into()));
        if (200..300).contains(&r.status) {
            Ok(v)
        } else {
            let msg = v.get("message").or_else(|| v.get("error")).map(|m| m.to_string()).unwrap_or_else(|| v.to_string());
            Err(format!("{} {path}: HTTP {}: {msg}", self.base, r.status))
        }
    }

    /// `(block hash, block number)` of the last finalized block.
    pub fn last_finalized(&self) -> Result<(String, i64), String> {
        let v = self.call("GET", "/api/last-finalized-block", None)?;
        let bi = v.get("blockInfo").unwrap_or(&v);
        let h = bi.get("blockHash").and_then(|x| x.as_str()).ok_or("no blockHash")?.to_string();
        let n = bi.get("blockNumber").and_then(|x| x.as_i64()).unwrap_or(0);
        Ok((h, n))
    }

    /// Registry entry at `uri`, at `block` (default: last finalized):
    /// `(data, block hash, block number)`.
    pub fn registry(&self, uri: &str, block: Option<&str>) -> Result<(Vec<Value>, String, i64), String> {
        let q = block.map(|b| format!("?block_hash={}", enc(b))).unwrap_or_default();
        let v = self.call("GET", &format!("/api/registry/{}{q}", enc(uri)), None)?;
        let data = v.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default();
        let h = v.get("blockHash").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let n = v.get("blockNumber").and_then(|x| x.as_i64()).unwrap_or(0);
        Ok((data, h, n))
    }

    /// Exploratory deploy against the last finalized post-state:
    /// `(values on return, block hash, cost)`.
    pub fn explore(&self, term: &str) -> Result<(Vec<Value>, String, u64), String> {
        let v = self.call("POST", "/api/explore-deploy", Some(&json!({ "term": term })))?;
        let data = v.get("expr").and_then(|d| d.as_array()).cloned().unwrap_or_default();
        let h = v.pointer("/block/blockHash").and_then(|x| x.as_str()).unwrap_or("").to_string();
        Ok((data, h, v.get("cost").and_then(|c| c.as_u64()).unwrap_or(0)))
    }

    /// Data at a private unforgeable name, at `block`.
    pub fn data_at_private(&self, hex: &str, block: &str) -> Result<Vec<Value>, String> {
        let body = json!({ "name": { "UnforgPrivate": { "data": hex } }, "blockHash": block, "usePreStateHash": false });
        let v = self.call("POST", "/api/data-at-name-by-block-hash", Some(&body))?;
        Ok(v.get("expr").and_then(|d| d.as_array()).cloned().unwrap_or_default())
    }

    pub fn estimate_cost(&self, term: &str, deployer_hex: &str) -> Result<u64, String> {
        let v = self.call("POST", "/api/estimate-cost", Some(&json!({ "term": term, "deployer": deployer_hex })))?;
        v.get("cost").and_then(|c| c.as_u64()).ok_or_else(|| "no cost in estimate".into())
    }

    /// Submit; returns the node's message.
    pub fn deploy(&self, d: &SignedDeploy) -> Result<String, String> {
        let v = self.call("POST", "/api/deploy", Some(&d.to_json()))?;
        Ok(v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))
    }

    /// `(state, latest block)`; state is Finalized, Failed, Pending or Expired.
    pub fn finalization(&self, sig_hex: &str) -> Result<(String, Option<String>), String> {
        let v = self.call("GET", &format!("/api/deploy-finalization-status/{sig_hex}"), None)?;
        let st = v.get("state").and_then(|s| s.as_str()).unwrap_or("Pending").to_string();
        Ok((st, v.get("latest_block_hash").and_then(|s| s.as_str()).map(str::to_string)))
    }

    /// The event stream's URL.
    pub fn events_url(&self) -> String {
        let b = if let Some(r) = self.base.strip_prefix("https://") {
            format!("wss://{r}")
        } else if let Some(r) = self.base.strip_prefix("http://") {
            format!("ws://{r}")
        } else {
            self.base.clone()
        };
        format!("{b}/ws/events")
    }
}
