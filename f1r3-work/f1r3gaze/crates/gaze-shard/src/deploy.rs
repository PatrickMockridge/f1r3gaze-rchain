//! Deploy construction and signing, exactly as the node verifies it
//! (`crypto::signatures::Signed::from_signed_data`): the signature is DER
//! ECDSA over secp256k1, on the BLAKE2b-256 prehash of the protobuf encoding
//! of `DeployDataProto` with the signer fields (deployer, sig, sigAlgorithm)
//! empty; the deployer is the 65-byte uncompressed public key.
//!
//! The preimage differs by dialect: F1R3FLY's node carries an
//! `expiration_timestamp` as proto field 13, which rchain-rust's
//! `DeployDataProto` does not define. The field-number set is otherwise
//! identical, so the only difference is whether field 13 is emitted.

use crate::node::NodeDialect;
use k1ndl1ng_norm::hash::blake2b_256;
use k256::ecdsa::signature::hazmat::{PrehashSigner, PrehashVerifier};
use k256::ecdsa::{Signature, SigningKey, VerifyingKey};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeployData {
    pub term: String,
    pub timestamp: i64,
    pub phlo_price: i64,
    pub phlo_limit: i64,
    pub valid_after_block_number: i64,
    pub shard_id: String,
    /// Milliseconds; `None` or `0` means no expiry.
    pub expiration_timestamp: Option<i64>,
}

