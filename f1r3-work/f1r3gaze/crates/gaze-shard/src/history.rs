//! REV transfer history, read from the node's own block reports.
//!
//! rnode serves `GET /api/transactions/{blockHash}` — the REV transfers in one
//! block — when `api-server.enable-reporting` is on. An *address's* history is
//! therefore a walk backwards over recent blocks: [`windows`] is the arithmetic
//! and [`crate::Bridge::transfer_history`] drives it.
//!
//! The node sends each transfer's `retUnforgeable` as a whole RChain `Par`
//! **AST** (a nested object with `sends`/`receives`/`news`/… keys). Nothing here
//! declares it — serde ignores unknown fields — which is how a client that has
//! no use for the AST avoids having to model one in JSON. It is correspondingly
//! absent from the [`Norm`] a page receives.

use k1ndl1ng_norm::Norm;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The most blocks one history read will walk.
pub const HISTORY_MAX_BLOCKS: i32 = 200;

/// How many blocks one `/api/blocks/{start}/{end}` call may cover. The node
/// refuses a wider window (`api-server.max-blocks-limit`, default 50), so this
/// is its limit rather than a preference.
pub const WALK_WINDOW: i64 = 50;

/// How many transfers a *page* receives in one `chain!("txns", …)` reply. The
/// CLI prints everything it found.
pub const PAGE_TRANSFERS: usize = 20;

/// One REV transfer: the node's fields, plus the block the walk found it in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transfer {
    pub from_addr: String,
    pub to_addr: String,
    pub amount: i64,
    #[serde(default)]
    pub fail_reason: Option<String>,
    /// `PreCharge`, `UserDeploy`, `Refund`, `CloseBlock` or `SlashingDeploy`.
    pub kind: String,
    /// The deploy id the type names, or the block hash for a system type.
    #[serde(default)]
    pub ref_id: Option<String>,
    /// Where it was found. The node's per-block reply does not carry these; the
    /// walk stamps them.
    #[serde(default)]
    pub block_hash: String,
    #[serde(default)]
    pub block_number: i64,
}

/// What a history walk found, and how much of the chain it could read.
///
/// The two counts matter: a walk is dozens of requests against a route that
/// shares the node's rate limiter, so "no transfers" and "no transfers *in the
/// blocks I could read*" are different answers, and only one of them is about
/// the address.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct History {
    pub transfers: Vec<Transfer>,
    /// Blocks whose report was read.
    pub blocks_read: i64,
    /// Blocks whose report could not be read, whose transfers are therefore
    /// missing from `transfers`.
    pub blocks_unread: i64,
}

/// The node's reply: `{"data": [{"transaction": {…}, "transactionType": {…}}]}`.
#[derive(Deserialize)]
struct RawResponse {
    #[serde(default)]
    data: Vec<RawInfo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawInfo {
    transaction: RawTransaction,
    /// An externally-tagged enum, and the node's `rename_all_fields` renames
    /// only the *inner* fields — so the tag is the variant as written
    /// (`{"UserDeploy":{"deployId":"…"}}`). Read as a `Value` rather than five
    /// hand-written variants, which is all this needs.
    transaction_type: Value,
}

/// Deliberately without `retUnforgeable`: see the module note.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTransaction {
    from_addr: String,
    to_addr: String,
    amount: i64,
    #[serde(default)]
    fail_reason: Option<String>,
}

/// The transfers in one block's report.
pub fn parse_transfers(v: &Value) -> Result<Vec<Transfer>, String> {
    let r: RawResponse = serde_json::from_value(v.clone()).map_err(|e| format!("transactions: {e}"))?;
    Ok(r.data
        .into_iter()
        .map(|i| {
            let (kind, ref_id) = kind_of(&i.transaction_type);
            Transfer {
                from_addr: i.transaction.from_addr,
                to_addr: i.transaction.to_addr,
                amount: i.transaction.amount,
                fail_reason: i.transaction.fail_reason,
                kind,
                ref_id,
                block_hash: String::new(),
                block_number: 0,
            }
        })
        .collect())
}

/// `{"UserDeploy": {"deployId": "…"}}` → `("UserDeploy", Some("…"))`.
fn kind_of(v: &Value) -> (String, Option<String>) {
    let Some((tag, inner)) = v.as_object().and_then(|o| o.iter().next()) else {
        return ("Unknown".into(), None);
    };
    let id = inner
        .get("deployId")
        .or_else(|| inner.get("blockHash"))
        .and_then(|x| x.as_str())
        .map(str::to_string);
    (tag.clone(), id)
}

/// The height windows to walk, newest first. Each is inclusive and no wider
/// than `window`, and answers in the order `/api/blocks/{start}/{end}` does
/// (oldest first inside a window). Stops at height 1, so a chain shorter than
/// the request yields what exists.
pub fn windows(tip: i64, blocks: i32, window: i64) -> Vec<(i64, i64)> {
    let mut left = i64::from(blocks.clamp(1, HISTORY_MAX_BLOCKS));
    let window = window.max(1);
    let mut end = tip.min(i64::from(i32::MAX));
    let mut out = Vec::new();
    while left > 0 && end >= 1 {
        let start = (end - window.min(left) + 1).max(1);
        out.push((start, end));
        left -= end - start + 1;
        end = start - 1;
    }
    out
}

