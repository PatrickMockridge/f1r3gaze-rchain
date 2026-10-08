//! The node's JSON `RhoExpr` (externally tagged: `{"ExprInt": {"data": 1}}`)
//! as ground normal forms, which is what pages receive and what the quorum
//! rung compares, by encoding.

use k1ndl1ng_norm::Norm;
use serde_json::Value;

pub fn to_norm(v: &Value) -> Norm {
    let Some(obj) = v.as_object() else { return Norm::nil() };
    let Some((tag, body)) = obj.iter().next() else { return Norm::nil() };
    let data = body.get("data");
    let list = |d: Option<&Value>| -> Vec<Norm> {
        d.and_then(|x| x.as_array()).map(|a| a.iter().map(to_norm).collect()).unwrap_or_default()
    };
    match tag.as_str() {
        "ExprBool" => Norm::bool(data.and_then(|d| d.as_bool()).unwrap_or(false)),
        "ExprInt" => Norm::int(data.and_then(|d| d.as_i64()).unwrap_or(0)),
        "ExprString" | "ExprUri" | "ExprBigInt" => Norm::str(data.and_then(|d| d.as_str()).unwrap_or("")),
        "ExprBytes" => Norm::bytes(&data.and_then(|d| d.as_str()).and_then(gaze_net::unhex).unwrap_or_default()),
        "ExprTuple" => {
            let items = list(data);
            if items.len() >= 2 { Norm::tuple(items) } else { items.into_iter().next().unwrap_or_else(Norm::nil) }
        }
        "ExprList" | "ExprSet" => Norm::list(list(data)),
        "ExprPar" => {
            let items = list(data);
            match items.len() {
                0 => Norm::nil(),
                1 => items.into_iter().next().unwrap_or_else(Norm::nil),
                _ => Norm::list(items),
            }
        }
        "ExprMap" => {
            let pairs = data
                .and_then(|d| d.as_object())
                .map(|m| m.iter().map(|(k, v)| (Norm::str(k), to_norm(v))).collect())
                .unwrap_or_default();
            Norm::map(pairs)
        }
        // A name on the shard is not a name in the page: it arrives as a
        // tagged string the page can show or hand back to the bridge.
        "ExprUnforg" => {
            let (kind, hexs) = body
                .get("data")
                .and_then(|u| u.as_object())
                .and_then(|u| u.iter().next())
                .map(|(k, v)| (k.clone(), v.get("data").and_then(|d| d.as_str()).unwrap_or("").to_string()))
                .unwrap_or_default();
            Norm::tuple(vec![Norm::str("unforgeable"), Norm::str(&kind), Norm::str(&hexs)])
        }
        "ExprBundle" => body.get("data").map(to_norm).unwrap_or_else(Norm::nil),
        other => Norm::tuple(vec![Norm::str("unrepresented"), Norm::str(other)]),
    }
}

/// Plain JSON (not a `RhoExpr` envelope) as ground normal forms — the node's
/// ordinary camelCase DTOs, which the chain and staking reads return.
///
/// Deliberately dumb about types: a hex-looking string stays a `str`, because
/// `sig`, `blockHash`, `preStateHash`, `deployer`, `deployId` and even a
/// deploy's `term` are all strings that merely look like bytes. Where a field
/// is *known* to be a byte array, the caller (see [`crate::chain`],
/// [`crate::pos`]) decodes it to `Norm::bytes` itself.
///
/// A number that does not fit `i64` — only `DeployInfo.cost` is a `u64` — and
/// any float become strings rather than being truncated.
pub fn json_to_norm(v: &Value) -> Norm {
    match v {
        Value::Null => Norm::nil(),
        Value::Bool(b) => Norm::bool(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => Norm::int(i),
            None => Norm::str(&n.to_string()),
        },
        Value::String(s) => Norm::str(s),
        Value::Array(a) => Norm::list(a.iter().map(json_to_norm).collect()),
        Value::Object(m) => Norm::map(m.iter().map(|(k, v)| (Norm::str(k), json_to_norm(v))).collect()),
    }
}

/// A 65-byte validator (or deployer) key, decoded to bytes. Falls back to the
/// raw string when the field is not a 65-byte hex key, so a node that changes
/// its spelling does not lose the value.
pub fn key_field(hex: &str) -> Norm {
    match gaze_net::unhex(hex) {
        Some(b) if b.len() == 65 => Norm::bytes(&b),
        _ => Norm::str(hex),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_json_converts() {
        let v: Value = serde_json::from_str(
            r#"{"blockHash":"ab","blockNumber":7,"final":false,"missing":null,
                "bonds":[{"validator":"04","stake":3}],"big":18446744073709551615}"#,
        )
        .unwrap();
        let n = json_to_norm(&v);
        assert_eq!(n.map_get("blockHash").and_then(|x| x.as_str()), Some("ab"));
        assert_eq!(n.map_get("blockNumber").and_then(|x| x.as_int()), Some(7));
        assert_eq!(n.map_get("final"), Some(&Norm::bool(false)));
        assert!(n.map_get("missing").is_some_and(Norm::is_nil));
        // Key order in JSON does not change the encoding: maps are canonical.
        let reordered: Value = serde_json::from_str(
            r#"{"big":18446744073709551615,"bonds":[{"validator":"04","stake":3}],"missing":null,
                "final":false,"blockNumber":7,"blockHash":"ab"}"#,
        )
        .unwrap();
        assert_eq!(json_to_norm(&reordered).encode(), n.encode());
        // A 65-byte key that looks like hex is still a string here, not bytes.
        assert_eq!(n.map_get("bonds").and_then(|b| b.as_coll(k1ndl1ng_norm::CollKind::List)).and_then(|l| l.first()).and_then(|e| e.map_get("validator")).and_then(|x| x.as_str()), Some("04"));
        // A u64 beyond i64::MAX degrades to a string rather than truncating.
        assert_eq!(n.map_get("big").and_then(|x| x.as_str()), Some("18446744073709551615"));
    }

    #[test]
    fn manifests_convert() {
        let v: Value = serde_json::from_str(
            r#"{"ExprMap":{"data":{"gaze":{"ExprInt":{"data":1}},"entry":{"ExprString":{"data":"index.html"}},
               "files":{"ExprMap":{"data":{"index.html":{"ExprBytes":{"data":"00ff"}}}}},
               "mirrors":{"ExprList":{"data":[{"ExprString":{"data":"https://m/"}}]}}}}}"#,
        )
        .unwrap();
        let n = to_norm(&v);
        assert_eq!(n.map_get("gaze").and_then(|x| x.as_int()), Some(1));
        assert_eq!(n.map_get("entry").and_then(|x| x.as_str()), Some("index.html"));
        // Key order in JSON does not change the encoding: maps are canonical.
        let v2: Value = serde_json::from_str(
            r#"{"ExprMap":{"data":{"mirrors":{"ExprList":{"data":[{"ExprString":{"data":"https://m/"}}]}},
               "files":{"ExprMap":{"data":{"index.html":{"ExprBytes":{"data":"00ff"}}}}},
               "entry":{"ExprString":{"data":"index.html"}},"gaze":{"ExprInt":{"data":1}}}}}"#,
        )
        .unwrap();
        assert_eq!(to_norm(&v2).encode(), n.encode());
    }
}
