//! Staking (proof-of-stake) reads on the rchain dialect.
//!
//! Two surfaces, both read-only:
//!
//! - the node's HTTP reads, `GET /api/v1/pos` and `/api/v1/pos/delegations`;
//! - the native `rho:rchain:pos` contract's read methods (`getBonds`,
//!   `getActiveValidators`, `getTrusted`, `getDelegations`), reached through an
//!   exploratory deploy. None of the read methods takes `*deployerId`, which is
//!   exactly what lets them run under `explore` — the writes (`bond`,
//!   `withdraw`, `delegate`, `undelegate`, `trust`, `untrust`) do, and are out
//!   of scope here.
//!
//! The native methods are reached via `rho:registry:lookup` on `rho:rchain:pos`
//! and reply on the term's **first `new`-bound name**, because the exploratory
//! path reads there (see [`crate::wallet`] for the same rule). Terms follow
//! `rchain-community/r-wallet` (`src/utils/rho.ts::fn_pos_info`).
//!
//! Two wire traps are handled deliberately, both of them in the node's
//! `RhoExpr` conversion (`node/src/api/rho_expr.rs`):
//!
//! 1. A `ByteArray` **map key** is stringified to lowercase hex, so a
//!    `getBonds` reply read through `expr::to_norm` would carry *string* keys
//!    and lose the 65-byte-ness. [`bonds_from`] decodes them back to bytes.
//! 2. A `Nil` **tuple element is filtered out**, so a `getDelegations` entry
//!    arrives as a 3-element tuple when nothing is staged and a 4-element one
//!    when it is. [`delegations_from`] accepts both and normalises the absent
//!    deadline to `Norm::nil`.
//!
//! The **writes** are here too — [`bond_term`] and [`withdraw_term`] — and they
//! differ from the reads in the two ways that matter:
//!
//! - They bind `rho:rchain:deployerId` and `rho:rchain:deployId` and reply on
//!   the latter, because a *deploy's* result is read from its own deploy id
//!   channel, not from the term's first `new`-bound name. A write that replied
//!   on its first private name would report nothing, hiding the node's refusal.
//! - Their reply is `(Bool, Nil | String)`, and **a refusal is a successful
//!   deploy**. It is not a deploy error; it is the value the term sent. So the
//!   caller must read the result back — [`reply_from`] is that parse — and the
//!   reason it yields is the same `Nil`-filtered tuple the reads contend with.

use crate::expr::{key_field, to_norm};
use crate::wallet::{DEPLOYER_ID, DEPLOY_ID};
use k1ndl1ng_norm::Norm;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The native PoS contract, and the registry that resolves it.
pub const POS: &str = "rho:rchain:pos";
pub const REGISTRY: &str = "rho:registry:lookup";