pub fn transfer_to_norm(t: &Transfer) -> Norm {
    Norm::map(vec![
        (Norm::str("blockNumber"), Norm::int(t.block_number)),
        (Norm::str("blockHash"), Norm::str(&t.block_hash)),
        (Norm::str("from"), Norm::str(&t.from_addr)),
        (Norm::str("to"), Norm::str(&t.to_addr)),
        (Norm::str("amount"), Norm::int(t.amount)),
        (Norm::str("kind"), Norm::str(&t.kind)),
        (Norm::str("refId"), t.ref_id.as_deref().map(Norm::str).unwrap_or_else(Norm::nil)),
        (Norm::str("failReason"), t.fail_reason.as_deref().map(Norm::str).unwrap_or_else(Norm::nil)),
    ])
}

pub fn transfers_to_norm(ts: &[Transfer]) -> Norm {
    Norm::list(ts.iter().take(PAGE_TRANSFERS).map(transfer_to_norm).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reply with a `retUnforgeable` Par in it, so the fixture pins that the
    /// AST is ignored rather than merely absent.
    const REPLY: &str = r#"{"data": [
        {"transaction": {"fromAddr": "1111aaa", "toAddr": "1111bbb", "amount": 250000000,
                         "retUnforgeable": {"sends": [], "receives": [], "news": [],
                                            "exprs": [{"GString": "1111bbb"}], "matches": [],
                                            "unforgeables": [], "bundles": [], "connectives": [],
                                            "locally_free": null, "connective_used": false},
                         "failReason": null},
         "transactionType": {"UserDeploy": {"deployId": "aa11"}}},
        {"transaction": {"fromAddr": "1111ccc", "toAddr": "1111aaa", "amount": 5,
                         "retUnforgeable": {"sends": [], "receives": [], "news": [], "exprs": [],
                                            "matches": [], "unforgeables": [], "bundles": [],
                                            "connectives": [], "locally_free": null,
                                            "connective_used": false},
                         "failReason": "Insufficient funds"},
         "transactionType": {"Refund": {"deployId": "bb22"}}},
        {"transaction": {"fromAddr": "1111ddd", "toAddr": "1111eee", "amount": 0,
                         "retUnforgeable": {"sends": [], "receives": [], "news": [], "exprs": [],
                                            "matches": [], "unforgeables": [], "bundles": [],
                                            "connectives": [], "locally_free": null,
                                            "connective_used": false},
                         "failReason": null},
         "transactionType": {"CloseBlock": {"blockHash": "cc33"}}}
    ]}"#;

    #[test]
    fn a_reply_parses_the_transfers_and_ignores_the_par() {
        let v: Value = serde_json::from_str(REPLY).unwrap();
        let ts = parse_transfers(&v).unwrap();
        assert_eq!(ts.len(), 3);
        assert_eq!(ts[0].from_addr, "1111aaa");
        assert_eq!(ts[0].amount, 250000000);
        assert_eq!(ts[0].kind, "UserDeploy");
        assert_eq!(ts[0].ref_id.as_deref(), Some("aa11"));
        assert_eq!(ts[0].fail_reason, None);
        // A failed transfer keeps its reason...
        assert_eq!(ts[1].fail_reason.as_deref(), Some("Insufficient funds"));
        assert_eq!(ts[1].kind, "Refund");
        // ...and a system type names a block, not a deploy.
        assert_eq!(ts[2].kind, "CloseBlock");
        assert_eq!(ts[2].ref_id.as_deref(), Some("cc33"));
    }

    #[test]
    fn a_page_reply_is_capped_and_carries_the_walk_fields() {
        let v: Value = serde_json::from_str(REPLY).unwrap();
        let mut ts = parse_transfers(&v).unwrap();
        ts[0].block_hash = "ab".into();
        ts[0].block_number = 412;
        let n = transfers_to_norm(&ts);
        let rows = n.as_coll(k1ndl1ng_norm::CollKind::List).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].map_get("blockNumber").and_then(|x| x.as_int()), Some(412));
        assert_eq!(rows[0].map_get("from").and_then(|x| x.as_str()), Some("1111aaa"));
        assert_eq!(rows[0].map_get("kind").and_then(|x| x.as_str()), Some("UserDeploy"));
        // An absent reason is `Nil`, not the string "nil".
        assert!(rows[0].map_get("failReason").unwrap().is_nil());
        assert_eq!(rows[1].map_get("failReason").and_then(|x| x.as_str()), Some("Insufficient funds"));
    }

    #[test]
    fn windows_walk_backwards_and_clamp_at_height_one() {
        // One window, newest first.
        assert_eq!(windows(100, 20, 50), vec![(81, 100)]);
        // The chain is shorter than the request: what exists is what is asked.
        assert_eq!(windows(5, 20, 50), vec![(1, 5)]);
        // Deeper than one window, still newest first — and the last window is
        // only as wide as what is left to walk, not padded out to `window`.
        assert_eq!(windows(100, 60, 50), vec![(51, 100), (41, 50)]);
        let walked: i64 = windows(100, 60, 50).iter().map(|(a, b)| b - a + 1).sum();
        assert_eq!(walked, 60);
        // The client cap holds even when the caller asks for more.
        assert_eq!(windows(1000, 10_000, 50).len(), (HISTORY_MAX_BLOCKS as usize).div_ceil(50));
        let all: i64 = windows(1000, 10_000, 50).iter().map(|(a, b)| b - a + 1).sum();
        assert_eq!(all, i64::from(HISTORY_MAX_BLOCKS));
        // A window never runs past the tip or below height 1.
        for (a, b) in windows(1000, 10_000, 50) {
            assert!(a >= 1 && b <= 1000 && a <= b);
        }
    }
}
