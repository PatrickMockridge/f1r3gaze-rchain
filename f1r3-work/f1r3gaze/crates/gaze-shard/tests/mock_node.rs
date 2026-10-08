//! The bridge against mock nodes speaking the node's HTTP API.

use gaze_blob::{Blobs, ContentCache};
use gaze_knf::Knf;
use gaze_net::{Http, Pool, digest};
use gaze_shard::deploy::{DeployData, SignedDeploy, verify};
use gaze_shard::*;
use k1ndl1ng_norm::{CollKind, Name, Norm};
use k1ndl1ng_parse::Level;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Log = Arc<Mutex<Vec<(String, String)>>>;

/// A node whose registry answers `value`, finalized at block `num`.
fn mock(value: Value, num: i64) -> (String, Log) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let log: Log = Arc::default();
    let log2 = Arc::clone(&log);
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let mut len = 0;
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                if h.trim().is_empty() {
                    break;
                }
                if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            let body = String::from_utf8(body).unwrap();
            log2.lock().unwrap().push((path.clone(), body));
            let resp = if path.starts_with("/api/last-finalized-block") {
                json!({"blockInfo": {"blockHash": "b1", "blockNumber": num}})
            } else if path.starts_with("/api/registry/") {
                json!({"uri": "u", "data": [value], "blockNumber": num, "blockHash": "b1"})
            } else if path.starts_with("/api/estimate-cost") {
                json!({"cost": 1000, "blockNumber": num, "blockHash": "b1", "deployerIdentity": "x"})
            } else if path.starts_with("/api/deploy-finalization-status/") {
                json!({"state": "Finalized", "rejection_count": 0, "latest_block_hash": "b2"})
            } else if path.starts_with("/api/deploy") {
                json!("Success! DeployId is: ok")
            } else {
                json!({"message": "no route"})
            };
            let b = resp.to_string();
            let mut s = s;
            let _ = write!(s, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{b}", b.len());
        }
    });
    (base, log)
}

/// A node speaking the rchain dialect's HTTP API: no `/api/registry`, no
/// `/api/estimate-cost`, no websocket, the `{expr, block}` envelope on the
/// data-at-name path, a tagged `deploy-status`, and a deploy reply whose id is
/// the signature it received. `finalized` false models a fresh single-validator
/// node, whose fringe is unavailable and which answers `/api/blocks` instead.
fn mock_rchain(value: Value, num: i64, finalized: bool) -> (String, Log) {
    mock_rchain_with(value, num, finalized, false)
}

/// 65 bytes as 130 lowercase hex chars.
const KEY: &str = "04aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// As [`mock_rchain`], with `pos_error` making `/api/v1/pos` answer the 500 the
/// node's own store failure produces.
fn mock_rchain_with(value: Value, num: i64, finalized: bool, pos_error: bool) -> (String, Log) {
    mock_rchain_full(value, num, finalized, pos_error, json!([]))
}

