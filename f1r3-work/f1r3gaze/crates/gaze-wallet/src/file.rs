//! Wallet files, as F1R3Sky saves and loads them (the Embers SDK's
//! `serializeKey` / `deserializeKey`):
//! `{"keyType":"secp256k1","value":"<HEX>","valueFormat":"hex"}`,
//! upper-case hex. A key moves between F1R3Sky and F1R3Gaze as this file.

use k256::ecdsa::SigningKey;

pub fn serialize(k: &SigningKey) -> String {
    let hex: String = k.to_bytes().iter().map(|b| format!("{b:02X}")).collect();
    format!(r#"{{"keyType":"secp256k1","value":"{hex}","valueFormat":"hex"}}"#)
}

/// Read a wallet file. A bare 64-digit hex key is accepted too.
pub fn deserialize(text: &str) -> Result<SigningKey, String> {
    let t = text.trim();
    let hex = if t.starts_with('{') {
        let v: serde_json::Value = serde_json::from_str(t).map_err(|e| format!("not a wallet file: {e}"))?;
        if v.get("keyType").and_then(|x| x.as_str()) != Some("secp256k1") {
            return Err("unsupported key type (expected secp256k1)".into());
        }
        if v.get("valueFormat").and_then(|x| x.as_str()) != Some("hex") {
            return Err("unsupported value format (expected hex)".into());
        }
        v.get("value").and_then(|x| x.as_str()).ok_or("wallet file has no value")?.to_string()
    } else {
        t.trim_start_matches("0x").to_string()
    };
    let b = gaze_net::unhex(&hex.to_ascii_lowercase()).filter(|b| b.len() == 32).ok_or("the key must be 32 bytes of hex")?;
    SigningKey::from_slice(&b).map_err(|_| "not a valid secp256k1 private key".to_string())
}
