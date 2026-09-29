//! Deploy construction and signing, exactly as `f1r3node-rust` verifies it
//! (`crypto::signatures::Signed::from_signed_data`): the signature is DER
//! ECDSA over secp256k1, on the BLAKE2b-256 prehash of the protobuf encoding
//! of `DeployDataProto` with the signer fields (deployer, sig, sigAlgorithm)
//! empty; the deployer is the 65-byte uncompressed public key.

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
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut o = Vec::new();
        field_bytes(2, self.term.as_bytes(), &mut o);
        field_varint(3, self.timestamp, &mut o);
        field_varint(7, self.phlo_price, &mut o);
        field_varint(8, self.phlo_limit, &mut o);
        field_varint(10, self.valid_after_block_number, &mut o);
        field_bytes(11, self.shard_id.as_bytes(), &mut o);
        field_varint(13, self.expiration_timestamp.unwrap_or(0), &mut o);
        o
    }

    pub fn signing_hash(&self) -> [u8; 32] {
        blake2b_256(&self.signing_bytes()).0
    }
}

#[derive(Clone, Debug)]
pub struct SignedDeploy {
    pub data: DeployData,
    pub deployer: Vec<u8>,
    pub sig: Vec<u8>,
}

pub fn public_key(k: &SigningKey) -> Vec<u8> {
    k.verifying_key().to_encoded_point(false).as_bytes().to_vec()
}

pub fn sign(k: &SigningKey, data: DeployData) -> Result<SignedDeploy, String> {
    let sig: Signature = k.sign_prehash(&data.signing_hash()).map_err(|e| e.to_string())?;
    let sig = sig.normalize_s().unwrap_or(sig);
    Ok(SignedDeploy {
        deployer: public_key(k),
        sig: sig.to_der().as_bytes().to_vec(),
        data,
    })
}

/// The node's check, for tests and for the hostile-proxy harness.
pub fn verify(d: &SignedDeploy) -> bool {
    let Ok(vk) = VerifyingKey::from_sec1_bytes(&d.deployer) else { return false };
    let Ok(sig) = Signature::from_der(&d.sig) else { return false };
    vk.verify_prehash(&d.data.signing_hash(), &sig).is_ok()
}

impl SignedDeploy {
    /// The body of `POST /api/deploy`.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "data": {
                "term": self.data.term,
                "timestamp": self.data.timestamp,
                "phloPrice": self.data.phlo_price,
                "phloLimit": self.data.phlo_limit,
                "validAfterBlockNumber": self.data.valid_after_block_number,
                "shardId": self.data.shard_id,
                "expiration_timestamp": self.data.expiration_timestamp,
            },
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
}