/// As [`mock_rchain_with`], with the `deployResult` a settled deploy carries —
/// a write's answer lives there, not in the status tag.
fn mock_rchain_full(value: Value, num: i64, finalized: bool, pos_error: bool, deploy_result: Value) -> (String, Log) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let log: Log = Arc::default();
    let log2 = Arc::clone(&log);
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let mut len = 0;
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                if h.trim().is_empty() {
                    break;
                }
                if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            let body = String::from_utf8(body).unwrap();
            log2.lock().unwrap().push((path.clone(), body.clone()));
            // A full LightBlockInfo, since the chain reads deserialise it.
            let block = json!({
                "version": 1, "shardId": "/root", "blockHash": "b1", "blockNumber": num,
                "sender": KEY, "seqNum": num, "preStateHash": "p", "postStateHash": "q",
                "justifications": [], "bonds": [{"validator": KEY, "stake": 3}],
                "sigAlgorithm": "secp256k1", "sig": "3045", "blockSize": "512",
                "deployCount": 1, "rejectedDeploys": [], "timestamp": 99
            });
            let (status, resp) = if path.starts_with("/api/last-finalized-block") {
                if finalized {
                    (200, json!({"blockInfo": block}).to_string())
                } else {
                    // A single-validator net never finalizes; the route 400s.
                    (400, json!("Finalized fringe is not available.").to_string())
                }
            } else if path.starts_with("/api/block/") {
                // Before /api/blocks: "/api/block/x" does not start with the
                // latter, but keeping the pair adjacent keeps it obvious.
                (200, json!({"blockInfo": block, "deploys": [deploy_info(num)]}).to_string())
            } else if path.starts_with("/api/blocks") {
                // Covers /api/blocks, /{depth} and /{start}/{end}.
                (200, json!([block]).to_string())
            } else if path.starts_with("/api/is-finalized/") {
                (200, "false".to_string())
            } else if path.starts_with("/api/deploy/") {
                // find-deploy, and it MUST precede the /api/deploy arm below,
                // which would otherwise read it as a submission.
                (200, block.to_string())
            } else if path.starts_with("/api/v1/deploys") {
                (200, json!({"deploys": [{"deployId": "3044aa", "timestamp": 1, "deployer": KEY, "term": "Nil", "phloPrice": 1, "phloLimit": 100, "validAfterBlockNumber": 1}]}).to_string())
            } else if path.starts_with("/api/v1/capabilities") {
                (200, json!({"autopropose": true, "proposeOnDeploy": true, "manualPropose": false, "adminHttp": true, "devMode": true, "faucet": true}).to_string())
            } else if path.starts_with("/api/shards") {
                (200, json!({"primaryShard": "/root", "shardCount": 1, "shards": [{"shardId": "/root", "primary": true, "latestBlockNumber": num}]}).to_string())
            } else if path.starts_with("/api/v1/pos/delegations") {
                // Before /api/v1/pos. The node answers 400 for a malformed key,
                // which is a true answer, not an empty list.
                let k = path.split("delegator=").nth(1).unwrap_or("");
                if k.len() != 130 || !k.bytes().all(|b| b.is_ascii_hexdigit()) {
                    (400, json!({"error": "delegator must be a hex-encoded 65-byte public key"}).to_string())
                } else {
                    (200, json!([{"operator": KEY, "amount": 10, "accruedRewards": 2, "pendingUndelegation": {"deadline": 500, "blocksRemaining": 40}}]).to_string())
                }
            } else if path.starts_with("/api/v1/pos") {
                if pos_error {
                    (500, json!({"error": "pos store unavailable"}).to_string())
                } else {
                    (200, json!({"latestBlockNumber": num, "epochLength": 100, "quarantineLength": 10, "epoch": 0, "blocksUntilEpochBoundary": 5, "activeValidators": [KEY], "pendingWithdrawals": []}).to_string())
                }
            } else if path.starts_with("/api/status") {
                // The wallet reads the shard id here rather than guessing it.
                (200, json!({"shardId": "/root", "latestBlockNumber": num, "minPhloPrice": 1}).to_string())
            } else if path.starts_with("/api/faucet") {
                (200, json!({"deployId": "faucet-id", "amount": 30_000_000, "to": "x"}).to_string())
            } else if path.starts_with("/api/data-at-name-by-block-hash")
                || path.starts_with("/api/explore-deploy-by-block-hash")
            {
                (200, json!({"expr": [value], "block": block}).to_string())
            } else if path.starts_with("/api/v1/deploy-status/") {
                (200, json!({"ProcessedWithSuccess": {"deployResult": deploy_result.clone(), "block": block}}).to_string())
            } else if path.starts_with("/api/deploy") {
                // Echo the signature the client signed, so the id must match.
                let sig = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| v.get("signature").and_then(|s| s.as_str()).map(str::to_string))
                    .unwrap_or_default();
                (200, json!(format!("Success!\nDeployId is: {sig}")).to_string())
            } else {
                (200, json!({"message": "no route"}).to_string())
            };
            let mut s = s;
            let reason = match status {
                400 => "Bad Request",
                500 => "Internal Server Error",
                _ => "OK",
            };
            let _ = write!(s, "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{resp}", resp.len());
        }
    });
    (base, log)
}

