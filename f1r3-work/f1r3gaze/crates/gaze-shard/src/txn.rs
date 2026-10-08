//! Cross-shard transactions: the gateway's two-phase commit.
//!
//! A transaction commits on **every** participant shard or aborts on every one (Law 27), decided once
//! by a coordinator and recorded durably (Laws 26–29, `docs/src/formal/cross-shard-transactions.md`).
//! When the coordinator is the **node itself** — a node that is a member of several shards, which is
//! what `f1r3gaze devnet up --shards 2` starts — the whole transaction is driven on the node and
//! reached as `POST /api/v1/txn` on its **admin** listener, beside `propose`.
//!
//! **Whose funds move is the thing not to misread.** The gateway signs every leg with the node's
//! validator key and escrows out of that key's own REV account, so a transaction is the *node* moving
//! its own funds between its own shards. It is not a client-signed transfer, and the CLI says so.

use crate::expr::json_to_norm;
use k1ndl1ng_norm::Norm;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TxnLeg {
    /// The shard that owns the leg — every request resolves to exactly one.
    pub shard_id: String,
    pub amount: i64,
    /// A REV address on that shard.
    pub to: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TxnVote {
    pub shard_id: String,
    /// `ready` or `abort`.
    pub vote: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TxnRecord {
    pub txn_id: String,
    /// `proposed`, `prepared`, `committed` or `aborted`.
    pub state: String,
    pub coordinator: String,
    pub record_hash: String,
    pub legs: Vec<TxnLeg>,
    /// One per shard that voted; a leg that never prepared has none.
    pub votes: Vec<TxnVote>,
    /// Why it aborted, when it did.
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TxnRequest {
    pub txn_id: String,
    pub legs: Vec<TxnLeg>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TxnList {
    pub in_flight: Vec<TxnRecord>,
}

impl TxnRecord {
    /// The decision has been made — a `prepared` or `proposed` record is still in flight, and a
    /// participant holding a lock from one of those is holding it until this changes.
    pub fn is_terminal(&self) -> bool {
        self.state == "committed" || self.state == "aborted"
    }

    /// Every leg voted `ready`. This is the recorded shape of a commit, and it is a *check* on the
    /// record rather than the decision: the coordinator decides once and the record is the evidence.
    pub fn all_ready(&self) -> bool {
        !self.legs.is_empty() && self.votes.len() == self.legs.len() && self.votes.iter().all(|v| v.vote == "ready")
    }
}

/// The plain-JSON shape, for a caller that renders it (the CLI, and a page verb if one is ever added).
pub fn record_to_norm(r: &TxnRecord) -> Norm {
    json_to_norm(&serde_json::to_value(r).unwrap_or_default())
}

/// `SHARD:AMOUNT:TO` — the CLI's one-leg spelling. A shard id is a path (`/root/child1`) and a REV
/// address is base58, so neither holds a colon and the split is unambiguous.
pub fn parse_leg(s: &str) -> Result<TxnLeg, String> {
    let mut it = s.splitn(3, ':');
    let (Some(shard_id), Some(amount), Some(to)) = (it.next(), it.next(), it.next()) else {
        return Err(format!("{s} is not SHARD:AMOUNT:TO"));
    };
    if shard_id.is_empty() || to.is_empty() {
        return Err(format!("{s} is not SHARD:AMOUNT:TO"));
    }
    let amount: i64 = amount.parse().map_err(|_| format!("{s}: the amount is not a number"))?;
    if amount < 0 {
        return Err(format!("{s}: the amount is negative"));
    }
    Ok(TxnLeg {
        shard_id: shard_id.to_string(),
        amount,
        to: to.to_string(),
    })
}

/// A transaction id: non-empty hex, at most 64 bytes, which is what the node's ingress checks.
pub fn fresh_id(now_ms: i64) -> String {
    format!("{now_ms:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legs_parse_the_way_the_cli_writes_them() {
        let l = parse_leg("/root/child1:100:11112VYAt8rUGNRRZX3eJdgagaAhtWTK8Js7F7X5iqddMVqyDTtYau").unwrap();
        assert_eq!(l.shard_id, "/root/child1");
        assert_eq!(l.amount, 100);
        assert!(l.to.starts_with("11112"));
        assert!(parse_leg("/root:x:addr").is_err(), "amount");
        assert!(parse_leg("/root:100").is_err(), "missing the address");
        assert!(parse_leg("/root:-1:addr").is_err(), "negative");
        assert!(parse_leg(":100:addr").is_err(), "no shard");
    }

    #[test]
    fn a_commit_is_a_record_where_every_leg_voted_ready() {
        let mut r = TxnRecord {
            txn_id: "01".into(),
            state: "committed".into(),
            coordinator: "04".into(),
            record_hash: "ab".into(),
            legs: vec![
                TxnLeg { shard_id: "/root".into(), amount: 1, to: "a".into() },
                TxnLeg { shard_id: "/root/child1".into(), amount: 1, to: "a".into() },
            ],
            votes: vec![
                TxnVote { shard_id: "/root".into(), vote: "ready".into() },
                TxnVote { shard_id: "/root/child1".into(), vote: "ready".into() },
            ],
            reason: None,
        };
        assert!(r.is_terminal() && r.all_ready());
        // A leg that never prepared has no vote, so the sets cannot be equal.
        r.votes.pop();
        assert!(!r.all_ready());
        r.state = "prepared".into();
        assert!(!r.is_terminal(), "in flight, and a prepared participant still holds its lock");
    }
}
