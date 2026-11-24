//! A small JSON reader for the output of `scripts/linux/inspect-ntfs.py`.
//!
//! Written here rather than pulled in as a dependency for one reason beyond
//! keeping the crate dependency-free: NTFS names are UTF-16 code units and
//! may contain unpaired surrogates, which the inspector emits as lone
//! `\uD800`-style escapes. Strings are therefore kept as UTF-16 code units,
//! so that names compare unit for unit.

use std::fmt;

/// A JSON value. Numbers are integers (the inspector emits nothing else).
#[derive(Clone, PartialEq, Eq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i128),
    Str(Vec<u16>),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl fmt::Debug for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Json::Null => f.write_str("null"),
            Json::Bool(b) => write!(f, "{b}"),
            Json::Int(i) => write!(f, "{i}"),
            Json::Str(s) => write!(f, "{:?}", String::from_utf16_lossy(s)),
            Json::Arr(a) => f.debug_list().entries(a).finish(),
            Json::Obj(o) => f
                .debug_map()
                .entries(o.iter().map(|(k, v)| (k, v)))
                .finish(),
        }
    }
}

impl Json {
    /// Parses one JSON document.
    pub fn parse(text: &[u8]) -> Result<Json, String> {
        let mut p = Parser { text, pos: 0 };
        let value = p.value(0)?;
        p.skip_ws();
        if p.pos != text.len() {
            return Err(format!("trailing data at byte {}", p.pos));
        }
        Ok(value)
    }

    /// Member `key` of an object; `None` when absent or not an object.
    pub fn opt(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Member `key` of an object. Panics when it is missing: the tests rely
    /// on the inspector's documented schema.
    pub fn get(&self, key: &str) -> &Json {
        self.opt(key)
            .unwrap_or_else(|| panic!("JSON member {key:?} missing in {self:?}"))
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    pub fn int(&self) -> i128 {
        match self {
            Json::Int(i) => *i,
            other => panic!("JSON integer expected, found {other:?}"),
        }
    }

    pub fn u64(&self) -> u64 {
        u64::try_from(self.int()).unwrap_or_else(|_| panic!("{self:?} is not a u64"))
    }

    pub fn i64(&self) -> i64 {
        i64::try_from(self.int()).unwrap_or_else(|_| panic!("{self:?} is not an i64"))
    }

    pub fn bool(&self) -> bool {
        match self {
            Json::Bool(b) => *b,
            other => panic!("JSON boolean expected, found {other:?}"),
        }
    }

    pub fn arr(&self) -> &[Json] {
        match self {
            Json::Arr(a) => a,
            other => panic!("JSON array expected, found {other:?}"),
        }
    }

    /// A string as UTF-16 code units.
    pub fn units(&self) -> &[u16] {
        match self {
            Json::Str(s) => s,
            other => panic!("JSON string expected, found {other:?}"),
        }
    }
}

struct Parser<'a> {
    text: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while matches!(self.text.get(self.pos), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.text.get(self.pos) == Some(&byte) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected {:?} at byte {}", byte as char, self.pos))
        }
    }

    fn literal(&mut self, word: &str, value: Json) -> Result<Json, String> {
        if self.text[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(format!("unexpected token at byte {}", self.pos))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > 64 {
            return Err("nesting too deep".into());
        }
        self.skip_ws();
        match self.text.get(self.pos).copied() {
            Some(b'{') => {
                self.pos += 1;
                let mut members = Vec::new();
                self.skip_ws();
                if self.text.get(self.pos) == Some(&b'}') {
                    self.pos += 1;
                    return Ok(Json::Obj(members));
                }
                loop {
                    self.skip_ws();
                    let key = String::from_utf16(&self.string()?)
                        .map_err(|_| format!("bad key before byte {}", self.pos))?;
                    self.skip_ws();
                    self.expect(b':')?;
                    members.push((key, self.value(depth + 1)?));
                    self.skip_ws();
                    match self.text.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b'}') => {
                            self.pos += 1;
                            return Ok(Json::Obj(members));
                        }
                        _ => return Err(format!("expected ',' or '}}' at byte {}", self.pos)),
                    }
                }
            }
            Some(b'[') => {
                self.pos += 1;
                let mut items = Vec::new();
                self.skip_ws();
                if self.text.get(self.pos) == Some(&b']') {
                    self.pos += 1;
                    return Ok(Json::Arr(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.skip_ws();
                    match self.text.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {
                            self.pos += 1;
                            return Ok(Json::Arr(items));
                        }
                        _ => return Err(format!("expected ',' or ']' at byte {}", self.pos)),
                    }
                }
            }
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'n') => self.literal("null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(format!("unexpected byte at {}", self.pos)),
        }
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        if self.text.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        while matches!(self.text.get(self.pos), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if matches!(self.text.get(self.pos), Some(b'.' | b'e' | b'E')) {
            return Err(format!("non-integer number at byte {start}"));
        }
        std::str::from_utf8(&self.text[start..self.pos])
            .ok()
            .and_then(|digits| digits.parse::<i128>().ok())
            .map(Json::Int)
            .ok_or_else(|| format!("bad number at byte {start}"))
    }

    fn string(&mut self) -> Result<Vec<u16>, String> {
        self.expect(b'"')?;
        let mut units = Vec::new();
        loop {
            let byte = *self
                .text
                .get(self.pos)
                .ok_or_else(|| "unterminated string".to_string())?;
            self.pos += 1;
            match byte {
                b'"' => return Ok(units),
                b'\\' => {
                    let escape = *self
                        .text
                        .get(self.pos)
                        .ok_or_else(|| "unterminated escape".to_string())?;
                    self.pos += 1;
                    match escape {
                        b'"' | b'\\' | b'/' => units.push(u16::from(escape)),
                        b'b' => units.push(0x08),
                        b'f' => units.push(0x0C),
                        b'n' => units.push(0x0A),
                        b'r' => units.push(0x0D),
                        b't' => units.push(0x09),
                        b'u' => {
                            let hex = self
                                .text
                                .get(self.pos..self.pos + 4)
                                .and_then(|h| std::str::from_utf8(h).ok())
                                .and_then(|h| u16::from_str_radix(h, 16).ok())
                                .ok_or_else(|| format!("bad \\u escape at byte {}", self.pos))?;
                            self.pos += 4;
                            units.push(hex);
                        }
                        _ => return Err(format!("bad escape at byte {}", self.pos)),
                    }
                }
                0x00..=0x1F => return Err(format!("control byte in string at {}", self.pos)),
                0x20..=0x7F => units.push(u16::from(byte)),
                _ => {
                    // Raw UTF-8 (the inspector escapes everything, but be a
                    // correct reader): decode one scalar value.
                    let start = self.pos - 1;
                    let len = match byte {
                        0xC0..=0xDF => 2,
                        0xE0..=0xEF => 3,
                        0xF0..=0xF7 => 4,
                        _ => return Err(format!("bad UTF-8 at byte {start}")),
                    };
                    let scalar = self
                        .text
                        .get(start..start + len)
                        .and_then(|b| std::str::from_utf8(b).ok())
                        .and_then(|s| s.chars().next())
                        .ok_or_else(|| format!("bad UTF-8 at byte {start}"))?;
                    self.pos = start + len;
                    let mut buf = [0u16; 2];
                    units.extend_from_slice(scalar.encode_utf16(&mut buf));
                }
            }
        }
    }
}