/// The deploy metadata a block carries.
fn deploy_info(_num: i64) -> Value {
    json!({"deployer": KEY, "term": "Nil", "timestamp": 1, "sig": "3044aa", "sigAlgorithm": "secp256k1",
           "phloPrice": 1, "phloLimit": 100, "validAfterBlockNumber": 1, "cost": 42, "errored": false, "systemDeployError": ""})
}

fn manifest() -> Value {
    json!({"ExprMap": {"data": {
        "gaze": {"ExprInt": {"data": 1}},
        "entry": {"ExprString": {"data": "index.html"}},
        "files": {"ExprMap": {"data": {"index.html": {"ExprBytes": {"data": "11".repeat(32)}}}}},
        "mirrors": {"ExprList": {"data": []}}}}})
}

fn bridge(observers: Vec<String>, validator: String, dir: &str) -> Arc<Bridge> {
    let d = std::env::temp_dir().join(format!("gaze-shard-{dir}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let blobs = Arc::new(Blobs::new(ContentCache::new(d.join("blobs"), 1 << 20), Http::new()));
    Bridge::new(
        ShardConfig {
            observers,
            validator,
            quorum: 2,
            ..Default::default()
        },
        Http::new(),
        Pool::new(2),
        Arc::new(KeyPayer {
            key: k256::ecdsa::SigningKey::from_slice(&[7u8; 32]).unwrap(),
            address: "1111test".into(),
        }),
        blobs,
    )
}

/// A bridge on one dialect, with the fixed test payer.
fn bridge_dialect(dialect: NodeDialect, observers: Vec<String>, validator: String) -> Arc<Bridge> {
    let d = std::env::temp_dir().join(format!("gaze-shard-{}-{}", dialect.name(), std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let blobs = Arc::new(Blobs::new(ContentCache::new(d.join("blobs"), 1 << 20), Http::new()));
    Bridge::new(
        ShardConfig {
            dialect,
            observers,
            validator,
            quorum: 1,
            ..Default::default()
        },
        Http::new(),
        Pool::new(2),
        Arc::new(KeyPayer {
            key: k256::ecdsa::SigningKey::from_slice(&[7u8; 32]).unwrap(),
            address: "1111test".into(),
        }),
        blobs,
    )
}

#[test]
fn quorum_disagreement_and_freshness() {
    let (a, _) = mock(manifest(), 10);
    let (b, _) = mock(manifest(), 10);
    let (c, _) = mock(json!({"ExprInt": {"data": 666}}), 10);
    let br = bridge(vec![a.clone(), b.clone(), c], a.clone(), "q");
    let addr = SiteAddr::parse("f1r3://abcd/todo@^1/index.html").unwrap();
    let (rung, m) = br.resolve_site(&addr).unwrap();
    assert_eq!(rung, Rung::Quorum, "two of three agree; the liar is outvoted");
    assert_eq!(m.entry, "index.html");

    let (d, _) = mock(json!({"ExprInt": {"data": 1}}), 10);
    let (e, _) = mock(json!({"ExprInt": {"data": 2}}), 10);
    let split = bridge(vec![d.clone(), e], d, "split");
    assert!(split.lookup("rho:id:x").unwrap_err().contains("disagree"));

}

#[test]
fn freshness_rejects_rollback_on_one_bridge() {
    // One bridge whose observers roll back from block 12 to block 5.
    let num = Arc::new(Mutex::new(12i64));
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let n2 = Arc::clone(&num);
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                if h.trim().is_empty() {
                    break;
                }
            }
            let n = *n2.lock().unwrap();
            let resp = if line.contains("last-finalized") {
                json!({"blockInfo": {"blockHash": "b", "blockNumber": n}})
            } else {
                json!({"data": [manifest()], "blockNumber": n, "blockHash": "b"})
            };
            let b = resp.to_string();
            let mut s = s;
            let _ = write!(s, "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{b}", b.len());
        }
    });
    let br = bridge(vec![base.clone()], base, "rollback");
    let addr = SiteAddr::parse("f1r3://abcd/todo/").unwrap();
    assert_eq!(br.resolve_site(&addr).unwrap().0, Rung::Node);
    *num.lock().unwrap() = 5;
    assert!(br.resolve_site(&addr).unwrap_err().contains("stale"));
}

