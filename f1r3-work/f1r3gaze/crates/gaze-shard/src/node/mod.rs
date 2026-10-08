//! The node client: one of two dialects behind one interface.
//!
//! `f1r3node-rust` (F1R3FLY) and `rnode` (rchain-rust) both speak a
//! RChain-family HTTP API, but they differ in the routes, request bodies and
//! response shapes at exactly the places this crate depends on. Each dialect
//! lives in its own module — [`f1r3fly`] and [`rchain`] — so that a
//! f1r3fly-only convention cannot leak into rchain behaviour, or the reverse.
//! This module is the seam: [`Node`] carries the dialect and dispatches.

mod f1r3fly;
mod rchain;

use crate::chain::{self, Blocks};
use crate::deploy::SignedDeploy;
use crate::history;
use crate::pos;
use crate::txn::{self, TxnRequest};
use gaze_net::{Http, HttpRequest};
use serde_json::Value;

/// The refusal for an admin-surface method that is wired for rnode only.
///
/// It says *unverified* rather than "f1r3fly has no such route", because nobody has run an f1r3fly
/// node here: a route that exists and one that does not would both look like this, and a claim either
/// way would be a guess.
fn unverified_on_f1r3fly(what: &str) -> String {
    format!(
        "{what} is wired for the rchain dialect only: its admin route has not been verified against a \
         real f1r3fly node"
    )
}

/// The one refusal the f1r3fly dialect gives the chain and staking reads. It is
/// produced *without touching the wire* — the f1r3fly arm never calls
/// [`Node::call`] — so an rchain-only route cannot physically reach an f1r3fly
/// node, however the caller behaves.
const NO_RCHAIN_READS: &str = "the chain and staking reads are the rchain dialect's; f1r3fly has no such route";

/// Whether a node error was a rate-limit refusal.
///
/// The one failure worth waiting out rather than giving up on: a history walk
/// is dozens of requests, and `/api/transactions` shares the node's deploy rate
/// limiter, so a 429 is expected rather than exceptional.
pub fn is_rate_limited(e: &str) -> bool {
    e.contains("HTTP 429")
}

/// Which node the bridge speaks to.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum NodeDialect {
    /// F1R3FLY's `f1r3node-rust`: `/api/registry`, `/api/estimate-cost`,
    /// `/api/deploy-finalization-status` and a `/ws/events` stream.
    #[default]
    F1r3fly,
    /// rchain-rust's `rnode`: the same deploy wire, but no registry, no
    /// estimate-cost and no websocket, and a tagged `deploy-status` reply.
    Rchain,
}

impl NodeDialect {
    pub fn name(self) -> &'static str {
        match self {
            NodeDialect::F1r3fly => "f1r3fly",
            NodeDialect::Rchain => "rchain",
        }
    }

    /// The `settings.conf` spelling.
    pub fn parse(s: &str) -> Option<NodeDialect> {
        match s.trim().to_ascii_lowercase().as_str() {
            "f1r3fly" | "f1r3" | "f1r3node" => Some(NodeDialect::F1r3fly),
            "rchain" | "rnode" => Some(NodeDialect::Rchain),
            _ => None,
        }
    }

    /// The default shard id, when `settings.conf` does not name one. The root
    /// shard is `root` on f1r3fly and `/root` on rchain.
    pub fn default_shard_id(self) -> &'static str {
        match self {
            NodeDialect::F1r3fly => "root",
            NodeDialect::Rchain => "/root",
        }
    }

    /// The phlo bound quoted when the node offers no cost estimate: rchain has
    /// no `/api/estimate-cost`, so a deploy's limit is a configured bound
    /// rather than a measured price.
    pub fn default_phlo_limit(self) -> i64 {
        250_000
    }

    /// The phrasing for the deploy consent prompt: a measured price on
    /// f1r3fly, a bound on rchain (which cannot estimate). The colon is part
    /// of the phrase so the f1r3fly wording is unchanged.
    pub fn cost_phrase(self) -> &'static str {
        match self {
            NodeDialect::F1r3fly => "Estimated cost:",
            NodeDialect::Rchain => "Up to",
        }
    }
}

/// A client for one node, in one dialect.
#[derive(Clone)]
pub struct Node {
    pub base: String,
    pub dialect: NodeDialect,
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
    pub fn new(base: &str, dialect: NodeDialect, http: Http) -> Node {
        Node {
            base: base.trim_end_matches('/').to_string(),
            dialect,
            http,
        }
    }

