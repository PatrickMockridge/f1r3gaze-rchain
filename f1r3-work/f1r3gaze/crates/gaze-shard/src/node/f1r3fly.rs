//! The F1R3FLY dialect: F1R3FLY's `f1r3node-rust` HTTP API and event stream.
//!
//! Everything f1r3fly-only lives here — `/api/registry/{uri}`,
//! `/api/estimate-cost`, `/api/deploy-finalization-status/{sig}`, the
//! `/ws/events` stream, the `{"term": …}` explore body, the bare
//! `UnforgPrivate` name, and the `{state, latest_block_hash}` deploy status.
//! None of it may leak into [`super::rchain`].

use super::{Node, enc};
use crate::deploy::SignedDeploy;
use serde_json::{Value, json};

/// `GET /api/registry/{uri}?block_hash=…`:
/// `(data, block hash, block number)`.
pub(super) fn registry(n: &Node, uri: &str, block: Option<&str>) -> Result<(Vec<Value>, String, i64), String> {
    let q = block.map(|b| format!("?block_hash={}", enc(b))).unwrap_or_default();
    let v = n.call("GET", &format!("/api/registry/{}{q}", enc(uri)), None)?;
    let data = v.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default();
    let h = v.get("blockHash").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let n = v.get("blockNumber").and_then(|x| x.as_i64()).unwrap_or(0);
    Ok((data, h, n))
}

/// `POST /api/explore-deploy` against the last finalized post-state:
/// `(values on return, block hash, cost)`. The `block` the bridge pins is
/// ignored here; this node reads at its own last-finalized state.
pub(super) fn explore(n: &Node, term: &str) -> Result<(Vec<Value>, String, u64), String> {
    let v = n.call("POST", "/api/explore-deploy", Some(&json!({ "term": term })))?;
    let data = v.get("expr").and_then(|d| d.as_array()).cloned().unwrap_or_default();
    let h = v.pointer("/block/blockHash").and_then(|x| x.as_str()).unwrap_or("").to_string();
    Ok((data, h, v.get("cost").and_then(|c| c.as_u64()).unwrap_or(0)))
}

/// Data at a private unforgeable name, at `block`.
pub(super) fn data_at_private(n: &Node, hex: &str, block: &str) -> Result<Vec<Value>, String> {
    let body = json!({ "name": { "UnforgPrivate": { "data": hex } }, "blockHash": block, "usePreStateHash": false });
    let v = n.call("POST", "/api/data-at-name-by-block-hash", Some(&body))?;
    Ok(v.get("expr").and_then(|d| d.as_array()).cloned().unwrap_or_default())
}

pub(super) fn estimate_cost(n: &Node, term: &str, deployer_hex: &str) -> Result<u64, String> {
    let v = n.call("POST", "/api/estimate-cost", Some(&json!({ "term": term, "deployer": deployer_hex })))?;
    v.get("cost").and_then(|c| c.as_u64()).ok_or_else(|| "no cost in estimate".into())
}

/// Submit; returns the node's message.
pub(super) fn deploy(n: &Node, d: &SignedDeploy) -> Result<String, String> {
    let v = n.call("POST", "/api/deploy", Some(&d.to_json()))?;
    Ok(v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))
}

/// `POST /api/deploy-finalization-status/{sig}` → `{state, latest_block_hash}`.
pub(super) fn finalization(n: &Node, sig_hex: &str) -> Result<(String, Option<String>), String> {
    let v = n.call("GET", &format!("/api/deploy-finalization-status/{sig_hex}"), None)?;
    let st = v.get("state").and_then(|s| s.as_str()).unwrap_or("Pending").to_string();
    Ok((st, v.get("latest_block_hash").and_then(|s| s.as_str()).map(str::to_string)))
}

/// The event stream's URL.
pub(super) fn events_url(n: &Node) -> String {
    let b = if let Some(r) = n.base.strip_prefix("https://") {
        format!("wss://{r}")
    } else if let Some(r) = n.base.strip_prefix("http://") {
        format!("ws://{r}")
    } else {
        n.base.clone()
    };
    format!("{b}/ws/events")
}