#[test]
fn deploys_wait_for_the_user_and_are_signed_correctly() {
    let (obs, _) = mock(manifest(), 10);
    let (val, vlog) = mock(manifest(), 10);
    let br = bridge(vec![obs], val, "deploy");
    // Publish a program into the blob cache, as a site would.
    let k = Knf::from_source("for (@x <- args) { stdout!(x) }", Level::K1G, &[("args", "rho:gaze:args"), ("stdout", "rho:io:stdout")]).unwrap();
    let bytes = k.encode();
    let h = digest(&bytes);
    br.blobs.cache.put(&h, &bytes).unwrap();

    let woke = Arc::new(Mutex::new(0));
    let w2 = Arc::clone(&woke);
    let mut svc = ShardService::new(Arc::clone(&br), "f1r3://abcd/todo", Arc::new(move || *w2.lock().unwrap() += 1));
    let ret = Name::Unforgeable([5; 32]);
    svc.request(
        "rho:gaze:shard",
        &[Norm::str("deploy"), Norm::bytes(&h), Norm::list(vec![Norm::str("hi")]), Norm::map(vec![]), Norm::eval(ret.clone())],
    );
    let t0 = Instant::now();
    while svc.prompts().is_empty() && t0.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let p = svc.prompts().pop().expect("a prompt");
    assert!(p.text.contains("1000 phlo"), "{}", p.text);
    assert!(!vlog.lock().unwrap().iter().any(|(p, _)| p == "/api/deploy"), "nothing deployed before consent");
    svc.answer(p.id, true);
    let mut outs = Vec::new();
    while outs.len() < 2 && t0.elapsed() < Duration::from_secs(20) {
        outs.extend(svc.drain());
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(outs.len(), 2, "one reply with the id, one with the outcome");
    let ShardOut::Reply { datum, .. } = &outs[1] else { panic!() };
    let m = &datum.as_coll(CollKind::Tuple).unwrap()[2];
    assert_eq!(m.map_get("status").and_then(|s| s.as_str()), Some("Finalized"));

    // The body the validator received verifies exactly as the node checks it.
    let (_, body) = vlog.lock().unwrap().iter().find(|(p, _)| p == "/api/deploy").cloned().unwrap();
    let v: Value = serde_json::from_str(&body).unwrap();
    let d = &v["data"];
    let sd = SignedDeploy {
        data: DeployData {
            term: d["term"].as_str().unwrap().into(),
            timestamp: d["timestamp"].as_i64().unwrap(),
            phlo_price: d["phloPrice"].as_i64().unwrap(),
            phlo_limit: d["phloLimit"].as_i64().unwrap(),
            valid_after_block_number: d["validAfterBlockNumber"].as_i64().unwrap(),
            shard_id: d["shardId"].as_str().unwrap().into(),
            expiration_timestamp: d["expiration_timestamp"].as_i64(),
        },
        deployer: gaze_net::unhex(v["deployer"].as_str().unwrap()).unwrap(),
        sig: gaze_net::unhex(v["signature"].as_str().unwrap()).unwrap(),
        dialect: NodeDialect::F1r3fly,
    };
    assert!(verify(&sd));
    assert!(sd.data.term.contains("\"hi\"") && sd.data.term.contains("`rho:io:stdout`"), "{}", sd.data.term);
    assert_eq!(sd.data.phlo_limit, 1000 * 3 / 2 + 10_000);
    assert_eq!(sd.data.valid_after_block_number, 10);
}

#[test]
fn declined_deploys_and_unknown_programs_answer_errors() {
    let (obs, _) = mock(manifest(), 10);
    let br = bridge(vec![obs.clone()], obs, "decline");
    let mut svc = ShardService::new(br, "https://a.example:443", Arc::new(|| {}));
    svc.request("rho:gaze:shard", &[Norm::str("deploy"), Norm::bytes(&[9; 32]), Norm::list(vec![]), Norm::eval(Name::Unforgeable([1; 32]))]);
    let t0 = Instant::now();
    let mut outs = Vec::new();
    while outs.is_empty() && t0.elapsed() < Duration::from_secs(10) {
        outs.extend(svc.drain());
        std::thread::sleep(Duration::from_millis(20));
    }
    let ShardOut::Reply { datum, .. } = &outs[0] else { panic!() };
    assert_eq!(datum.as_coll(CollKind::Tuple).unwrap()[0].as_str(), Some("err"));
    svc.request("rho:gaze:shard", &[Norm::str("proof"), Norm::eval(Name::Unforgeable([1; 32]))]);
    let o = svc.drain();
    let ShardOut::Reply { datum, .. } = &o[0] else { panic!() };
    assert_eq!(datum.as_coll(CollKind::Tuple).unwrap()[1].as_str(), Some("unavailable"));
}

/// The rchain dialect: the no-fringe anchor fallback, the public-channel
/// registry read, and a deploy whose body omits `expiration_timestamp` and
/// names the shard `/root`.
#[test]
fn the_rchain_dialect_reads_and_deploys() {
    // A single-validator node: no finalized fringe, so the anchor falls back
    // to `/api/blocks`.
    let (obs, _) = mock_rchain(manifest(), 10, false);
    let (val, vlog) = mock_rchain(manifest(), 10, true);
    let cfg = ShardConfig {
        dialect: NodeDialect::Rchain,
        shard_id: "/root".into(),
        observers: vec![obs],
        validator: val,
        quorum: 2,
        ..Default::default()
    };
    let d = std::env::temp_dir().join(format!("gaze-shard-rchain-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let blobs = Arc::new(Blobs::new(ContentCache::new(d.join("blobs"), 1 << 20), Http::new()));
    let br = Bridge::new(
        cfg,
        Http::new(),
        Pool::new(2),
        Arc::new(KeyPayer { key: k256::ecdsa::SigningKey::from_slice(&[7u8; 32]).unwrap(), address: "1111test".into() }),
        blobs,
    );

    // A site resolves through the public-channel read, at the fallback block.
    let addr = SiteAddr::parse("f1r3://abcd/todo@^1/index.html").unwrap();
    let (rung, m) = br.resolve_site(&addr).unwrap();
    assert_eq!(rung, Rung::Node);
    assert_eq!(m.entry, "index.html");

    // A deploy: rchain has no estimate endpoint, so the prompt quotes the bound.
    let k = Knf::from_source("for (@x <- args) { stdout!(x) }", Level::K1G, &[("args", "rho:gaze:args"), ("stdout", "rho:io:stdout")]).unwrap();
    let bytes = k.encode();
    let h = digest(&bytes);
    br.blobs.cache.put(&h, &bytes).unwrap();
    let mut svc = ShardService::new(Arc::clone(&br), "f1r3://abcd/todo", Arc::new(|| {}));
    let ret = Name::Unforgeable([5; 32]);
    svc.request(
        "rho:gaze:shard",
        &[Norm::str("deploy"), Norm::bytes(&h), Norm::list(vec![Norm::str("hi")]), Norm::map(vec![]), Norm::eval(ret.clone())],
    );
    let t0 = Instant::now();
    while svc.prompts().is_empty() && t0.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let p = svc.prompts().pop().expect("a prompt");
    assert!(p.text.contains("Up to 250000 phlo"), "{}", p.text);
    svc.answer(p.id, true);
    let mut outs = Vec::new();
    while outs.len() < 2 && t0.elapsed() < Duration::from_secs(20) {
        outs.extend(svc.drain());
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(outs.len(), 2, "one reply with the id, one with the outcome");
    let ShardOut::Reply { datum, .. } = &outs[1] else { panic!() };
    assert_eq!(datum.as_coll(CollKind::Tuple).unwrap()[2].map_get("status").and_then(|s| s.as_str()), Some("Finalized"));

    // The body the validator received: no field 13, shard `/root`, and the
    // signature verifies under the rchain preimage.
    let (_, body) = vlog.lock().unwrap().iter().find(|(p, _)| p == "/api/deploy").cloned().unwrap();
    let v: Value = serde_json::from_str(&body).unwrap();
    let d = &v["data"];
    assert!(d.get("expiration_timestamp").is_none(), "rchain omits field 13: {d}");
    assert_eq!(d["shardId"], "/root");
    let sd = SignedDeploy {
        data: DeployData {
            term: d["term"].as_str().unwrap().into(),
            timestamp: d["timestamp"].as_i64().unwrap(),
            phlo_price: d["phloPrice"].as_i64().unwrap(),
            phlo_limit: d["phloLimit"].as_i64().unwrap(),
            valid_after_block_number: d["validAfterBlockNumber"].as_i64().unwrap(),
            shard_id: d["shardId"].as_str().unwrap().into(),
            expiration_timestamp: None,
        },
        deployer: gaze_net::unhex(v["deployer"].as_str().unwrap()).unwrap(),
        sig: gaze_net::unhex(v["signature"].as_str().unwrap()).unwrap(),
        dialect: NodeDialect::Rchain,
    };
    assert!(verify(&sd), "the rchain preimage must verify");
}

/// The rchain wallet's wire, offline: a balance read through the native
/// `revVault`, a transfer whose term names the payer's `deployerId`, and the
/// faucet. (The live counterpart is `rchain_live.rs`.)
#[test]
fn the_rchain_wallet_reads_and_moves_rev() {
    const TO: &str = "11112VYAt8rUGNRRZX3eJdgagaAhtWTK8Js7F7X5iqddMVqyDTtYau";
    // The mock answers every exploratory read with 12345.
    let (obs, _) = mock_rchain(json!({"ExprInt": {"data": 12345}}), 10, true);
    let br = bridge_dialect(NodeDialect::Rchain, vec![obs.clone()], obs);

    let (rung, drops) = br.rev_balance(TO).unwrap();
    assert_eq!(drops, 12345);
    assert_eq!(rung, Rung::Node);

    // A transfer is a deploy whose term spends the payer's own vault.
    let d = br.rev_transfer(TO, 1000).unwrap();
    assert!(d.data.term.contains("transfer"), "{}", d.data.term);
    assert!(d.data.term.contains("*deployerId"), "{}", d.data.term);
    assert!(d.data.term.contains(&TO.to_string()), "{}", d.data.term);
    assert_eq!(d.data.shard_id, "/root", "the shard id comes from the node's status");

    let (id, amount) = br.faucet(TO).unwrap();
    assert_eq!(id, "faucet-id");
    assert_eq!(amount, 30_000_000);

    // The f1r3fly dialect routes REV through Embers, not the node.
    let (f, _) = mock(manifest(), 10);
    let fb = bridge_dialect(NodeDialect::F1r3fly, vec![f.clone()], f);
    assert!(fb.rev_transfer(TO, 1000).unwrap_err().contains("Embers"));
    // The balance read was ungated before this change.
    assert!(fb.rev_balance(TO).unwrap_err().contains("rchain"), "a balance read must not reach f1r3fly");
}

// --- the rchain read surface ------------------------------------------------

/// The chain reads: identifier-addressed ones grade, head-relative ones are one
/// observer's and say so.
#[test]
fn the_rchain_chain_reads_answer_and_grade() {
    let (a, _) = mock_rchain(manifest(), 10, true);
    let (b, _) = mock_rchain(manifest(), 10, true);
    let br = bridge_dialect(NodeDialect::Rchain, vec![a.clone(), b], a.clone());

    // A hash-addressed read: every observer must agree.
    let (rung, bi) = br.block("b1").unwrap();
    assert_eq!(rung, Rung::Quorum);
    assert_eq!(bi.block_info.block_number, 10);
    assert_eq!(bi.deploys.len(), 1);
    assert_eq!(bi.deploys[0].cost, 42);
    assert_eq!(br.find_deploy("3044aa").unwrap().1.block_hash, "b1");

    // Head-relative and node-local reads are one observer's.
    let (rung, bs) = br.blocks(gaze_shard::chain::Blocks::Head).unwrap();
    assert_eq!(rung, Rung::Node);
    assert_eq!(bs.len(), 1);
    assert!(!br.is_finalized("b1").unwrap().1, "a single-validator net finalizes nothing");
    assert!(br.capabilities().unwrap().1.faucet);
    assert_eq!(br.shards().unwrap().1.primary_shard, "/root");
    assert_eq!(br.pool().unwrap().1.len(), 1);
}

/// A transposed depth/range route would still return plausible content, so the
/// only assertion that catches it is on the path actually requested.
#[test]
fn the_block_reads_address_the_right_routes() {
    use gaze_shard::chain::Blocks;
    let (base, log) = mock_rchain(manifest(), 10, true);
    let br = bridge_dialect(NodeDialect::Rchain, vec![base.clone()], base);
    br.blocks(Blocks::Head).unwrap();
    br.blocks(Blocks::Depth(3)).unwrap();
    br.blocks(Blocks::Range(2, 5)).unwrap();
    let paths: Vec<String> = log.lock().unwrap().iter().map(|(p, _)| p.clone()).collect();
    assert!(paths.iter().any(|p| p == "/api/blocks"), "head: {paths:?}");
    assert!(paths.iter().any(|p| p == "/api/blocks/3"), "depth: {paths:?}");
    assert!(paths.iter().any(|p| p == "/api/blocks/2/5"), "range: {paths:?}");
}

#[test]
fn the_rchain_staking_reads_answer() {
    let (base, _) = mock_rchain(manifest(), 10, true);
    let br = bridge_dialect(NodeDialect::Rchain, vec![base.clone()], base);
    let (rung, s) = br.pos_status().unwrap();
    assert_eq!(rung, Rung::Node);
    assert_eq!(s.epoch_length, 100);
    assert_eq!(s.active_validators.len(), 1);
    let (_, ps) = br.pos_delegations(KEY).unwrap();
    assert_eq!(ps.len(), 1);
    assert_eq!(ps[0].amount, 10);
    assert_eq!(ps[0].accrued_rewards, 2);
    assert_eq!(ps[0].pending_undelegation.as_ref().map(|u| u.blocks_remaining), Some(40));
}

/// A node-side failure and a malformed key both reach the caller.
#[test]
fn a_pos_failure_and_a_bad_key_are_surfaced() {
    let (base, _) = mock_rchain_with(manifest(), 10, true, true);
    let br = bridge_dialect(NodeDialect::Rchain, vec![base.clone()], base);
    let e = br.pos_status().unwrap_err();
    assert!(e.contains("pos store unavailable"), "{e}");
    // A malformed key is refused locally -- an empty list is a *true* answer
    // about a delegator with no positions, so it must not be reached by error.
    assert!(br.pos_delegations("04aa").unwrap_err().contains("65 bytes"));
    assert!(br.pos_delegations("zz").unwrap_err().contains("hex"));
    assert!(br.pos_delegations_native("04aa").unwrap_err().contains("65 bytes"));
}

/// The native `getBonds` reply's keys are byte arrays, not hex strings.
#[test]
fn the_native_bonds_read_decodes_its_keys() {
    let (base, _) = mock_rchain(json!({"ExprMap": {"data": {KEY: {"ExprInt": {"data": 7}}}}}), 10, true);
    let br = bridge_dialect(NodeDialect::Rchain, vec![base.clone()], base);
    let (_, bonds) = br.pos_bonds().unwrap();
    let pair = bonds.as_coll(CollKind::Map).unwrap();
    let len = pair[0].as_lit().and_then(|l| match l {
        k1ndl1ng_norm::Lit::Bytes(b) => Some(b.len()),
        _ => None,
    });
    assert_eq!(len, Some(65), "a bonds key is 65 bytes, not 130 chars of hex");
    assert_eq!(pair[1].as_int(), Some(7));
}

/// **The guard for the whole design.** An f1r3fly bridge must answer every
/// rchain-only read without the node ever seeing an rchain route or term.
#[test]
fn no_rchain_route_or_term_reaches_an_f1r3fly_node() {
    use gaze_shard::chain::Blocks;
    let (base, vlog) = mock(manifest(), 10);
    let br = bridge_dialect(NodeDialect::F1r3fly, vec![base.clone()], base);

    assert!(br.block("b1").is_err());
    assert!(br.blocks(Blocks::Head).is_err());
    assert!(br.find_deploy("3044aa").is_err());
    assert!(br.is_finalized("b1").is_err());
    assert!(br.pool().is_err());
    assert!(br.capabilities().is_err());
    assert!(br.shards().is_err());
    assert!(br.pos_status().is_err());
    assert!(br.pos_delegations(KEY).is_err());
    assert!(br.pos_bonds().is_err());
    assert!(br.pos_active_validators().is_err());
    assert!(br.pos_trusted().is_err());
    assert!(br.pos_delegations_native(KEY).is_err());
    assert!(br.pos_bond(1).is_err());
    assert!(br.pos_withdraw().is_err());
    assert!(br.pos_settle("x").is_err());
    // The wallet's balance read was ungated before this change and would have
    // shipped a term naming `rho:rchain:revVault` to an f1r3fly node.
    assert!(br.rev_balance(KEY).is_err());

    // The page verbs answer the structured `unavailable`, synchronously.
    let mut svc = ShardService::new(Arc::clone(&br), "https://a.example:443", Arc::new(|| {}));
    let ret = Name::Unforgeable([9; 32]);
    for sub in ["block", "blocks", "find-deploy", "finalized", "pool", "caps", "shards", "pos", "bonds", "validators", "trusted"] {
        svc.request("rho:gaze:shard", &[Norm::str("chain"), Norm::str(sub), Norm::eval(ret.clone())]);
    }
    for sub in ["bond", "withdraw"] {
        svc.request("rho:gaze:shard", &[Norm::str("stake"), Norm::str(sub), Norm::int(1), Norm::eval(ret.clone())]);
    }
    let outs = svc.drain();
    assert_eq!(outs.len(), 13, "one reply per chain read and per stake action");
    for o in &outs {
        let ShardOut::Reply { datum, .. } = o else { panic!() };
        let t = datum.as_coll(CollKind::Tuple).unwrap();
        assert_eq!(t[0].as_str(), Some("err"));
        assert_eq!(t[1].as_str(), Some("unavailable"), "{datum:?}");
    }

    // And nothing rchain-shaped reached the wire.
    let log = vlog.lock().unwrap();
    for (path, _) in log.iter() {
        for p in ["/api/block", "/api/blocks", "/api/v1/pos", "/api/shards", "/api/v1/deploys", "/api/v1/capabilities"] {
            assert!(!path.starts_with(p), "an f1r3fly node was sent {path}");
        }
    }
    for (_, body) in log.iter() {
        for t in ["getBonds", "getActiveValidators", "getTrusted", "getDelegations", "revVault", "rho:rchain:pos", "deployerId"] {
            assert!(!body.contains(t), "an f1r3fly node was sent a term naming {t}: {body}");
        }
    }
}

/// A write settles on its **result**, not its status: a refusal is a
/// *successful* deploy, so the status tag alone cannot tell the two apart.
#[test]
fn a_pos_write_settles_on_its_result_not_its_status() {
    // (true, Nil) -- the node filters the Nil, so this is a one-element tuple.
    let ok = json!([{"ExprTuple": {"data": [{"ExprBool": {"data": true}}]}}]);
    let (base, _) = mock_rchain_full(manifest(), 10, true, false, ok);
    let br = bridge_dialect(NodeDialect::Rchain, vec![base.clone()], base);
    let d = br.pos_bond(1_000_000).unwrap();
    assert_eq!(br.pos_settle(&d.id()).unwrap(), Ok(()), "a successful bond");

    // (false, "reason") -- a refusal, on a deploy that otherwise succeeded.
    let refused = json!([{"ExprTuple": {"data": [
        {"ExprBool": {"data": false}},
        {"ExprString": {"data": "User is not bonded"}}
    ]}}]);
    let (base2, _) = mock_rchain_full(manifest(), 10, true, false, refused);
    let br2 = bridge_dialect(NodeDialect::Rchain, vec![base2.clone()], base2);
    let d2 = br2.pos_withdraw().unwrap();
    assert_eq!(
        br2.pos_settle(&d2.id()).unwrap(),
        Err("User is not bonded".to_string()),
        "the node's own words reach the caller"
    );
}
