//! A minimal JSON value, writer and parser: the wire between the module and
//! `gaze-reach.js`. Integers only (no floats cross the wall), which keeps
//! every value the page sees bit-identical to the native browser's.

#[derive(Clone, Debug, PartialEq)]
pub enum J {
    Null,
    Bool(bool),
    Int(i64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    pub fn get(&self, k: &str) -> Option<&J> {
        match self {
            J::Obj(v) => v.iter().find(|(x, _)| x == k).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn str(&self) -> Option<&str> {
        match self {
            J::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn int(&self) -> Option<i64> {
        match self {
            J::Int(i) => Some(*i),
            _ => None,
        }
    }
    pub fn arr(&self) -> Option<&[J]> {
        match self {
            J::Arr(v) => Some(v),
            _ => None,
        }
    }
    pub fn obj(kv: Vec<(&str, J)>) -> J {
        J::Obj(kv.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }
    pub fn s(x: &str) -> J {
        J::Str(x.to_string())
    }

    pub fn write(&self, o: &mut String) {
        match self {
            J::Null => o.push_str("null"),
            J::Bool(b) => o.push_str(if *b { "true" } else { "false" }),
            J::Int(i) => o.push_str(&i.to_string()),
            J::Str(s) => {
                o.push('"');
                for c in s.chars() {
                    match c {
                        '"' => o.push_str("\\\""),
                        '\\' => o.push_str("\\\\"),
                        '\n' => o.push_str("\\n"),
                        '\r' => o.push_str("\\r"),
                        '\t' => o.push_str("\\t"),
                        c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
                        c => o.push(c),
                    }
                }
                o.push('"');
            }
            J::Arr(v) => {
                o.push('[');
                for (i, x) in v.iter().enumerate() {
                    if i > 0 {
                        o.push(',');
                    }
                    x.write(o);
                }
                o.push(']');
            }
            J::Obj(v) => {
                o.push('{');
                for (i, (k, x)) in v.iter().enumerate() {
                    if i > 0 {
                        o.push(',');
                    }
                    J::Str(k.clone()).write(o);
                    o.push(':');
                    x.write(o);
                }
                o.push('}');
            }
        }
    }

    pub fn to_string(&self) -> String {
        let mut s = String::new();
        self.write(&mut s);
        s
    }

    pub fn parse(s: &str) -> Result<J, String> {
        let b = s.as_bytes();
        let mut i = 0;
        let v = value(b, &mut i, 0)?;
        ws(b, &mut i);
        if i != b.len() {
            return Err(format!("trailing data at {i}"));
        }
        Ok(v)
    }
}

fn ws(b: &[u8], i: &mut usize) {
    while *i < b.len() && b[*i].is_ascii_whitespace() {
        *i += 1;
    }
}

fn value(b: &[u8], i: &mut usize, depth: u32) -> Result<J, String> {
    if depth > 256 {
        return Err("too deep".into());
    }
    ws(b, i);
    let c = *b.get(*i).ok_or("unexpected end")?;
    match c {
        b'n' if b[*i..].starts_with(b"null") => {
            *i += 4;
            Ok(J::Null)
        }
        b't' if b[*i..].starts_with(b"true") => {
            *i += 4;
            Ok(J::Bool(true))
        }
        b'f' if b[*i..].starts_with(b"false") => {
            *i += 5;
            Ok(J::Bool(false))
        }
        b'"' => string(b, i).map(J::Str),
        b'[' => {
            *i += 1;
            let mut v = Vec::new();
            ws(b, i);
            if b.get(*i) == Some(&b']') {
                *i += 1;
                return Ok(J::Arr(v));
            }
            loop {
                v.push(value(b, i, depth + 1)?);
                ws(b, i);
                match b.get(*i) {
                    Some(b',') => *i += 1,
                    Some(b']') => {
                        *i += 1;
                        return Ok(J::Arr(v));
                    }
                    _ => return Err("expected , or ]".into()),
                }
            }
        }
        b'{' => {
            *i += 1;
            let mut v = Vec::new();
            ws(b, i);
            if b.get(*i) == Some(&b'}') {
                *i += 1;
                return Ok(J::Obj(v));
            }
            loop {
                ws(b, i);
                let k = string(b, i)?;
                ws(b, i);
                if b.get(*i) != Some(&b':') {
                    return Err("expected :".into());
                }
                *i += 1;
                v.push((k, value(b, i, depth + 1)?));
                ws(b, i);
                match b.get(*i) {
                    Some(b',') => *i += 1,
                    Some(b'}') => {
                        *i += 1;
                        return Ok(J::Obj(v));
                    }
                    _ => return Err("expected , or }".into()),
                }
            }
        }
        b'-' | b'0'..=b'9' => {
            let st = *i;
            *i += 1;
            while *i < b.len() && (b[*i].is_ascii_digit() || matches!(b[*i], b'.' | b'e' | b'E' | b'+' | b'-')) {
                *i += 1;
            }
            let t = std::str::from_utf8(&b[st..*i]).map_err(|e| e.to_string())?;
            // Numbers from the host are integers; a fractional one (a
            // coordinate) is truncated toward zero.
            t.parse::<i64>().or_else(|_| t.parse::<f64>().map(|f| f as i64)).map(J::Int).map_err(|e| e.to_string())
        }
        _ => Err(format!("unexpected byte {c} at {i}")),
    }
}

fn string(b: &[u8], i: &mut usize) -> Result<String, String> {
    if b.get(*i) != Some(&b'"') {
        return Err("expected string".into());
    }
    *i += 1;
    let mut out: Vec<u8> = Vec::new();
    loop {
        let c = *b.get(*i).ok_or("unterminated string")?;
        *i += 1;
        match c {
            b'"' => return String::from_utf8(out).map_err(|e| e.to_string()),
            b'\\' => {
                let e = *b.get(*i).ok_or("bad escape")?;
                *i += 1;
                match e {
                    b'"' => out.push(b'"'),
                    b'\\' => out.push(b'\\'),
                    b'/' => out.push(b'/'),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b'u' => {
                        let hex = std::str::from_utf8(b.get(*i..*i + 4).ok_or("bad \\u")?).map_err(|e| e.to_string())?;
                        *i += 4;
                        let mut cp = u32::from_str_radix(hex, 16).map_err(|e| e.to_string())?;
                        if (0xD800..0xDC00).contains(&cp) && b.get(*i..*i + 2) == Some(b"\\u") {
                            let lo = std::str::from_utf8(b.get(*i + 2..*i + 6).ok_or("bad surrogate")?).map_err(|e| e.to_string())?;
                            let lo = u32::from_str_radix(lo, 16).map_err(|e| e.to_string())?;
                            *i += 6;
                            cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                        }
                        let ch = char::from_u32(cp).unwrap_or('\u{fffd}');
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    }
                    _ => return Err("bad escape".into()),
                }
            }
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round_trips() {
        let v = J::obj(vec![("a", J::Arr(vec![J::Int(-3), J::Null, J::Bool(true)])), ("s", J::s("q\"\n\u{1}é😀"))]);
        assert_eq!(J::parse(&v.to_string()).unwrap(), v);
        assert_eq!(J::parse(r#"{"x": 12.7, "e": "\ud83d\ude00"}"#).unwrap().get("x"), Some(&J::Int(12)));
        assert_eq!(J::parse(r#""\ud83d\ude00""#).unwrap(), J::s("😀"));
        assert!(J::parse("[1,").is_err());
    }
}
