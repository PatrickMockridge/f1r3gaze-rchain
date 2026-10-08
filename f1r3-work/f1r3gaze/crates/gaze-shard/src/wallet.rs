//! REV wallet operations on the rchain dialect.
//!
//! `rnode` exposes a **native** `rho:rchain:revVault` system process whose
//! method set is `getBalance` / `transfer` / `findOrCreate` — it does not have
//! the Scala-era `findOrCreate` → vault-handle → `balance` chain, and the
//! balance is address-keyed, so reading it needs no `deployerId`. A transfer's
//! `from` *is* the caller's unforgeable `deployerId`, which is why a transfer
//! has to be a deploy and not an exploratory read.
//!
//! The reply channel differs by path, and this is the trap: an **exploratory
//! deploy** is read from the term's first `new`-bound name, while a **deploy's**
//! result is read from `rho:rchain:deployId`. [`balance_term`] replies on the
//! former; [`transfer_term`] posts to the latter.
//!
//! Terms follow `rchain-community/r-wallet` (`src/utils/rho.ts`), the reference
//! client for this node.

use k1ndl1ng_norm::Norm;

/// The native REV vault system name.
pub const REV_VAULT: &str = "rho:rchain:revVault";

/// The deploy id channel, where a deploy's result is read from.
pub const DEPLOY_ID: &str = "rho:rchain:deployId";

/// The payer's unforgeable deployer identity.
pub const DEPLOYER_ID: &str = "rho:rchain:deployerId";

/// Phlo a REV transfer is allowed to spend. Transfers run a native process and
/// a few rholang steps; this is the reference client's limit.
pub const TRANSFER_PHLO_LIMIT: i64 = 500_000;

/// `revVault!("getBalance", "<addr>", *balanceCh)`, replying on the term's
/// first `new`-bound name. Address-keyed, so it runs under an exploratory
/// deploy with no `deployerId` bound.
pub fn balance_term(addr: &str) -> String {
    format!(
        "new return, revVault(`{REV_VAULT}`), balanceCh in {{\n  \
         revVault!(\"getBalance\", \"{addr}\", *balanceCh) |\n  \
         for (@balance <- balanceCh) {{ return!(balance) }}\n}}"
    )
}

/// `revVault!("transfer", *deployerId, "<to>", <drops>, *resultCh)`, replying on
/// `rho:rchain:deployId`. The source vault is derived from the signer's
/// `deployerId` on the node, so no `from` address is passed and a deploy can
/// only ever spend its own vault.
pub fn transfer_term(to: &str, drops: i64) -> String {
    format!(
        "new revVault(`{REV_VAULT}`), deployerId(`{DEPLOYER_ID}`), deployId(`{DEPLOY_ID}`), resultCh in {{\n  \
         revVault!(\"transfer\", *deployerId, \"{to}\", {drops}, *resultCh) |\n  \
         for (_ <- resultCh) {{ deployId!((true, \"Transfer submitted\")) }}\n}}"
    )
}

/// The balance a [`balance_term`] reply carries: the `Int` the contract
/// produced. A `String` reply is the node's refusal, surfaced as an error
/// rather than as a number.
pub fn parse_balance(n: &Norm) -> Result<u64, String> {
    match n.as_int() {
        Some(v) if v >= 0 => Ok(v as u64),
        Some(v) => Err(format!("balance is negative: {v}")),
        None => match n.as_str() {
            Some(s) => Err(format!("balance query answered {s}")),
            None => Err("balance query answered neither an integer nor a string".into()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_terms_match_the_reference_client() {
        let b = balance_term("11112ptoW26K6rXCHe1GLKocEFP5mkrNrRqP66ozGT2yqHzvJXTVV");
        assert!(b.contains("getBalance"), "{b}");
        assert!(b.contains("\"11112ptoW26K6rXCHe1GLKocEFP5mkrNrRqP66ozGT2yqHzvJXTVV\""), "{b}");
        assert!(b.contains("rho:rchain:revVault"), "{b}");
        // The exploratory path replies on the term's first `new`-bound name.
        assert!(b.starts_with("new return,"), "{b}");
        // The Scala-era vault-handle chain is not how this node works.
        assert!(!b.contains("findOrCreate"), "{b}");

        let t = transfer_term("11112ptoW26K6rXCHe1GLKocEFP5mkrNrRqP66ozGT2yqHzvJXTVV", 250_000);
        assert!(t.contains("*deployerId"), "{t}");
        assert!(t.contains("250000"), "{t}");
        // A deploy's result is read from its deployId channel.
        assert!(t.contains("deployId!("), "{t}");
    }

    #[test]
    fn the_balance_reply_is_parsed_or_refused() {
        assert_eq!(parse_balance(&Norm::int(42)), Ok(42));
        assert_eq!(parse_balance(&Norm::int(0)), Ok(0));
        assert!(parse_balance(&Norm::int(-1)).is_err());
        assert!(parse_balance(&Norm::str("some refusal")).is_err());
        assert!(parse_balance(&Norm::nil()).is_err());
    }
}
