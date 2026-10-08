//! The rchain dialect against a live `rnode`.
//!
//! Skipped unless `RCHAIN_NODE` names a running node; `RCHAIN_DEPLOYER_KEY`
//! (32-byte base16) names the account that pays. This is the cross-repo proof:
//! a deploy this crate signs must be the deploy the node verifies, and the
//! wallet must read and move REV through the node's native `revVault`.
//!
//!   RCHAIN_NODE=http://127.0.0.1:40403 \
//!   RCHAIN_DEPLOYER_KEY=<64 hex> \
//!   cargo test -p gaze-shard --test rchain_live -- --nocapture
//!
//! A wrong preimage shows up as `400 Deploy signature is invalid.`, so the
//! deploy assertion below pins the whole signing path.

use gaze_blob::{Blobs, ContentCache};
use gaze_net::{Http, Pool};
use gaze_shard::deploy::{DeployData, public_key, sign_for};
use gaze_shard::{Bridge, KeyPayer, Node, NodeDialect, ShardConfig};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A REV address from the reference client's test vectors
/// (`r-wallet/scripts/test-unit.ts`), used as a transfer recipient.
const RECIPIENT: &str = "11112VYAt8rUGNRRZX3eJdgagaAhtWTK8Js7F7X5iqddMVqyDTtYau";

/// A second address, funded by the faucet, so a drip can never perturb the
/// `before`/`after` the transfer assertion measures (including across runs).
const FAUCET_TO: &str = "11112dz5hKK18bRqrfY5puLbKURCjKEhf2KrDDwZDufqiAuVqDrkMS";

fn base_and_key() -> Option<(String, k256::ecdsa::SigningKey)> {
    let base = std::env::var("RCHAIN_NODE").ok()?;
    let hex = std::env::var("RCHAIN_DEPLOYER_KEY").ok()?;
    let bytes: [u8; 32] = gaze_net::unhex(&hex)?.try_into().ok()?;
    let key = k256::ecdsa::SigningKey::from_slice(&bytes).expect("a secp256k1 key");
    Some((base, key))
}

fn bridge(base: &str, key: k256::ecdsa::SigningKey) -> Arc<Bridge> {
    let dir = std::env::temp_dir().join(format!("gaze-rchain-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let blobs = Arc::new(Blobs::new(ContentCache::new(dir.join("blobs"), 1 << 20), Http::new()));
    Bridge::new(
        ShardConfig {
            dialect: NodeDialect::Rchain,
            observers: vec![base.to_string()],
            validator: base.to_string(),
            quorum: 1,
            ..Default::default()
        },
        Http::new(),
        Pool::new(2),
        Arc::new(KeyPayer { key, address: RECIPIENT.into() }),
        blobs,
    )
}

/// Wait for a deploy to leave `Pending`, returning its terminal state.
fn settle(b: &Bridge, id: &str) -> String {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(40) {
        if let Ok((st, _)) = b.finalization(id)
            && st != "Pending"
        {
            return st;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    "Pending".into()
}

#[test]
fn the_rchain_dialect_deploys_and_reads_against_a_live_node() {
    let Some((base, key)) = base_and_key() else {
        eprintln!("RCHAIN_NODE unset; skipping the live rchain test");
        return;
    };
    let node = Node::new(&base, NodeDialect::Rchain, Http::new());

    // The node names its own shard; the deploy signs whatever it reports.
    assert_eq!(node.shard_id().expect("status"), "/root");

    // 1. The anchoring block. A single-validator net never finalizes, so this
    //    exercises the newest-block fallback.
    let (block, num) = node.last_finalized().expect("an anchoring block");
    eprintln!("anchor block {num} {block}");
    assert!(!block.is_empty() && num > 0);

    // 2. An exploratory deploy, read at that block's post-state. The reply is
    //    read from the term's first `new`-bound name, then `@"out"` (the
    //    exploratory path answers what the term *sent*, not what it evaluated
    //    to), so the probe sends on `@"out"`.
    let (values, at, _cost) = node.explore("new out in { out!(7) }", &block).expect("explore");
    eprintln!("explore -> {values:?} at {at}");
    assert_eq!(values.first().and_then(|v| v.pointer("/ExprInt/data")).and_then(|v| v.as_i64()), Some(7));

    // 3. A deploy this crate signs, submitted to the node.
    let data = DeployData {
        term: "Nil".into(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
        phlo_price: 1,
        phlo_limit: 250_000,
        valid_after_block_number: num,
        shard_id: "/root".into(),
        expiration_timestamp: None,
    };
    let deploy = sign_for(&key, data, NodeDialect::Rchain).expect("sign");
    eprintln!("deployer {} id {}", gaze_net::hex(&public_key(&key)), deploy.id());
    let message = node.deploy(&deploy).expect("the node must accept the signature");
    eprintln!("deploy -> {message}");

    // 4. Its status, which is the node's authoritative word on the deploy.
    let t0 = Instant::now();
    let mut state = String::new();
    while t0.elapsed() < Duration::from_secs(30) {
        if let Ok((st, _)) = node.finalization(&deploy.id())
            && st != "Pending"
        {
            state = st;
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    eprintln!("deploy status {state}");
    assert_eq!(state, "Finalized", "an accepted deploy is included and finalizes");
}

/// The wallet: read a REV balance, move REV to an address, and fund from the
/// faucet — all through the node's native `revVault`.
#[test]
fn the_rchain_wallet_reads_and_moves_rev() {
    let Some((base, key)) = base_and_key() else {
        eprintln!("RCHAIN_NODE unset; skipping the live rchain wallet test");
        return;
    };
    let b = bridge(&base, key);

    let before = b.rev_balance(RECIPIENT).expect("balance").1;
    eprintln!("balance of {RECIPIENT} before: {before}");

    let d = b.rev_transfer(RECIPIENT, 1000).expect("transfer");
    eprintln!("transfer deploy {}", d.id());
    let state = settle(&b, &d.id());
    eprintln!("transfer state: {state}");
    assert_eq!(state, "Finalized", "the transfer must be included and finalize");

    let after = b.rev_balance(RECIPIENT).expect("balance").1;
    eprintln!("balance of {RECIPIENT} after: {after}");
    assert_eq!(after, before + 1000, "the transfer moved exactly 1000 drops");

    // The faucet signs server-side, so it needs no key from us.
    let (id, drops) = b.faucet(FAUCET_TO).expect("faucet");
    eprintln!("faucet deploy {id} funded {drops} drops");
    assert!(drops > 0, "the faucet answers a positive drip");
}
