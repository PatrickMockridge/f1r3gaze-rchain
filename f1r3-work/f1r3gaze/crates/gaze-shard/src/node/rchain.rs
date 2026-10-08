//! The rchain dialect: rchain-rust's `rnode` HTTP API.
//!
//! The deploy wire is the same as f1r3fly's (see [`crate::deploy`]), but the
//! read surface differs: there is no `/api/registry/{uri}`, no
//! `/api/estimate-cost` and no websocket, the exploratory path takes a bare
//! JSON string and returns `{expr, block, replySource}` with no cost, an
//! unforgeable name must be wrapped in the `ExprUnforg` envelope, and deploy
//! status is a tagged enum.

use super::{Node, enc};
use crate::chain::{self, Blocks};
use crate::deploy::SignedDeploy;
use crate::pos;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// `(block hash, block number)` to anchor reads and deploys.
///
/// The finalized fringe first, and the newest block when there is none: a
/// single-validator `rnode` proposes blocks that never finalize, so
/// `/api/last-finalized-block` answers 400 while `/api/blocks` lists the chain.
/// On a net that does finalize, the fringe is used and this is exact.
pub(super) fn last_finalized(n: &Node) -> Result<(String, i64), String> {
    if let Ok(head) = n.finalized_head() {
        return Ok(head);
    }
    let v = n.call("GET", "/api/blocks", None)?;
    let b = v.as_array().and_then(|a| a.first()).ok_or("no blocks on this node")?;
    let h = b.get("blockHash").and_then(|x| x.as_str()).ok_or("no blockHash")?.to_string();
    let num = b.get("blockNumber").and_then(|x| x.as_i64()).unwrap_or(0);
    Ok((h, num))
}

/// A registry entry, read as data on the public channel named by the URI.
///
/// rchain has no `/api/registry`; a site's manifest is published by sending
/// it to a public name (the same `rho:serve:1:…` string the f1r3fly dialect
/// uses as a registry URI), and read back here. Returns
/// `(data, block hash, block number)`.
pub(super) fn registry(n: &Node, uri: &str, block: Option<&str>) -> Result<(Vec<Value>, String, i64), String> {
    let block = match block {
        Some(b) => b.to_string(),
        None => n.last_finalized()?.0,
    };
    let body = json!({
        "name": { "ExprString": { "data": uri } },
        "blockHash": block,
        "usePreStateHash": false,
    });
    let v = n.call("POST", "/api/data-at-name-by-block-hash", Some(&body))?;
    Ok(project_block(&v))
}

/// An exploratory deploy at `block`'s post-state: `(values on return, block
/// hash, cost)`. rchain returns no cost, so the third element is always 0 and
/// the bridge quotes a configured bound instead.
pub(super) fn explore(n: &Node, term: &str, block: &str) -> Result<(Vec<Value>, String, u64), String> {
    let body = json!({ "term": term, "blockHash": block, "usePreStateHash": false });
    let v = n.call("POST", "/api/explore-deploy-by-block-hash", Some(&body))?;
    let (data, h, _) = project_block(&v);
    Ok((data, h, 0))
}

/// Data at a private unforgeable name, at `block`. The name must carry the
/// `ExprUnforg` envelope, unlike the bare form the f1r3fly node accepts.
pub(super) fn data_at_private(n: &Node, hex: &str, block: &str) -> Result<Vec<Value>, String> {
    let body = json!({
        "name": { "ExprUnforg": { "data": { "UnforgPrivate": { "data": hex } } } },
        "blockHash": block,
        "usePreStateHash": false,
    });
    let v = n.call("POST", "/api/data-at-name-by-block-hash", Some(&body))?;
    Ok(project_block(&v).0)
}

/// Submit; returns the node's message, checking the deploy id it reports.
///
/// `rnode` answers `"Success!\nDeployId is: <base16 signature>"`. The id is the
/// signature, so it must equal [`SignedDeploy::id`]; a mismatch means the node
/// derived a different preimage than we signed.
pub(super) fn deploy(n: &Node, d: &SignedDeploy) -> Result<String, String> {
    let v = n.call("POST", "/api/deploy", Some(&d.to_json()))?;
    let msg = v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
    if let Some((_, rest)) = msg.split_once("DeployId is: ") {
        let id = rest.trim();
        if id != d.id() {
            return Err(format!("node reported deploy id {id}, expected {}", d.id()));
        }
    }
    Ok(msg)
}

