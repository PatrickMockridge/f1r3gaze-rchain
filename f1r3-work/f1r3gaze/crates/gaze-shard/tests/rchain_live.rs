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
use gaze_shard::{Bridge, KeyPayer, Node, NodeDialect, ShardConfig, SiteAddr};
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!("gaze-rchain-live-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&dir);
    let blobs = Arc::new(Blobs::new(ContentCache::new(dir.join("blobs"), 1 << 20), Http::new()));
    let b = Bridge::new(
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
        Arc::clone(&blobs),
    );
    // The on-chain blob source, registered exactly as the shell registers it —
    // so a published site's files are fetched through this client's own path
    // and not read back out of the directory they came from.
    blobs.add_source(Arc::new(gaze_shard::DriveSource {
        bridge: Arc::clone(&b),
        root: gaze_shard::site::DRIVE_ROOT.into(),
    }));
    b
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

/// The chain and staking reads against a live node.
#[test]
fn the_rchain_reads_answer_a_live_node() {
    use gaze_shard::chain::Blocks;
    let Some((base, key)) = base_and_key() else {
        eprintln!("RCHAIN_NODE unset; skipping the live rchain reads test");
        return;
    };
    let b = bridge(&base, key.clone());
    let node = Node::new(&base, NodeDialect::Rchain, Http::new());

    let (anchor, num) = node.last_finalized().expect("an anchor");
    let (_, bi) = b.block(&anchor).expect("block");
    assert_eq!(bi.block_info.block_number, num, "the block read returns the anchor");
    let (_, bs) = b.blocks(Blocks::Depth(5)).expect("blocks");
    assert!(!bs.is_empty());
    assert!(bs[0].block_number <= num + 1, "the newest block is at or just past the anchor");

    // Finality is a *local view*, and a single-validator net finalizes nothing,
    // so assert only that the read answers -- never what it says.
    assert!(b.is_finalized(&anchor).is_ok());

    let (_, caps) = b.capabilities().expect("capabilities");
    eprintln!("capabilities: faucet={} adminHttp={} devMode={}", caps.faucet, caps.admin_http, caps.dev_mode);
    let (_, s) = b.shards().expect("shards");
    assert_eq!(s.primary_shard, "/root");
    assert!(b.pool().is_ok(), "the mempool read answers, empty or not");

    let (_, ps) = b.pos_status().expect("pos status");
    eprintln!("pos: epoch {} length {} quarantine {}", ps.epoch, ps.epoch_length, ps.quarantine_length);
    assert!(ps.epoch_length >= 1, "epoch length {}", ps.epoch_length);

    // The native reads run under an exploratory deploy.
    let (_, bonds) = b.pos_bonds().expect("bonds");
    assert!(bonds.as_coll(k1ndl1ng_norm::CollKind::Map).is_some(), "getBonds answers a map");
    assert!(b.pos_active_validators().is_ok());
    assert!(b.pos_trusted().is_ok());

    // A delegator with no positions answers an empty list, which is a true
    // answer -- so assert the call, not the count.
    let me = gaze_net::hex(&public_key(&key));
    let (_, d) = b.pos_delegations(&me).expect("delegations");
    eprintln!("delegations for {me}: {}", d.len());
    let (_, dn) = b.pos_delegations_native(&me).expect("native delegations");
    eprintln!("native delegations: {}", k1ndl1ng_norm::show(&dn));
}

/// The staking write, proven **without mutating** the dev chain.
///
/// A refusal is the strongest assertion available here, not a consolation: the
/// reason only arrives if the deploy was accepted, the node ran the contract,
/// the result was read back from `rho:rchain:deployId`, and the `(Bool, ...)`
/// reply was parsed. A successful bond would prove the same path and *change
/// validator state mid-suite* — the payer here is the bonded validator — so the
/// refusal is the one to take.
///
/// `withdraw` is not probed live for the mirror reason: it would either stage a
/// real unbond on the bonded validator, or run from a fresh key whose vault is
/// empty, in which case the deploy dies at pre-charge and never reaches the
/// contract at all. Its path is covered offline
/// (`a_pos_write_settles_on_its_result_not_its_status`).
#[test]
fn the_rchain_staking_write_reports_the_nodes_refusal() {
    let Some((base, key)) = base_and_key() else {
        eprintln!("RCHAIN_NODE unset; skipping the live staking-write test");
        return;
    };
    let b = bridge(&base, key);
    let d = b.pos_bond(1_000_000).expect("the deploy is accepted");
    eprintln!("bond deploy {}", d.id());
    let answer = b.pos_settle(&d.id()).expect("the deploy settles");
    eprintln!("bond as the bonded validator -> {answer:?}");
    let reason = answer.expect_err("an already-bonded key cannot bond again");
    assert!(reason.contains("already bonded"), "unexpected refusal: {reason}");
}

/// The delegation writes, proven **without mutating** the dev chain.
///
/// The first probe is the informative one: the node checks self-delegation
/// *before* any other gate, so `A key cannot delegate to itself` can only be
/// produced once the operator key was parsed out of the term and the contract
/// was reached — which is the named-key path that `bond`/`withdraw` cannot
/// exercise, since they name no key at all.
///
/// A *successful* delegation is available on this chain (the payer's vault is
/// funded and the genesis bonds are pooled) and is deliberately **not** run: it
/// would put a delegation on an operator's bond entry, after which that
/// operator's `withdraw` is refused for the rest of the suite. The success path
/// is covered offline (`a_delegation_settles_on_its_result`).
#[test]
fn the_rchain_delegation_writes_report_the_nodes_refusal() {
    let Some((base, key)) = base_and_key() else {
        eprintln!("RCHAIN_NODE unset; skipping the live delegation test");
        return;
    };
    let me = gaze_net::hex(&public_key(&key));
    let b = bridge(&base, key);

    let d = b.pos_delegate(&me, 1_000_000).expect("the deploy is accepted");
    eprintln!("delegate deploy {}", d.id());
    let reason = b.pos_settle(&d.id()).expect("settles").expect_err("self-delegation is refused");
    eprintln!("delegate to itself -> {reason}");
    assert!(reason.contains("delegate to itself"), "unexpected refusal: {reason}");

    let d2 = b.pos_undelegate(&me).expect("the deploy is accepted");
    let reason2 = b.pos_settle(&d2.id()).expect("settles").expect_err("there is nothing to withdraw");
    eprintln!("undelegate with no delegation -> {reason2}");
    assert!(reason2.contains("no delegation"), "unexpected refusal: {reason2}");
}

/// The publish round trip: publish a manifest, then read it back through this
/// client's own resolution path.
///
/// That is the whole loop on rnode — the term parses, the send persists a datum
/// on the public channel the address names, and `resolve_site` finds it again —
/// and it is non-destructive: one new channel value, plus phlo.
#[test]
fn publishing_a_site_round_trips_on_a_live_node() {
    use gaze_shard::site::manifest_for_dir;
    let Some((base, key)) = base_and_key() else {
        eprintln!("RCHAIN_NODE unset; skipping the live publish test");
        return;
    };
    let b = bridge(&base, key);

    // A one-file site, packaged from a directory.
    let dir = std::env::temp_dir().join(format!("gaze-publish-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("index.html"), b"<h1>gaze</h1>").unwrap();
    // No mirror on purpose: the bytes must come from the chain, or this proves
    // nothing about the blobs publish.
    let m = manifest_for_dir(&dir, "index.html", &[]).expect("a manifest");

    // A fresh address every run, and not for tidiness: a channel accumulates,
    // and publishing *different* content at an address that already holds a
    // manifest is refused (asserted at the end). A changed site takes a new
    // address, which is what the range field is for.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let addr = SiteAddr::parse(&format!("f1r3://abc0def1/livetest{stamp}@^1")).unwrap();
    let uri = addr.registry_uri();
    let d = b.publish_site(&uri, &m).expect("the deploy is accepted");
    eprintln!("publish deploy {} to {uri}", d.id());
    b.settle(&d.id()).expect("the publish settles");

    // The deploy's *status* and the *state* at a pinned block are answered from
    // different places, and they can lag by a block: a deploy the node reports
    // processed is not yet readable at every block the read might pin. So a
    // publisher polls, which is what a publisher does in practice anyway.
    let t0 = Instant::now();
    let mut last = String::new();
    let mut got = None;
    while t0.elapsed() < Duration::from_secs(60) {
        match b.resolve_site(&addr) {
            Ok((rung, back)) => {
                got = Some((rung, back));
                break;
            }
            Err(e) => last = e,
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    let (rung, back) = got.unwrap_or_else(|| panic!("the site never resolved: {last}"));
    eprintln!("resolved {uri} at the {} rung", rung.name());
    assert_eq!(back, m, "what resolves is what was published");

    // The manifest makes it *resolve*; the blobs make it *load*. Publish the
    // files on-chain in the reader's layout and then fetch the entry through
    // this client's own blob path.
    let files: Vec<([u8; 32], Vec<u8>)> = m
        .files
        .iter()
        .map(|(n, h)| (*h, std::fs::read(dir.join(n)).unwrap()))
        .collect();
    let bd = b.publish_blobs(&files).expect("the blobs deploy is accepted");
    eprintln!("blobs deploy {}", bd.id());
    b.settle(&bd.id()).expect("the blobs settle");

    let (name, bytes) = b.site_file(&back, "").expect("the entry loads from the chain");
    eprintln!("loaded {name}, {} bytes", bytes.len());
    assert_eq!(name, "index.html");
    assert_eq!(bytes, b"<h1>gaze</h1>", "the bytes came back off the chain");

    // And a *changed* manifest at one address is refused rather than silently
    // picked, which is the other half of the accumulation rule.
    let mut changed = m.clone();
    changed.mirrors = vec!["https://elsewhere.invalid/".into()];
    let cd = b.publish_site(&uri, &changed).expect("the deploy is accepted");
    b.settle(&cd.id()).expect("it settles");
    let e = b.resolve_site(&addr).expect_err("two different manifests at one address");
    eprintln!("changed manifest refused: {e}");
    assert!(e.contains("different values"), "{e}");

    let _ = std::fs::remove_dir_all(&dir);
}
