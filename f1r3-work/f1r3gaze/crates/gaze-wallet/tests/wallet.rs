//! The wallet against the Embers SDK's own behaviour, and against mock
//! Embers servers, honest and not.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use gaze_net::Http;
use gaze_shard::deploy::DeployData;
use gaze_shard::{MemKeystore, Payer};
use gaze_wallet::contract::{Limits, check_transfer, render_transfer};
use gaze_wallet::{Address, Embers, Wallets, file};
use k256::ecdsa::signature::hazmat::PrehashVerifier;
use k256::ecdsa::{Signature, SigningKey, VerifyingKey};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// (private key, address, wallet file, signature over CONTRACT), produced by
/// running the Embers SDK's code (embers-frontend packages/client:
/// Address.fromPublicKey, serializeKey, signContract) with its libraries.
const VECTORS: &[(&str, &str, &str, &str)] = &[
    ("0000000000000000000000000000000000000000000000000000000000000001", "1111PXDQTDEd4XNuX4YWoB6XeL7ssWvhePGD2XmkENkG5sHfAMW9Q", r#"{"keyType":"secp256k1","value":"0000000000000000000000000000000000000000000000000000000000000001","valueFormat":"hex"}"#, "3045022100b707752315d8ae184eb2e761e2f1a3a78596274550d21c03f9ee791998f87a1202200c6bb93971ae04fbc8926f8311098a772b7ca96c949809f7ffeb637400d000b7"),
    ("a0b1c2d3e4f5061728394a5b6c7d8e9fa0b1c2d3e4f5061728394a5b6c7d8e9f", "11112dz5hKK18bRqrfY5puLbKURCjKEhf2KrDDwZDufqiAuVqDrkMS", r#"{"keyType":"secp256k1","value":"A0B1C2D3E4F5061728394A5B6C7D8E9FA0B1C2D3E4F5061728394A5B6C7D8E9F","valueFormat":"hex"}"#, "3045022100f8d9abe4fe7763bf943e92988d8de6b51cbdb9dd11294d8494aedae4a011b76702207b0bdff0de6d85b8b7107e838537be6c2ae87be7e1c77c47a3a4c425ffbc1b26"),
    ("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364140", "11112oK2fv3B4ZHyjVgT5CsahvRjEeJrjpZ4DSrbGqp9xo5GHHTafg", r#"{"keyType":"secp256k1","value":"FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364140","valueFormat":"hex"}"#, "3045022100dbbc911b2ceb229a9211fe36e68c75b70bb1110a0462682c4df9c347dd4603fd0220698f2ebed0a568874641e252303d479d068bcbf1dd3e01bfd0cb7364b660edd3")
];
const CONTRACT: &[u8] = b"gaze-wallet test contract";

fn key(h: &str) -> SigningKey {
    SigningKey::from_slice(&gaze_net::unhex(h).unwrap()).unwrap()
}

#[test]
fn addresses_files_and_signatures_match_the_sdk() {
    for (k, addr, f, sig) in VECTORS {
        let sk = key(k);
        assert_eq!(Address::from_key(sk.verifying_key()).as_str(), *addr);
        assert!(Address::parse(addr).is_ok());
        // The file F1R3Sky writes, byte for byte, and it reads back.
        assert_eq!(file::serialize(&sk), *f);
        assert_eq!(file::deserialize(f).unwrap().to_bytes(), sk.to_bytes());
        // Both sides sign deterministically (RFC 6979), so the bytes agree.
        assert_eq!(gaze_net::hex(&gaze_shard::deploy::sign_bytes(&sk, CONTRACT).unwrap()), *sig);
    }
    // The SDK's own test address.
    assert!(Address::parse("1111NypGkNrhxpLKFwiZ8gLKmiwLQUyzuEe1p3nEKQCSKMvd1YHY3").is_ok());
    assert!(Address::parse("1111NypGkNrhxpLKFwiZ8gLKmiwLQUyzuEe1p3nEKQCSKMvd1YHY4").is_err());
    assert!(Address::parse("invalid").is_err());
    // Lower-case and bare hex import too; junk does not.
    assert!(file::deserialize(&VECTORS[1].0.to_lowercase()).is_ok());
    assert!(file::deserialize(r#"{"keyType":"ed25519","value":"00","valueFormat":"hex"}"#).is_err());
    assert!(file::deserialize("{}").is_err());
}

fn addr(i: usize) -> Address {
    Address::parse(VECTORS[i].1).unwrap()
}

fn prepared(term: &str, limit: i64) -> Vec<u8> {
    DeployData {
        term: term.into(),
        timestamp: 1_759_000_000_000,
        phlo_price: 1,
        phlo_limit: limit,
        valid_after_block_number: 42,
        shard_id: "root".into(),
        expiration_timestamp: None,
    }
    .signing_bytes()
}

#[test]
fn the_wallet_signs_only_the_transfer_it_was_asked_for() {
    let (a, b, c) = (addr(0), addr(1), addr(2));
    let lim = Limits::default();
    let good = render_transfer("rho:id:abc123xyz", 1_759_000_000, &a, &b, 250, Some("rent \"sept\""));
    let ok = check_transfer(&prepared(&good, 5_000_000), &a, &b, 250, Some("rent \"sept\""), &lim).unwrap();
    assert_eq!(ok.env_uri, "rho:id:abc123xyz");
    // Another recipient, another amount, another description: refused.
    let t = render_transfer("rho:id:abc123xyz", 1, &a, &c, 250, Some("rent \"sept\""));
    assert!(check_transfer(&prepared(&t, 5_000_000), &a, &b, 250, Some("rent \"sept\""), &lim).is_err());
    let t = render_transfer("rho:id:abc123xyz", 1, &a, &b, 25_000, Some("rent \"sept\""));
    assert!(check_transfer(&prepared(&t, 5_000_000), &a, &b, 250, Some("rent \"sept\""), &lim).is_err());
    let t = render_transfer("rho:id:abc123xyz", 1, &a, &b, 250, None);
    assert!(check_transfer(&prepared(&t, 5_000_000), &a, &b, 250, Some("x"), &lim).is_err());
    // Extra code smuggled in, or a different call: refused.
    let smuggled = good.replace("walletsCh in {", "walletsCh, deployerId(`rho:rchain:deployerId`) in {");
    assert!(check_transfer(&prepared(&smuggled, 5_000_000), &a, &b, 250, Some("rent \"sept\""), &lim).is_err());
    let appended = format!("{good} | @\"x\"!(1)");
    assert!(check_transfer(&prepared(&appended, 5_000_000), &a, &b, 250, Some("rent \"sept\""), &lim).is_err());
    // A fee bound beyond the limit, or another shard: refused.
    assert!(check_transfer(&prepared(&good, 50_000_000), &a, &b, 250, Some("rent \"sept\""), &lim).is_err());
    let mut other = DeployData::decode(&prepared(&good, 5_000_000)).unwrap();
    other.shard_id = "elsewhere".into();
    assert!(check_transfer(&other.signing_bytes(), &a, &b, 250, Some("rent \"sept\""), &lim).is_err());
    // Signer fields or unknown fields hidden in the bytes: refused.
    let mut hidden = prepared(&good, 5_000_000);
    hidden.extend_from_slice(&[0x0a, 0x01, 0x00]); // field 1, deployer
    assert!(check_transfer(&hidden, &a, &b, 250, Some("rent \"sept\""), &lim).is_err());
}

#[test]
fn wallets_are_kept_listed_exported_and_pay() {
    let dir = std::env::temp_dir().join(format!("gaze-wallets-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let ks: Arc<MemKeystore> = Arc::default();
    let w = Wallets::open(&dir, ks.clone(), None);
    assert!(w.payer().unwrap_err().contains("no wallet"));
    let a = w.import(VECTORS[1].2, "from F1R3Sky").unwrap();
    assert_eq!(a.as_str(), VECTORS[1].1);
    let b = w.create("spending").unwrap();
    // The first wallet is the one that pays until another is chosen.
    assert_eq!(w.payer().unwrap().1, a.as_str());
    w.set_active(&b).unwrap();
    assert_eq!(w.payer().unwrap().1, b.as_str());
    // Export is the F1R3Sky file, and survives a restart.
    assert_eq!(w.export(&a).unwrap(), VECTORS[1].2);
    let w2 = Wallets::open(&dir, ks.clone(), None);
    assert_eq!(w2.list().len(), 2);
    assert_eq!(w2.active(), Some(b.clone()));
    assert_eq!(w2.list()[0].0.label, "from F1R3Sky");
    // Removing the active wallet deletes its key and falls back to another.
    w2.remove(&b).unwrap();
    assert_eq!(w2.active(), Some(a.clone()));
    assert!(w2.key(&b).is_err());
    let _ = std::fs::remove_dir_all(dir);
}

// --- a mock Embers ---------------------------------------------------------

struct Mock {
    base: String,
    log: Arc<Mutex<Vec<(String, Value)>>>,
}

/// An Embers that prepares transfers from the template, or, when `evil`,
/// substitutes its own recipient.
fn embers(evil: bool) -> Mock {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let log: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
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
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            log2.lock().unwrap().push((path.clone(), body.clone()));
            let resp = if path.ends_with("/state") {
                json!({"balance": "1000000", "requests": [], "exchanges": [], "boosts": [], "transfers": []})
            } else if path.ends_with("/transfer/prepare") {
                let from = Address::parse(body["from"].as_str().unwrap()).unwrap();
                let mut to = Address::parse(body["to"].as_str().unwrap()).unwrap();
                if evil {
                    to = Address::parse(VECTORS[2].1).unwrap();
                }
                let amount: i64 = body["amount"].as_str().unwrap().parse().unwrap();
                let term = render_transfer("rho:id:envtest1", 1_759_000_000, &from, &to, amount, body["description"].as_str());
                json!({"response": {"contract": B64.encode(prepared(&term, 5_000_000))}, "token": "tok"})
            } else if path.ends_with("/transfer/send") {
                // Verify the signature as the node will.
                let rq = &body["request"];
                let contract = B64.decode(rq["contract"].as_str().unwrap()).unwrap();
                let sig = Signature::from_der(&B64.decode(rq["sig"].as_str().unwrap()).unwrap()).unwrap();
                let vk = VerifyingKey::from_sec1_bytes(&B64.decode(rq["deployer"].as_str().unwrap()).unwrap()).unwrap();
                let h = k1ndl1ng_norm::hash::blake2b_256(&contract).0;
                assert!(vk.verify_prehash(&h, &sig).is_ok(), "bad signature");
                assert_eq!(body["token"], "tok");
                json!({"deploy_id": gaze_net::hex(&sig.to_der().as_bytes().to_vec())})
            } else {
                json!({"message": "no route"})
            };
            let b = resp.to_string();
            let mut s = s;
            let _ = write!(s, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{b}", b.len());
        }
    });
    Mock { base, log }
}

#[test]
fn transfers_through_an_honest_embers() {
    let m = embers(false);
    let w = Wallets::open(
        std::env::temp_dir().join(format!("gaze-w-honest-{}", std::process::id())),
        Arc::new(MemKeystore::default()),
        Some(Embers::new(&m.base, Http::new(), Limits::default())),
    );
    let a = w.import(VECTORS[0].2, "").unwrap();
    assert_eq!(w.state(&a).unwrap().balance, 1_000_000);
    assert_eq!(w.balance(), Some(1_000_000));
    let id = w.transfer(&a, &addr(1), 250, Some("lunch")).unwrap();
    assert!(!id.is_empty());
    let log = m.log.lock().unwrap();
    let send = log.iter().find(|(p, _)| p.ends_with("/transfer/send")).expect("sent");
    // The echo the server's token check needs.
    assert_eq!(send.1["prepare_request"]["amount"], "250");
    assert_eq!(send.1["request"]["sig_algorithm"], "secp256k1");
}

#[test]
fn a_dishonest_embers_gets_nothing_signed() {
    let m = embers(true);
    let w = Wallets::open(
        std::env::temp_dir().join(format!("gaze-w-evil-{}", std::process::id())),
        Arc::new(MemKeystore::default()),
        Some(Embers::new(&m.base, Http::new(), Limits::default())),
    );
    let a = w.import(VECTORS[0].2, "").unwrap();
    let e = w.transfer(&a, &addr(1), 250, None).unwrap_err();
    assert!(e.contains("exactly what you asked for"), "{e}");
    assert!(!m.log.lock().unwrap().iter().any(|(p, _)| p.ends_with("/transfer/send")), "nothing was sent");
}

/// The F1R3Cap derivation *is* rnode's REV address. Verified live: gaze derived
/// this address for the key below, rnode's genesis accepted it in `wallets.txt`
/// and credited the vault, and a deploy signed by that key then finalized — so
/// the two derivations name the same vault. `rholang/src/util/rev_address.rs`
/// derives the same prefix (`0x00000000`), key hash
/// (`keccak256(keccak256(pk[1..])[12..])`), 4-byte BLAKE2b-256 checksum and
/// Bitcoin-alphabet base58.
#[test]
fn the_address_is_rnodes_rev_address() {
    let a = Address::from_key(key("d639c2e0dfce4b910b80b3dd052d05290796e486e167c0403aab82eb2e8047f8").verifying_key());
    assert_eq!(a.as_str(), "11112ptoW26K6rXCHe1GLKocEFP5mkrNrRqP66ozGT2yqHzvJXTVV");
}