/// `GET /api/v1/deploy-status/{sig}` → a tagged enum, projected onto the
/// `(state, latest block)` shape the bridge polls.
///
/// `{"ProcessedWithSuccess": {…, "block": {…}}}` → `Finalized`,
/// `{"ProcessedWithError": {…}}` → `Failed`, and every `NotProcessed` (an
/// unknown or still-pooled signature, answered 200) → `Pending`. The bridge
/// polls until the state is not `Pending`.
pub(super) fn finalization(n: &Node, sig_hex: &str) -> Result<(String, Option<String>), String> {
    let v = n.call("GET", &format!("/api/v1/deploy-status/{sig_hex}"), None)?;
    let (tag, inner) = v
        .as_object()
        .and_then(|o| o.iter().next())
        .ok_or("deploy-status is not a tagged object")?;
    let state = match tag.as_str() {
        "ProcessedWithSuccess" => "Finalized",
        "ProcessedWithError" => "Failed",
        _ => "Pending",
    };
    let hash = inner.get("block").and_then(|b| b.get("blockHash")).and_then(|h| h.as_str()).map(str::to_string);
    Ok((state.to_string(), hash))
}

/// The `{expr, block}` envelope shared by the data-at-name and exploratory
/// responses: `(values, block hash, block number)`.
fn project_block(v: &Value) -> (Vec<Value>, String, i64) {
    let data = v.get("expr").and_then(|d| d.as_array()).cloned().unwrap_or_default();
    let h = v.pointer("/block/blockHash").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let n = v.pointer("/block/blockNumber").and_then(|x| x.as_i64()).unwrap_or(0);
    (data, h, n)
}

/// One typed GET, with the route named in the error so a shape change on the
/// node reads as `"block: missing field \`blockHash\`"` rather than a panic.
fn get<T: DeserializeOwned>(n: &Node, path: &str, what: &str) -> Result<T, String> {
    let v = n.call("GET", path, None)?;
    serde_json::from_value(v).map_err(|e| format!("{what}: {e}"))
}

// --- chain reads -----------------------------------------------------------
// Every caller-supplied path operand goes through `enc`, which escapes `/` --
// which matters, because a block hash may be spelled `blake2b-256:<hex>`.

pub(super) fn block(n: &Node, hash: &str) -> Result<chain::BlockInfo, String> {
    get(n, &format!("/api/block/{}", enc(hash)), "block")
}

/// The newest `spec` blocks. `cap` clamps a page-facing depth; the CLI passes
/// `None` and gets what it asked for.
pub(super) fn blocks(n: &Node, spec: Blocks, cap: Option<i32>) -> Result<Vec<chain::LightBlockInfo>, String> {
    get(n, &spec.path(cap), "blocks")
}

pub(super) fn find_deploy(n: &Node, id: &str) -> Result<chain::LightBlockInfo, String> {
    get(n, &format!("/api/deploy/{}", enc(id)), "find-deploy")
}

pub(super) fn is_finalized(n: &Node, hash: &str) -> Result<bool, String> {
    get(n, &format!("/api/is-finalized/{}", enc(hash)), "is-finalized")
}

pub(super) fn pool(n: &Node) -> Result<Vec<chain::PooledDeploy>, String> {
    let p: chain::PooledDeploys = get(n, "/api/v1/deploys", "pooled deploys")?;
    Ok(p.deploys)
}

pub(super) fn capabilities(n: &Node) -> Result<chain::NodeCapabilities, String> {
    get(n, "/api/v1/capabilities", "capabilities")
}

pub(super) fn shards(n: &Node) -> Result<chain::Shards, String> {
    get(n, "/api/shards", "shards")
}

// --- staking reads ---------------------------------------------------------

pub(super) fn pos_status(n: &Node) -> Result<pos::PosStatus, String> {
    get(n, "/api/v1/pos", "pos status")
}

pub(super) fn pos_delegations(n: &Node, key: &str) -> Result<Vec<pos::DelegatorPosition>, String> {
    get(n, &format!("/api/v1/pos/delegations?delegator={}", enc(key)), "delegations")
}