    /// One JSON request, with the status kept — so a caller can tell a route
    /// that is *missing* (a 404, which on this node often means a feature is
    /// switched off) from one that answered badly.
    fn call_status(&self, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value), String> {
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
        Ok((r.status, v))
    }

    /// One JSON request; on a non-2xx, the node's message from the body.
    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let (status, v) = self.call_status(method, path, body)?;
        if (200..300).contains(&status) {
            Ok(v)
        } else {
            let msg = v.get("message").or_else(|| v.get("error")).map(|m| m.to_string()).unwrap_or_else(|| v.to_string());
            Err(format!("{} {path}: HTTP {status}: {msg}", self.base))
        }
    }

    /// `GET /api/last-finalized-block` → `(block hash, block number)`, when the
    /// node has a finalized fringe. 400s otherwise; the route and the
    /// `blockInfo` envelope are the same on both nodes.
    fn finalized_head(&self) -> Result<(String, i64), String> {
        let v = self.call("GET", "/api/last-finalized-block", None)?;
        let bi = v.get("blockInfo").unwrap_or(&v);
        let h = bi.get("blockHash").and_then(|x| x.as_str()).ok_or("no blockHash")?.to_string();
        let n = bi.get("blockNumber").and_then(|x| x.as_i64()).unwrap_or(0);
        Ok((h, n))
    }

    /// `(block hash, block number)` anchoring every read and deploy.
    ///
    /// f1r3fly finalizes, so this is its finalized fringe. rchain may have no
    /// finalized fringe at all — a single-validator rnode proposes blocks that
    /// never finalize — so there it falls back to the newest block.
    pub fn last_finalized(&self) -> Result<(String, i64), String> {
        match self.dialect {
            NodeDialect::F1r3fly => self.finalized_head(),
            NodeDialect::Rchain => rchain::last_finalized(self),
        }
    }

    /// The node's own full shard id, from `GET /api/status`.
    ///
    /// A deploy whose `shardId` is not the node's is refused
    /// (`Deploy shardId '…' is not as expected network shard '…'.`), so a
    /// client reads it rather than guessing the spelling.
    pub fn shard_id(&self) -> Result<String, String> {
        let v = self.call("GET", "/api/status", None)?;
        v.get("shardId").and_then(|s| s.as_str()).map(str::to_string).ok_or_else(|| "no shardId in status".into())
    }

    /// `POST /api/faucet` `{address}` → `(deploy id, amount in drops)`.
    ///
    /// Dev-mode only — the node signs the drip server-side from its own funded
    /// deployer key, and answers 404 when there is no faucet. Discoverability
    /// is `GET /api/v1/capabilities`'s `faucet` flag.
    pub fn faucet(&self, address: &str) -> Result<(String, i64), String> {
        let v = self.call("POST", "/api/faucet", Some(&serde_json::json!({ "address": address })))?;
        let id = v.get("deployId").and_then(|s| s.as_str()).ok_or("no deployId in the faucet response")?.to_string();
        Ok((id, v.get("amount").and_then(|a| a.as_i64()).unwrap_or(0)))
    }

    // --- chain reads: the rchain dialect only ------------------------------
    // Each f1r3fly arm is a constant, produced without a request. `f1r3fly.rs`
    // does not change at all for any of these.

    /// A block and its deploys, by hash.
    pub fn block(&self, hash: &str) -> Result<chain::BlockInfo, String> {
        match self.dialect {
            NodeDialect::Rchain => rchain::block(self, hash),
            NodeDialect::F1r3fly => Err(NO_RCHAIN_READS.into()),
        }
    }

    /// The newest blocks named by `spec`. `cap` clamps a page-facing depth.
    pub fn blocks(&self, spec: Blocks, cap: Option<i32>) -> Result<Vec<chain::LightBlockInfo>, String> {
        match self.dialect {
            NodeDialect::Rchain => rchain::blocks(self, spec, cap),
            NodeDialect::F1r3fly => Err(NO_RCHAIN_READS.into()),
        }
    }

    /// The block containing the deploy with this signature.
    pub fn find_deploy(&self, id: &str) -> Result<chain::LightBlockInfo, String> {
        match self.dialect {
            NodeDialect::Rchain => rchain::find_deploy(self, id),
            NodeDialect::F1r3fly => Err(NO_RCHAIN_READS.into()),
        }
    }

    /// Whether the node has finalized the block with this hash.
    pub fn is_finalized(&self, hash: &str) -> Result<bool, String> {
        match self.dialect {
            NodeDialect::Rchain => rchain::is_finalized(self, hash),
            NodeDialect::F1r3fly => Err(NO_RCHAIN_READS.into()),
        }
    }

    /// The deploys this node has accepted and not yet included.
    pub fn pool(&self) -> Result<Vec<chain::PooledDeploy>, String> {
        match self.dialect {
            NodeDialect::Rchain => rchain::pool(self),
            NodeDialect::F1r3fly => Err(NO_RCHAIN_READS.into()),
        }
    }

    /// What this node will do — autopropose, the faucet, the admin surface.
    pub fn capabilities(&self) -> Result<chain::NodeCapabilities, String> {
        match self.dialect {
            NodeDialect::Rchain => rchain::capabilities(self),
            NodeDialect::F1r3fly => Err(NO_RCHAIN_READS.into()),
        }
    }

    /// The shards this node serves, with the primary.
    pub fn shards(&self) -> Result<chain::Shards, String> {
        match self.dialect {
            NodeDialect::Rchain => rchain::shards(self),
            NodeDialect::F1r3fly => Err(NO_RCHAIN_READS.into()),
        }
    }

    /// The REV transfers in the block with this hash, from the node's own
    /// report. Off by default on the node (`api-server.enable-reporting`), and
    /// a 404 then, which [`rchain::transactions`] turns into that advice.
    pub fn transactions(&self, hash: &str) -> Result<Vec<history::Transfer>, String> {
        match self.dialect {
            NodeDialect::Rchain => rchain::transactions(self, hash),
            NodeDialect::F1r3fly => Err(NO_RCHAIN_READS.into()),
        }
    }

    // --- staking reads: the rchain dialect only ----------------------------

    /// The epoch, its length, the active set and every staged withdrawal.
    pub fn pos_status(&self) -> Result<pos::PosStatus, String> {
        match self.dialect {
            NodeDialect::Rchain => rchain::pos_status(self),
            NodeDialect::F1r3fly => Err(NO_RCHAIN_READS.into()),
        }
    }

    /// One delegator's positions across the operators it has staked with. The
    /// key is 65-byte hex; a malformed one is a 400 at the node, so it is
    /// validated before sending.
    pub fn pos_delegations(&self, key: &str) -> Result<Vec<pos::DelegatorPosition>, String> {
        pos::validate_key(key)?;
        match self.dialect {
            NodeDialect::Rchain => rchain::pos_delegations(self, key),
            NodeDialect::F1r3fly => Err(NO_RCHAIN_READS.into()),
        }
    }

    /// Registry entry at `uri`, at `block` (default: last finalized):
    /// `(data, block hash, block number)`.
    pub fn registry(&self, uri: &str, block: Option<&str>) -> Result<(Vec<Value>, String, i64), String> {
        match self.dialect {
            NodeDialect::F1r3fly => f1r3fly::registry(self, uri, block),
            NodeDialect::Rchain => rchain::registry(self, uri, block),
        }
    }

    /// Exploratory deploy at `block`'s post-state: `(values on return, block
    /// hash, cost)`.
    pub fn explore(&self, term: &str, block: &str) -> Result<(Vec<Value>, String, u64), String> {
        match self.dialect {
            NodeDialect::F1r3fly => f1r3fly::explore(self, term),
            NodeDialect::Rchain => rchain::explore(self, term, block),
        }
    }

    /// Data at a private unforgeable name, at `block`.
    pub fn data_at_private(&self, hex: &str, block: &str) -> Result<Vec<Value>, String> {
        match self.dialect {
            NodeDialect::F1r3fly => f1r3fly::data_at_private(self, hex, block),
            NodeDialect::Rchain => rchain::data_at_private(self, hex, block),
        }
    }

    /// A cost estimate for `term`. rchain has no such endpoint (the bridge
    /// quotes a configured bound in its place), so this errors there.
    pub fn estimate_cost(&self, term: &str, deployer_hex: &str) -> Result<u64, String> {
        match self.dialect {
            NodeDialect::F1r3fly => f1r3fly::estimate_cost(self, term, deployer_hex),
            NodeDialect::Rchain => Err("the rchain dialect has no estimate-cost endpoint".into()),
        }
    }

    /// Force a block, returning the node's message.
    ///
    /// The route is on the node's **admin** listener (rnode's 40405), not the
    /// API port, so a `Node` built from the API base will not find it —
    /// [`crate::Bridge::propose`] builds the right one from `ShardConfig::admin`.
    pub fn propose(&self) -> Result<String, String> {
        match self.dialect {
            NodeDialect::Rchain => {
                let v = self.call("POST", "/api/propose", None)?;
                Ok(v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))
            }
            NodeDialect::F1r3fly => Err(unverified_on_f1r3fly("propose")),
        }
    }

    // --- the gateway's cross-shard transaction API (rchain only) ------------
    //
    // Like `propose`, these are on the **admin** listener, so a `Node` built from the API base will
    // not find them; `Bridge::txn_*` builds the right one. They are the *gateway's* surface: a node
    // that is a member of several shards drives the transaction itself, signing every leg with its
    // own validator key.

    /// Open a transaction. The node coordinates it: it prepares each leg on the shard that owns it,
    /// collects the votes, and commits all or aborts the prepared legs.
    pub fn txn_open(&self, req: &TxnRequest) -> Result<txn::TxnRecord, String> {
        match self.dialect {
            NodeDialect::Rchain => {
                let body = serde_json::to_value(req).map_err(|e| e.to_string())?;
                let v = self.call("POST", "/api/v1/txn", Some(&body))?;
                serde_json::from_value(v).map_err(|e| format!("txn: {e}"))
            }
            NodeDialect::F1r3fly => Err(unverified_on_f1r3fly("the cross-shard transaction API")),
        }
    }

    /// One transaction's record, by id.
    pub fn txn_status(&self, id: &str) -> Result<txn::TxnRecord, String> {
        match self.dialect {
            NodeDialect::Rchain => {
                let v = self.call("GET", &format!("/api/v1/txn/{}", enc(id)), None)?;
                serde_json::from_value(v).map_err(|e| format!("txn: {e}"))
            }
            NodeDialect::F1r3fly => Err(unverified_on_f1r3fly("the cross-shard transaction API")),
        }
    }

    /// The transactions this node still holds.
    pub fn txn_list(&self) -> Result<txn::TxnList, String> {
        match self.dialect {
            NodeDialect::Rchain => {
                let v = self.call("GET", "/api/v1/txn", None)?;
                serde_json::from_value(v).map_err(|e| format!("txn: {e}"))
            }
            NodeDialect::F1r3fly => Err(unverified_on_f1r3fly("the cross-shard transaction API")),
        }
    }

    /// Submit; returns the node's message.
    pub fn deploy(&self, d: &SignedDeploy) -> Result<String, String> {
        match self.dialect {
            NodeDialect::F1r3fly => f1r3fly::deploy(self, d),
            NodeDialect::Rchain => rchain::deploy(self, d),
        }
    }

    /// `(state, latest block)`; state is Finalized, Failed, Pending or Expired.
    pub fn finalization(&self, sig_hex: &str) -> Result<(String, Option<String>), String> {
        match self.dialect {
            NodeDialect::F1r3fly => f1r3fly::finalization(self, sig_hex),
            NodeDialect::Rchain => rchain::finalization(self, sig_hex),
        }
    }

    /// A deploy's outcome **including the value it produced** — which is where
    /// a write's answer lives.
    ///
    /// f1r3fly's route carries only a state and a block, so its result is empty;
    /// it is `rchain_only` for the staking writes anyway, and this keeps the
    /// dialect from pretending otherwise.
    pub fn deploy_outcome(&self, sig_hex: &str) -> Result<chain::DeployOutcome, String> {
        match self.dialect {
            NodeDialect::Rchain => rchain::deploy_outcome(self, sig_hex),
            NodeDialect::F1r3fly => {
                let (state, block) = self.finalization(sig_hex)?;
                Ok(chain::DeployOutcome {
                    state,
                    block,
                    result: Vec::new(),
                    error: None,
                })
            }
        }
    }

    /// The event stream's URL, when the node has one. rchain has no websocket,
    /// so a caller that gets `None` polls instead.
    pub fn events_url(&self) -> Option<String> {
        match self.dialect {
            NodeDialect::F1r3fly => Some(f1r3fly::events_url(self)),
            NodeDialect::Rchain => None,
        }
    }
}
