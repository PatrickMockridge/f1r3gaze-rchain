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
            let block = json!({"blockHash": "b1", "blockNumber": num});
            let (status, resp) = if path.starts_with("/api/last-finalized-block") {
                if finalized {
                    (200, json!({"blockInfo": block}).to_string())
                } else {
                    // A single-validator net never finalizes; the route 400s.
                    (400, json!("Finalized fringe is not available.").to_string())
                }
            } else if path.starts_with("/api/blocks") {
                (200, json!([block]).to_string())
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
                (200, json!({"ProcessedWithSuccess": {"deployResult": [], "block": block}}).to_string())
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
            let reason = if status == 400 { "Bad Request" } else { "OK" };
            let _ = write!(s, "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{resp}", resp.len());
        }
    });
    (base, log)
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
}
