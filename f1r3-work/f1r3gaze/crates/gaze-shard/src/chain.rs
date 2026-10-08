//! Chain reads on the rchain dialect: blocks, deploys, finality, the mempool,
//! the node's capabilities and its shards.
//!
//! These are ordinary camelCase DTOs, not `RhoExpr` envelopes, so they go
//! through [`crate::expr::json_to_norm`] — except where a field is *known* to
//! be a byte array (a validator key), which is decoded here rather than left
//! as a hex string.
//!
//! The `*_to_norm` conversions are **page-facing** and clip a deploy's `term`,
//! which is unbounded: a `chain!("pool", ret)` reply crossing into a page frame
//! must not be able to carry megabytes. The CLI prints the typed structs
//! instead and shows the whole term.

use crate::expr::{json_to_norm, key_field};
use k1ndl1ng_norm::Norm;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The most blocks a page may ask for in one `blocks` read.
pub const PAGE_MAX_DEPTH: i32 = 20;

/// How much of a deploy's `term` a page receives. Longer terms are clipped and
/// marked, so a page can tell that it happened.
pub const PAGE_TERM_CHARS: usize = 200;

/// Which blocks a read wants. Explicit rather than a depth-or-range pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blocks {
    /// The head only (`GET /api/blocks`).
    Head,
    /// The newest `n` (`GET /api/blocks/{depth}`).
    Depth(i32),
    /// An inclusive height range (`GET /api/blocks/{start}/{end}`).
    Range(i64, i64),
}

