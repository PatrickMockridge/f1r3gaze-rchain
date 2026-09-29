//! F1R3Cap addresses, as the Embers SDK and `firefly-client` derive them:
//! base58 of `[0, 0, 0] ‖ [0] ‖ keccak256(keccak256(pk[1..])[12..]) ‖ c`,
//! where `pk` is the 65-byte uncompressed public key and `c` the first four
//! bytes of Blake2b-256 of everything before it.

use k1ndl1ng_norm::hash::blake2b_256;
use k256::ecdsa::VerifyingKey;
use sha3::{Digest, Keccak256};

const PREFIX: [u8; 4] = [0, 0, 0, 0];

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Address(String);

impl Address {
    pub fn from_key(k: &VerifyingKey) -> Address {
        let pk = k.to_encoded_point(false);
        let key_hash = Keccak256::digest(&pk.as_bytes()[1..]);
        let eth_hash = Keccak256::digest(&key_hash[12..]);
        let mut payload = PREFIX.to_vec();
        payload.extend_from_slice(&eth_hash);
        let c = blake2b_256(&payload).0;
        payload.extend_from_slice(&c[..4]);
        Address(bs58::encode(payload).into_string())
    }

    /// Parse and check an address: base58, 40 bytes, the F1R3Cap prefix and
    /// version, and the checksum.
    pub fn parse(s: &str) -> Result<Address, String> {
        let s = s.trim();
        let b = bs58::decode(s).into_vec().map_err(|_| format!("{s} is not base58"))?;
        if b.len() != 40 || b[..4] != PREFIX {
            return Err(format!("{s} is not a F1R3Cap address"));
        }
        if blake2b_256(&b[..36]).0[..4] != b[36..] {
            return Err(format!("{s}: checksum mismatch"));
        }
        Ok(Address(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