/// The phlo a staking write is allowed to spend. The writes run a native
/// process and a handful of rholang steps; this is the reference client's
/// limit for the same operations.
pub const STAKE_PHLO_LIMIT: i64 = 500_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingWithdrawal {
    pub validator: String,
    pub deadline: i64,
    pub blocks_remaining: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PosStatus {
    pub latest_block_number: i64,
    pub epoch_length: i64,
    pub quarantine_length: i64,
    pub epoch: i64,
    pub blocks_until_epoch_boundary: i64,
    pub active_validators: Vec<String>,
    pub pending_withdrawals: Vec<PendingWithdrawal>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingUndelegation {
    pub deadline: i64,
    pub blocks_remaining: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegatorPosition {
    pub operator: String,
    pub amount: i64,
    pub accrued_rewards: i64,
    pub pending_undelegation: Option<PendingUndelegation>,
}

/// A validator key is 65 bytes, so its hex is 130 characters. The node answers
/// **400** for anything else, and an empty list is a *true answer* ("no
/// positions"), so a malformed key is rejected here rather than sent and
/// misread as "none".
pub fn validate_key(hex: &str) -> Result<(), String> {
    match gaze_net::unhex(hex) {
        Some(b) if b.len() == 65 => Ok(()),
        Some(b) => Err(format!("a validator key is 65 bytes, not {}", b.len())),
        None => Err("a validator key must be hex".into()),
    }
}

/// The shared skeleton: look the contract up, call it, reply on `return` (the
/// term's first `new`-bound name, which is where an exploratory deploy reads).
fn pos_call(call: &str) -> String {
    format!(
        "new return, PoSCh, rl(`{REGISTRY}`), retCh in {{\n  \
         rl!(`{POS}`, *PoSCh) |\n  \
         for (@(_, PoS) <- PoSCh) {{\n    \
         {call} |\n    \
         for (@v <- retCh) {{ return!(v) }}\n  \
         }}\n}}"
    )
}

pub fn bonds_term() -> String {
    pos_call("@PoS!(\"getBonds\", *retCh)")
}

pub fn active_validators_term() -> String {
    pos_call("@PoS!(\"getActiveValidators\", *retCh)")
}

pub fn trusted_term() -> String {
    pos_call("@PoS!(\"getTrusted\", *retCh)")
}

pub fn delegations_term(key_hex: &str) -> String {
    pos_call(&format!("@PoS!(\"getDelegations\", \"{key_hex}\".hexToBytes(), *retCh)"))
}

// --- writes ---------------------------------------------------------------
//
// A write binds the payer's `deployerId` and the deploy's own `deployId`, and
// forwards the `(Bool, Nil | String)` reply to the latter. The validator is
// derived by the node from `*deployerId`, so neither term names a key: a
// caller cannot bond or withdraw for anyone but itself.

fn pos_write(call: &str) -> String {
    format!(
        "new retCh, PoSCh, rl(`{REGISTRY}`), deployerId(`{DEPLOYER_ID}`), deployId(`{DEPLOY_ID}`) in {{\n  \
         rl!(`{POS}`, *PoSCh) |\n  \
         for (@(_, PoS) <- PoSCh) {{\n    \
         {call} |\n    \
         for (@result <- retCh) {{ deployId!(result) }}\n  \
         }}\n}}"
    )
}

/// Self-bond `amount` at the validator this deploy's key signs as. Refused for
/// an unbonded-and-untrusted key, and for an amount outside the shard's bounds.
pub fn bond_term(amount: i64) -> String {
    pos_write(&format!("@PoS!(\"bond\", *deployerId, {amount}, *retCh)"))
}

/// Stage the unbond. The validator stays bonded and earning until the next
/// epoch boundary, then the claim is escrowed for the quarantine before it
/// pays. Refused while the validator carries outstanding delegations.
pub fn withdraw_term() -> String {
    pos_write("@PoS!(\"withdraw\", *deployerId, *retCh)")
}

/// The `(Bool, Nil | String)` reply a write produces: `Ok(())` when the node
/// did the thing, `Err(reason)` when it refused.
///
/// The success tuple is `(true, Nil)` on the wire, and the node's conversion
/// **filters a `Nil` element out** — so a success arrives as a *one*-element
/// tuple and a refusal as a two-element one. Both are accepted, as is a bare
/// boolean, in case a future revision unwraps a one-tuple.
pub fn reply_from(values: &[Value]) -> Result<(), String> {
    let first = values.first().ok_or("the pos write answered nothing")?;
    let (ok, reason) = match first.pointer("/ExprTuple/data").and_then(|d| d.as_array()) {
        Some(t) => (
            t.first()
                .and_then(|v| v.pointer("/ExprBool/data"))
                .and_then(|d| d.as_bool())
                .ok_or("the pos write answered no boolean")?,
            t.get(1).and_then(|v| v.pointer("/ExprString/data")).and_then(|d| d.as_str()).map(str::to_string),
        ),
        None => (
            first
                .pointer("/ExprBool/data")
                .and_then(|d| d.as_bool())
                .ok_or("the pos write answered no result")?,
            None,
        ),
    };
    if ok {
        Ok(())
    } else {
        Err(reason.unwrap_or_else(|| "refused, with no reason given".into()))
    }
}

fn hex_key(k: &str) -> Norm {
    gaze_net::unhex(k).map(|b| Norm::bytes(&b)).unwrap_or_else(|| Norm::str(k))
}

fn int_at(v: &Value) -> i64 {
    v.pointer("/ExprInt/data").and_then(|d| d.as_i64()).unwrap_or(0)
}

fn bytes_at(v: &Value) -> Norm {
    v.pointer("/ExprBytes/data").and_then(|d| d.as_str()).map(hex_key).unwrap_or_else(Norm::nil)
}

/// `getBonds` — `Map[ByteArray(65) → Int]` — with the keys decoded back to
/// bytes (trap 1).
pub fn bonds_from(values: &[Value]) -> Result<Norm, String> {
    let m = values
        .first()
        .and_then(|v| v.pointer("/ExprMap/data"))
        .and_then(|d| d.as_object())
        .ok_or("getBonds answered no map")?;
    Ok(Norm::map(m.iter().map(|(k, v)| (hex_key(k), to_norm(v))).collect()))
}

/// `getActiveValidators` / `getTrusted` — a `Set[ByteArray(65)]`. Accepts a set
/// or a list, since the node renders both.
pub fn keys_from(values: &[Value], what: &str) -> Result<Norm, String> {
    let l = values
        .first()
        .and_then(|v| v.pointer("/ExprSet/data").or_else(|| v.pointer("/ExprList/data")))
        .and_then(|d| d.as_array())
        .ok_or_else(|| format!("{what} answered no set"))?;
    Ok(Norm::list(l.iter().map(bytes_at).collect()))
}

/// `getDelegations` — a list of `[operator, amount, accrued, deadline]`, where
/// the deadline is absent (a 3-element tuple) when nothing is staged (trap 2).
/// Both arities normalise to a 4-tuple with `nil` for the absent deadline.
pub fn delegations_from(values: &[Value]) -> Result<Norm, String> {
    let l = values
        .first()
        .and_then(|v| v.pointer("/ExprList/data"))
        .and_then(|d| d.as_array())
        .ok_or("getDelegations answered no list")?;
    let mut out = Vec::with_capacity(l.len());
    for item in l {
        let t = item.pointer("/ExprTuple/data").and_then(|d| d.as_array()).ok_or("a delegation is not a tuple")?;
        let deadline = match t.len() {
            0..=3 => Norm::nil(),
            _ => Norm::int(int_at(&t[3])),
        };
        out.push(Norm::tuple(vec![
            t.first().map(bytes_at).unwrap_or_else(Norm::nil),
            Norm::int(t.get(1).map(int_at).unwrap_or(0)),
            Norm::int(t.get(2).map(int_at).unwrap_or(0)),
            deadline,
        ]));
    }
    Ok(Norm::list(out))
}

pub fn status_to_norm(s: &PosStatus) -> Norm {
    Norm::map(vec![
        (Norm::str("latestBlockNumber"), Norm::int(s.latest_block_number)),
        (Norm::str("epochLength"), Norm::int(s.epoch_length)),
        (Norm::str("quarantineLength"), Norm::int(s.quarantine_length)),
        (Norm::str("epoch"), Norm::int(s.epoch)),
        (Norm::str("blocksUntilEpochBoundary"), Norm::int(s.blocks_until_epoch_boundary)),
        (Norm::str("activeValidators"), Norm::list(s.active_validators.iter().map(|k| key_field(k)).collect())),
        (
            Norm::str("pendingWithdrawals"),
            Norm::list(
                s.pending_withdrawals
                    .iter()
                    .map(|w| {
                        Norm::map(vec![
                            (Norm::str("validator"), key_field(&w.validator)),
                            (Norm::str("deadline"), Norm::int(w.deadline)),
                            (Norm::str("blocksRemaining"), Norm::int(w.blocks_remaining)),
                        ])
                    })
                    .collect(),
            ),
        ),
    ])
}

pub fn positions_to_norm(ps: &[DelegatorPosition]) -> Norm {
    Norm::list(
        ps.iter()
            .map(|p| {
                let staged = match &p.pending_undelegation {
                    Some(u) => Norm::map(vec![
                        (Norm::str("deadline"), Norm::int(u.deadline)),
                        (Norm::str("blocksRemaining"), Norm::int(u.blocks_remaining)),
                    ]),
                    None => Norm::nil(),
                };
                Norm::map(vec![
                    (Norm::str("operator"), key_field(&p.operator)),
                    (Norm::str("amount"), Norm::int(p.amount)),
                    (Norm::str("accruedRewards"), Norm::int(p.accrued_rewards)),
                    (Norm::str("pendingUndelegation"), staged),
                ])
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 65 bytes as 130 lowercase hex chars.
    const KEY: &str = "04aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn the_terms_are_explore_safe() {
        for t in [bonds_term(), active_validators_term(), trusted_term(), delegations_term(KEY)] {
            assert!(t.starts_with("new return,"), "replies on the first new-bound name: {t}");
            assert!(t.contains("rho:rchain:pos"), "{t}");
            assert!(t.contains("rho:registry:lookup"), "{t}");
            // A read must never name the deployer capability -- that would make
            // it a write, and it could not run under an exploratory deploy.
            assert!(!t.contains("deployerId"), "{t}");
            assert!(!t.contains("deployId"), "{t}");
        }
        assert!(delegations_term(KEY).contains(".hexToBytes()"));
        assert!(bonds_term().contains("getBonds"));
        assert!(active_validators_term().contains("getActiveValidators"));
        assert!(trusted_term().contains("getTrusted"));
    }

    #[test]
    fn a_validator_key_is_65_bytes() {
        assert!(validate_key(KEY).is_ok());
        assert!(validate_key("04aa").is_err());
        assert!(validate_key("zz".repeat(65).as_str()).is_err());
        assert!(validate_key("").is_err());
    }

    #[test]
    fn bonds_keys_are_bytes_not_strings() {
        // A ByteArray map key arrives as lowercase hex; it must come back as
        // bytes (trap 1).
        let v = vec![json!({"ExprMap": {"data": {KEY: {"ExprInt": {"data": 7}}}}})];
        let n = bonds_from(&v).unwrap();
        let pair = n.as_coll(k1ndl1ng_norm::CollKind::Map).unwrap();
        let key = &pair[0];
        assert_eq!(
            key.as_lit().and_then(|l| match l {
                k1ndl1ng_norm::Lit::Bytes(b) => Some(b.len()),
                _ => None,
            }),
            Some(65),
            "a bonds key is a 65-byte array, not a 130-char string"
        );
        assert_eq!(pair[1].as_int(), Some(7));
    }

    #[test]
    fn both_delegation_arities_normalise() {
        let staged = vec![json!({"ExprList": {"data": [
            {"ExprTuple": {"data": [{"ExprBytes": {"data": KEY}}, {"ExprInt": {"data": 10}}, {"ExprInt": {"data": 2}}, {"ExprInt": {"data": 500}}]}}
        ]}})];
        let unstaged = vec![json!({"ExprList": {"data": [
            {"ExprTuple": {"data": [{"ExprBytes": {"data": KEY}}, {"ExprInt": {"data": 10}}, {"ExprInt": {"data": 2}}]}}
        ]}})];
        let a = delegations_from(&staged).unwrap();
        let b = delegations_from(&unstaged).unwrap();
        let ta = a.as_coll(k1ndl1ng_norm::CollKind::List).unwrap()[0]
            .as_coll(k1ndl1ng_norm::CollKind::Tuple)
            .unwrap();
        let tb = b.as_coll(k1ndl1ng_norm::CollKind::List).unwrap()[0]
            .as_coll(k1ndl1ng_norm::CollKind::Tuple)
            .unwrap();
        assert_eq!(ta.len(), 4);
        assert_eq!(tb.len(), 4, "both arities normalise to four elements");
        assert_eq!(ta[3].as_int(), Some(500));
        assert!(tb[3].is_nil(), "an absent deadline is nil");
        assert_eq!(tb[1].as_int(), Some(10));
    }

    #[test]
    fn the_write_terms_bind_the_capability_and_reply_on_the_deploy_id() {
        for t in [bond_term(1_000_000), withdraw_term()] {
            assert!(t.starts_with("new retCh,"), "{t}");
            assert!(t.contains("rho:rchain:pos"), "{t}");
            assert!(t.contains("deployerId(`rho:rchain:deployerId`)"), "{t}");
            assert!(t.contains("deployId(`rho:rchain:deployId`)"), "{t}");
            // The reply goes to the deploy's own id channel -- a write that
            // replied on its first private name would report nothing.
            assert!(t.contains("deployId!(result)"), "{t}");
            assert!(!t.contains("return!"), "{t}");
        }
        assert!(bond_term(1_000_000).contains("bond\", *deployerId, 1000000,"));
        assert!(withdraw_term().contains("withdraw\", *deployerId,"));
        // A write names no key: the node derives the validator from the
        // capability, so a caller can only ever act for itself.
        assert!(!bond_term(1).contains(".hexToBytes()"), "a bond names no key");
    }

    #[test]
    fn both_write_reply_arities_parse() {
        // (true, Nil) -- the Nil is filtered by the node, leaving one element.
        let ok = vec![json!({"ExprTuple": {"data": [{"ExprBool": {"data": true}}]}})];
        assert_eq!(reply_from(&ok), Ok(()));
        // (false, "reason") -- two elements, and the reason is the whole point.
        let refused = vec![json!({"ExprTuple": {"data": [
            {"ExprBool": {"data": false}},
            {"ExprString": {"data": "Public key is already bonded."}}
        ]}})];
        assert_eq!(reply_from(&refused), Err("Public key is already bonded.".to_string()));
        // A bare boolean, in case a one-tuple is ever unwrapped.
        assert_eq!(reply_from(&[json!({"ExprBool": {"data": true}})]), Ok(()));
        assert!(reply_from(&[json!({"ExprBool": {"data": false}})]).is_err());
        // Anything else is not an answer.
        assert!(reply_from(&[]).is_err());
        assert!(reply_from(&[json!({"ExprString": {"data": "?"}})]).is_err());
    }
}