impl Blocks {
    /// The path this read addresses, with the page-facing depth cap applied.
    pub fn path(self, cap: Option<i32>) -> String {
        match self {
            Blocks::Head => "/api/blocks".into(),
            Blocks::Depth(n) => {
                let n = match cap {
                    Some(c) => n.min(c),
                    None => n,
                };
                format!("/api/blocks/{n}")
            }
            Blocks::Range(a, b) => format!("/api/blocks/{a}/{b}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BondInfo {
    pub validator: String,
    pub stake: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LightBlockInfo {
    pub version: i32,
    pub shard_id: String,
    pub block_hash: String,
    pub block_number: i64,
    pub sender: String,
    pub seq_num: i64,
    pub pre_state_hash: String,
    pub post_state_hash: String,
    pub justifications: Vec<String>,
    pub bonds: Vec<BondInfo>,
    pub sig_algorithm: String,
    pub sig: String,
    pub block_size: String,
    pub deploy_count: i32,
    pub rejected_deploys: Vec<String>,
    pub timestamp: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeployInfo {
    pub deployer: String,
    pub term: String,
    pub timestamp: i64,
    pub sig: String,
    pub sig_algorithm: String,
    pub phlo_price: i64,
    pub phlo_limit: i64,
    pub valid_after_block_number: i64,
    pub cost: u64,
    pub errored: bool,
    pub system_deploy_error: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockInfo {
    pub block_info: LightBlockInfo,
    pub deploys: Vec<DeployInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PooledDeploy {
    pub deploy_id: String,
    pub timestamp: i64,
    pub deployer: String,
    pub term: String,
    pub phlo_price: i64,
    pub phlo_limit: i64,
    pub valid_after_block_number: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PooledDeploys {
    pub deploys: Vec<PooledDeploy>,
}

/// `GET /api/v1/capabilities` — what this node will do, which is how a client
/// decides whether a feature (the faucet, an admin propose) exists without
/// hardcoding a devnet flag.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeCapabilities {
    pub autopropose: bool,
    pub propose_on_deploy: bool,
    pub manual_propose: bool,
    pub admin_http: bool,
    pub dev_mode: bool,
    pub faucet: bool,
}

/// A deploy's outcome, as the node reports it.
///
/// Distinct from the bridge's `(state, block)` projection, because a **write's
/// answer is a value, not a status**: `rho:rchain:pos` replies
/// `(Bool, Nil | String)` on the deploy's own id channel, and a refusal arrives
/// there while the deploy itself succeeds. Reading the status alone reports
/// success for a bond the node refused, so the answer has to come with it.
#[derive(Clone, Debug, PartialEq)]
pub struct DeployOutcome {
    /// `Finalized`, `Failed` or `Pending`.
    pub state: String,
    pub block: Option<String>,
    /// What the term sent to its `rho:rchain:deployId`.
    pub result: Vec<Value>,
    /// `ProcessedWithError`'s text — a deploy that *failed*, as opposed to a
    /// deploy that succeeded and whose program refused.
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShardInfo {
    pub shard_id: String,
    pub primary: bool,
    pub latest_block_number: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Shards {
    pub primary_shard: String,
    pub shard_count: usize,
    pub shards: Vec<ShardInfo>,
}

/// A deploy's `term`, clipped for a page.
fn clipped(term: &str) -> Norm {
    if term.chars().count() <= PAGE_TERM_CHARS {
        return Norm::str(term);
    }
    let head: String = term.chars().take(PAGE_TERM_CHARS).collect();
    Norm::str(&format!("{head}…"))
}

pub fn light_to_norm(b: &LightBlockInfo) -> Norm {
    let bonds = Norm::list(
        b.bonds
            .iter()
            .map(|x| Norm::map(vec![(Norm::str("validator"), key_field(&x.validator)), (Norm::str("stake"), Norm::int(x.stake))]))
            .collect(),
    );
    Norm::map(vec![
        (Norm::str("version"), Norm::int(b.version as i64)),
        (Norm::str("shardId"), Norm::str(&b.shard_id)),
        (Norm::str("blockHash"), Norm::str(&b.block_hash)),
        (Norm::str("blockNumber"), Norm::int(b.block_number)),
        (Norm::str("sender"), key_field(&b.sender)),
        (Norm::str("seqNum"), Norm::int(b.seq_num)),
        (Norm::str("preStateHash"), Norm::str(&b.pre_state_hash)),
        (Norm::str("postStateHash"), Norm::str(&b.post_state_hash)),
        (Norm::str("justifications"), Norm::list(b.justifications.iter().map(|j| Norm::str(j)).collect())),
        (Norm::str("bonds"), bonds),
        (Norm::str("sigAlgorithm"), Norm::str(&b.sig_algorithm)),
        (Norm::str("sig"), Norm::str(&b.sig)),
        (Norm::str("blockSize"), Norm::str(&b.block_size)),
        (Norm::str("deployCount"), Norm::int(b.deploy_count as i64)),
        (Norm::str("rejectedDeploys"), Norm::list(b.rejected_deploys.iter().map(|r| Norm::str(r)).collect())),
        (Norm::str("timestamp"), Norm::int(b.timestamp)),
    ])
}

fn deploy_to_norm(d: &DeployInfo) -> Norm {
    Norm::map(vec![
        (Norm::str("deployer"), key_field(&d.deployer)),
        (Norm::str("term"), clipped(&d.term)),
        (Norm::str("timestamp"), Norm::int(d.timestamp)),
        (Norm::str("sig"), Norm::str(&d.sig)),
        (Norm::str("sigAlgorithm"), Norm::str(&d.sig_algorithm)),
        (Norm::str("phloPrice"), Norm::int(d.phlo_price)),
        (Norm::str("phloLimit"), Norm::int(d.phlo_limit)),
        (Norm::str("validAfterBlockNumber"), Norm::int(d.valid_after_block_number)),
        (Norm::str("cost"), json_to_norm(&serde_json::json!(d.cost))),
        (Norm::str("errored"), Norm::bool(d.errored)),
        (Norm::str("systemDeployError"), Norm::str(&d.system_deploy_error)),
    ])
}

pub fn block_to_norm(b: &BlockInfo) -> Norm {
    Norm::map(vec![
        (Norm::str("blockInfo"), light_to_norm(&b.block_info)),
        (Norm::str("deploys"), Norm::list(b.deploys.iter().map(deploy_to_norm).collect())),
    ])
}

pub fn blocks_to_norm(bs: &[LightBlockInfo]) -> Norm {
    Norm::list(bs.iter().map(light_to_norm).collect())
}

pub fn pooled_to_norm(ps: &[PooledDeploy]) -> Norm {
    Norm::list(
        ps.iter()
            .map(|p| {
                Norm::map(vec![
                    (Norm::str("deployId"), Norm::str(&p.deploy_id)),
                    (Norm::str("deployer"), key_field(&p.deployer)),
                    (Norm::str("term"), clipped(&p.term)),
                    (Norm::str("timestamp"), Norm::int(p.timestamp)),
                    (Norm::str("phloPrice"), Norm::int(p.phlo_price)),
                    (Norm::str("phloLimit"), Norm::int(p.phlo_limit)),
                    (Norm::str("validAfterBlockNumber"), Norm::int(p.valid_after_block_number)),
                ])
            })
            .collect(),
    )
}

pub fn caps_to_norm(c: &NodeCapabilities) -> Norm {
    json_to_norm(&serde_json::to_value(c).unwrap_or_default())
}

pub fn shards_to_norm(s: &Shards) -> Norm {
    json_to_norm(&serde_json::to_value(s).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 65 bytes as 130 lowercase hex chars.
    const KEY: &str = "04aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn block_json() -> String {
        format!(
            r#"{{
            "blockInfo": {{"version":1,"shardId":"/root","blockHash":"ab","blockNumber":7,"sender":"{KEY}",
                "seqNum":6,"preStateHash":"p","postStateHash":"q","justifications":["j1"],"bonds":[{{"validator":"{KEY}","stake":3}}],
                "sigAlgorithm":"secp256k1","sig":"3045","blockSize":"512","deployCount":1,"rejectedDeploys":[],"timestamp":99}},
            "deploys": [{{"deployer":"{KEY}","term":"Nil","timestamp":1,"sig":"3044","sigAlgorithm":"secp256k1",
                "phloPrice":1,"phloLimit":100,"validAfterBlockNumber":5,"cost":42,"errored":false,"systemDeployError":""}}]
        }}"#
        )
    }

    fn bytes_len(n: &Norm) -> Option<usize> {
        n.as_lit().and_then(|l| match l {
            k1ndl1ng_norm::Lit::Bytes(b) => Some(b.len()),
            _ => None,
        })
    }

    #[test]
    fn a_block_round_trips_and_clips_terms() {
        let b: BlockInfo = serde_json::from_str(&block_json()).unwrap();
        assert_eq!(b.block_info.block_number, 7);
        assert_eq!(b.deploys[0].cost, 42);
        let n = block_to_norm(&b);
        let bi = n.map_get("blockInfo").unwrap();
        assert_eq!(bi.map_get("blockNumber").and_then(|x| x.as_int()), Some(7));
        // A 65-byte validator key is bytes, not a hex string...
        let v = bi.map_get("bonds").unwrap().as_coll(k1ndl1ng_norm::CollKind::List).unwrap()[0]
            .map_get("validator")
            .unwrap();
        assert_eq!(bytes_len(v), Some(65));
        // ...while a field that merely looks like hex stays a string.
        assert_eq!(bi.map_get("blockHash").and_then(|x| x.as_str()), Some("ab"));
        assert_eq!(key_field("04aa").as_str(), Some("04aa"), "a short key is not 65 bytes");

        // A long term is clipped and marked.
        let long = "x".repeat(PAGE_TERM_CHARS + 10);
        let clipped_n = clipped(&long);
        assert_eq!(clipped_n.as_str().map(|s| s.chars().count()), Some(PAGE_TERM_CHARS + 1));
        assert!(clipped_n.as_str().unwrap().ends_with('…'));
        assert_eq!(clipped("Nil").as_str(), Some("Nil"));
    }

    #[test]
    fn block_specs_address_their_routes() {
        assert_eq!(Blocks::Head.path(None), "/api/blocks");
        assert_eq!(Blocks::Depth(3).path(None), "/api/blocks/3");
        assert_eq!(Blocks::Range(2, 5).path(None), "/api/blocks/2/5");
        // A page-facing read is capped; the CLI passes no cap.
        assert_eq!(Blocks::Depth(1000).path(Some(PAGE_MAX_DEPTH)), "/api/blocks/20");
        assert_eq!(Blocks::Depth(3).path(Some(PAGE_MAX_DEPTH)), "/api/blocks/3");
    }
}
