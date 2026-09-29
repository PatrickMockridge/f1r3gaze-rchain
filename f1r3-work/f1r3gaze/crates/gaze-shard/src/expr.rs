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

#[cfg(test)]
mod tests {
    use super::*;

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