fn varint(mut v: u64, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn field_varint(n: u32, v: i64, out: &mut Vec<u8>) {
    if v != 0 {
        varint(((n << 3) | 0) as u64, out);
        varint(v as u64, out);
    }
}

fn field_bytes(n: u32, b: &[u8], out: &mut Vec<u8>) {
    if !b.is_empty() {
        varint(((n << 3) | 2) as u64, out);
        varint(b.len() as u64, out);
        out.extend_from_slice(b);
    }
}

impl DeployData {
    /// proto3 encoding of `DeployDataProto` without signer fields, fields in
    /// number order, defaults omitted (what `prost` produces).
    ///
    /// Field 13 (`expiration_timestamp`) is emitted only for the f1r3fly
    /// dialect: rchain-rust's `DeployDataProto` stops at field 12, and a
    /// signature over a preimage that includes field 13 is rejected there.
    pub fn signing_bytes_for(&self, dialect: NodeDialect) -> Vec<u8> {
        let mut o = Vec::new();
        field_bytes(2, self.term.as_bytes(), &mut o);
        field_varint(3, self.timestamp, &mut o);
        field_varint(7, self.phlo_price, &mut o);
        field_varint(8, self.phlo_limit, &mut o);
        field_varint(10, self.valid_after_block_number, &mut o);
        field_bytes(11, self.shard_id.as_bytes(), &mut o);
        if dialect == NodeDialect::F1r3fly {
            field_varint(13, self.expiration_timestamp.unwrap_or(0), &mut o);
        }
        o
    }

    /// The f1r3fly preimage. The Embers-prepared-contract path and its tests
    /// are pinned to this dialect.
    pub fn signing_bytes(&self) -> Vec<u8> {
        self.signing_bytes_for(NodeDialect::F1r3fly)
    }

    /// Decode prepared contract bytes (a `DeployDataProto` without signer
    /// fields), strictly: only the fields [`DeployData::signing_bytes`]
    /// writes, each at most once, and the bytes must be exactly the canonical
    /// encoding, so nothing can hide in what the wallet signs.
    pub fn decode(b: &[u8]) -> Result<DeployData, String> {
        fn var(b: &[u8], i: &mut usize) -> Result<u64, String> {
            let mut v = 0u64;
            for shift in (0..64).step_by(7) {
                let x = *b.get(*i).ok_or("truncated varint")?;
                *i += 1;
                v |= ((x & 0x7f) as u64) << shift;
                if x < 0x80 {
                    return Ok(v);
                }
            }
            Err("varint too long".into())
        }
        let mut d = DeployData {
            term: String::new(),
            timestamp: 0,
            phlo_price: 0,
            phlo_limit: 0,
            valid_after_block_number: 0,
            shard_id: String::new(),
            expiration_timestamp: None,
        };
        let mut seen = 0u32;
        let mut i = 0;
        while i < b.len() {
            let tag = var(b, &mut i)?;
            let (n, wire) = ((tag >> 3) as u32, tag & 7);
            if n >= 32 || seen & (1 << n) != 0 {
                return Err(format!("field {n} repeated or out of range"));
            }
            seen |= 1 << n;
            match (n, wire) {
                (2 | 11, 2) => {
                    let len = var(b, &mut i)? as usize;
                    let end = i.checked_add(len).filter(|e| *e <= b.len()).ok_or("truncated field")?;
                    let t = std::str::from_utf8(&b[i..end]).map_err(|_| "field is not UTF-8")?.to_string();
                    i = end;
                    if n == 2 { d.term = t } else { d.shard_id = t }
                }
                (3 | 7 | 8 | 10 | 13, 0) => {
                    let v = var(b, &mut i)? as i64;
                    match n {
                        3 => d.timestamp = v,
                        7 => d.phlo_price = v,
                        8 => d.phlo_limit = v,
                        10 => d.valid_after_block_number = v,
                        _ => d.expiration_timestamp = Some(v),
                    }
                }
                _ => return Err(format!("unexpected field {n} (wire type {wire}) in a prepared contract")),
            }
        }
        if d.signing_bytes() != b {
            return Err("prepared contract is not in canonical form".into());
        }
        Ok(d)
    }

    pub fn signing_hash_for(&self, dialect: NodeDialect) -> [u8; 32] {
        blake2b_256(&self.signing_bytes_for(dialect)).0
    }

    pub fn signing_hash(&self) -> [u8; 32] {
        self.signing_hash_for(NodeDialect::F1r3fly)
    }
}

#[derive(Clone, Debug)]
pub struct SignedDeploy {
    pub data: DeployData,
    pub deployer: Vec<u8>,
    pub sig: Vec<u8>,
    /// The dialect this was signed for: the two preimages differ by field 13,
    /// so a deploy signed for one node does not verify on the other.
    pub dialect: NodeDialect,
}

pub fn public_key(k: &SigningKey) -> Vec<u8> {
    k.verifying_key().to_encoded_point(false).as_bytes().to_vec()
}

/// The 65-byte uncompressed public key (hex) for a base16 secp256k1 private key.
///
/// A genesis `bonds.txt` names this, and `wallets.txt` names its REV address — two files that have to
/// agree about one key, which is why this is derived rather than pasted.
pub fn public_key_hex(private_hex: &str) -> Result<String, String> {
    let bytes: [u8; 32] = gaze_net::unhex(private_hex)
        .ok_or("the private key is not hex")?
        .try_into()
        .map_err(|_| "a private key is 32 bytes")?;
    let k = SigningKey::from_slice(&bytes).map_err(|e| e.to_string())?;
    Ok(gaze_net::hex(&public_key(&k)))
}

pub fn sign_for(k: &SigningKey, data: DeployData, dialect: NodeDialect) -> Result<SignedDeploy, String> {
    let sig: Signature = k.sign_prehash(&data.signing_hash_for(dialect)).map_err(|e| e.to_string())?;
    let sig = sig.normalize_s().unwrap_or(sig);
    Ok(SignedDeploy {
        deployer: public_key(k),
        sig: sig.to_der().as_bytes().to_vec(),
        data,
        dialect,
    })
}

pub fn sign(k: &SigningKey, data: DeployData) -> Result<SignedDeploy, String> {
    sign_for(k, data, NodeDialect::F1r3fly)
}

/// Sign prepared contract bytes as the Embers SDK's `signContract` does:
/// Blake2b-256 of the bytes, secp256k1, low-S, DER. For a `DeployDataProto`
/// encoding this is exactly the deploy signature the node checks.
pub fn sign_bytes(k: &SigningKey, bytes: &[u8]) -> Result<Vec<u8>, String> {
    let h = blake2b_256(bytes).0;
    let sig: Signature = k.sign_prehash(&h).map_err(|e| e.to_string())?;
    let sig = sig.normalize_s().unwrap_or(sig);
    Ok(sig.to_der().as_bytes().to_vec())
}

/// The node's check, for tests and for the hostile-proxy harness.
pub fn verify(d: &SignedDeploy) -> bool {
    let Ok(vk) = VerifyingKey::from_sec1_bytes(&d.deployer) else { return false };
    let Ok(sig) = Signature::from_der(&d.sig) else { return false };
    vk.verify_prehash(&d.data.signing_hash_for(d.dialect), &sig).is_ok()
}

impl SignedDeploy {
    /// The body of `POST /api/deploy`. The `data` object carries only the
    /// fields the dialect's `DeployDataProto` defines: rchain omits
    /// `expiration_timestamp`, which it does not have.
    pub fn to_json(&self) -> serde_json::Value {
        let mut data = serde_json::json!({
            "term": self.data.term,
            "timestamp": self.data.timestamp,
            "phloPrice": self.data.phlo_price,
            "phloLimit": self.data.phlo_limit,
            "validAfterBlockNumber": self.data.valid_after_block_number,
            "shardId": self.data.shard_id,
        });
        if self.dialect == NodeDialect::F1r3fly {
            data["expiration_timestamp"] = serde_json::json!(self.data.expiration_timestamp);
        }
        serde_json::json!({
            "data": data,
            "deployer": gaze_net::hex(&self.deployer),
            "signature": gaze_net::hex(&self.sig),
            "sigAlgorithm": "secp256k1",
        })
    }

    /// The deploy id: the signature, hex.
    pub fn id(&self) -> String {
        gaze_net::hex(&self.sig)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protobuf_encoding_matches_prost() {
        let d = DeployData {
            term: "Nil".into(),
            timestamp: 1,
            phlo_price: 1,
            phlo_limit: 300,
            valid_after_block_number: 0,
            shard_id: "root".into(),
            expiration_timestamp: None,
        };
        // 0x12 len "Nil" | 0x18 1 | 0x38 1 | 0x40 300 (0xac 0x02) | 0x5a len "root"
        assert_eq!(
            d.signing_bytes(),
            vec![0x12, 3, b'N', b'i', b'l', 0x18, 1, 0x38, 1, 0x40, 0xac, 0x02, 0x5a, 4, b'r', b'o', b'o', b't']
        );
        let mut e = d.clone();
        e.expiration_timestamp = Some(2);
        assert!(e.signing_bytes().ends_with(&[0x68, 2]));
        // A negative int64 is a ten-byte varint, as in protobuf.
        let mut n = d.clone();
        n.valid_after_block_number = -1;
        let b = n.signing_bytes();
        let i = b.iter().position(|x| *x == 0x50).unwrap();
        assert_eq!(&b[i + 1..i + 11], &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]);
    }

    #[test]
    fn signatures_verify_and_bind_every_field() {
        let k = SigningKey::from_slice(&[7u8; 32]).unwrap();
        let d = DeployData {
            term: "new x in { x!(1) }".into(),
            timestamp: 1_700_000_000_000,
            phlo_price: 1,
            phlo_limit: 100_000,
            valid_after_block_number: 42,
            shard_id: "root".into(),
            expiration_timestamp: Some(1_700_000_300_000),
        };
        let s = sign(&k, d).unwrap();
        assert_eq!(s.deployer.len(), 65);
        assert_eq!(s.deployer[0], 4);
        assert!(verify(&s));
        let mut t = s.clone();
        t.data.term.push(' ');
        assert!(!verify(&t), "a rewritten term is refused");
        let mut u = s.clone();
        u.data.shard_id = "other".into();
        assert!(!verify(&u), "a replay on another shard is refused");
        assert_eq!(s.to_json()["sigAlgorithm"], "secp256k1");
    }

    #[test]
    fn rchain_preimage_omits_field_13() {
        let d = DeployData {
            term: "Nil".into(),
            timestamp: 1,
            phlo_price: 1,
            phlo_limit: 300,
            valid_after_block_number: 0,
            shard_id: "/root".into(),
            expiration_timestamp: Some(2),
        };
        // The rchain preimage is the f1r3fly one minus the field-13 tag.
        let b = d.signing_bytes_for(NodeDialect::Rchain);
        assert!(!b.contains(&0x68), "field 13 (0x68) must be absent for rchain");
        // 0x12 len "Nil" | 0x18 1 | 0x38 1 | 0x40 300 | 0x5a len "/root"
        assert_eq!(b, vec![0x12, 3, b'N', b'i', b'l', 0x18, 1, 0x38, 1, 0x40, 0xac, 0x02, 0x5a, 5, b'/', b'r', b'o', b'o', b't']);
        let mut f = b.clone();
        f.extend_from_slice(&[0x68, 2]);
        assert_eq!(d.signing_bytes(), f, "f1r3fly is the rchain preimage plus field 13");

        // A deploy signed for one dialect does not verify for the other, and
        // the JSON body carries `expiration_timestamp` only on f1r3fly.
        let k = SigningKey::from_slice(&[7u8; 32]).unwrap();
        let s = sign_for(&k, d, NodeDialect::Rchain).unwrap();
        assert!(verify(&s));
        assert!(s.to_json()["data"].get("expiration_timestamp").is_none(), "rchain body omits it");
        let crossed = SignedDeploy { dialect: NodeDialect::F1r3fly, ..s };
        assert!(!verify(&crossed), "the rchain preimage must not verify as f1r3fly");
        assert!(crossed.to_json()["data"].get("expiration_timestamp").is_some(), "f1r3fly body keeps it");
    }
}
