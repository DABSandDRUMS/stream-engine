//! UBJSON decoder for the zlib state payload (`ZB`/`CK`). Port of `util/zlib/ubjson.ts`,
//! extended to the full UBJSON scalar set (the reference covers what consoles emit today;
//! a firmware update adding a type must not break the parser).

use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq)]
pub enum Ub {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Arr(Vec<Ub>),
    Obj(BTreeMap<String, Ub>),
}

impl Ub {
    pub fn get(&self, key: &str) -> Option<&Ub> {
        match self {
            Ub::Obj(m) => m.get(key),
            _ => None,
        }
    }
    pub fn as_obj(&self) -> Option<&BTreeMap<String, Ub>> {
        match self {
            Ub::Obj(m) => Some(m),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Ub::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Ub::Int(i) => Some(*i as f64),
            Ub::Float(f) => Some(*f),
            Ub::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum UbError {
    #[error("payload does not start with an object")]
    NotObject,
    #[error("unexpected end of data at {0}")]
    Eof(usize),
    #[error("unknown type marker 0x{0:02x} at {1}")]
    Marker(u8, usize),
    #[error("negative or oversized length at {0}")]
    Length(usize),
    #[error("nesting deeper than {MAX_DEPTH}")]
    TooDeep,
}

const MAX_DEPTH: usize = 64;

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8, UbError> {
        let b = *self.buf.get(self.pos).ok_or(UbError::Eof(self.pos))?;
        self.pos += 1;
        Ok(b)
    }
    fn peek(&self) -> Result<u8, UbError> {
        self.buf.get(self.pos).copied().ok_or(UbError::Eof(self.pos))
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], UbError> {
        let end = self.pos.checked_add(n).ok_or(UbError::Length(self.pos))?;
        let s = self.buf.get(self.pos..end).ok_or(UbError::Eof(self.pos))?;
        self.pos = end;
        Ok(s)
    }
    fn arr<const N: usize>(&mut self) -> Result<[u8; N], UbError> {
        let s = self.take(N)?;
        let mut a = [0u8; N];
        a.copy_from_slice(s);
        Ok(a)
    }
    /// Integer of the given marker (UBJSON is big-endian).
    fn int(&mut self, marker: u8) -> Result<Option<i64>, UbError> {
        Ok(Some(match marker {
            b'i' => i8::from_be_bytes(self.arr()?) as i64,
            b'U' => u8::from_be_bytes(self.arr()?) as i64,
            b'I' => i16::from_be_bytes(self.arr()?) as i64,
            b'l' => i32::from_be_bytes(self.arr()?) as i64,
            b'L' => i64::from_be_bytes(self.arr()?),
            _ => return Ok(None),
        }))
    }
    fn length(&mut self) -> Result<usize, UbError> {
        let at = self.pos;
        let m = self.byte()?;
        let n = self.int(m)?.ok_or(UbError::Marker(m, at))?;
        usize::try_from(n).map_err(|_| UbError::Length(at))
    }
    fn string(&mut self) -> Result<String, UbError> {
        let n = self.length()?;
        Ok(String::from_utf8_lossy(self.take(n)?).into_owned())
    }

    fn value(&mut self, marker: u8, depth: usize) -> Result<Ub, UbError> {
        if depth > MAX_DEPTH {
            return Err(UbError::TooDeep);
        }
        let at = self.pos - 1;
        if let Some(i) = self.int(marker)? {
            return Ok(Ub::Int(i));
        }
        Ok(match marker {
            b'Z' => Ub::Null,
            b'T' => Ub::Bool(true),
            b'F' => Ub::Bool(false),
            b'd' => Ub::Float(f32::from_be_bytes(self.arr()?) as f64),
            b'D' => Ub::Float(f64::from_be_bytes(self.arr()?)),
            b'C' => Ub::Str(char::from(self.byte()?).to_string()),
            b'S' | b'H' => Ub::Str(self.string()?),
            b'{' => self.object(depth + 1)?,
            b'[' => self.array(depth + 1)?,
            m => return Err(UbError::Marker(m, at)),
        })
    }

    fn object(&mut self, depth: usize) -> Result<Ub, UbError> {
        let mut m = BTreeMap::new();
        loop {
            match self.peek()? {
                b'}' => {
                    self.pos += 1;
                    return Ok(Ub::Obj(m));
                }
                b'N' => {
                    self.pos += 1;
                }
                _ => {
                    let k = self.string()?;
                    let mk = self.next_marker()?;
                    let v = self.value(mk, depth)?;
                    m.insert(k, v);
                }
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Ub, UbError> {
        let mut a = Vec::new();
        loop {
            match self.peek()? {
                b']' => {
                    self.pos += 1;
                    return Ok(Ub::Arr(a));
                }
                _ => {
                    let mk = self.next_marker()?;
                    if mk == b'N' {
                        continue;
                    }
                    a.push(self.value(mk, depth)?);
                }
            }
        }
    }

    fn next_marker(&mut self) -> Result<u8, UbError> {
        loop {
            let m = self.byte()?;
            if m != b'N' {
                return Ok(m);
            }
        }
    }
}

/// Decode a UBJSON document whose root is an object.
pub fn decode(buf: &[u8]) -> Result<Ub, UbError> {
    if buf.first() != Some(&b'{') {
        return Err(UbError::NotObject);
    }
    let mut r = Reader { buf, pos: 1 };
    r.object(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(k: &str) -> Vec<u8> {
        let mut v = vec![b'i', k.len() as u8];
        v.extend_from_slice(k.as_bytes());
        v
    }

    #[test]
    fn decodes_console_shaped_document() {
        // {"id":"Synchronize","children":{"line":{"values":{"volume":0.5d,"mute":0i,"n":-2i}}},"l":[1U,"x"]}
        let mut b = vec![b'{'];
        b.extend(key("id"));
        b.extend([b'S', b'i', 11]);
        b.extend(b"Synchronize");
        b.extend(key("children"));
        b.push(b'{');
        b.extend(key("line"));
        b.push(b'{');
        b.extend(key("values"));
        b.push(b'{');
        b.extend(key("volume"));
        b.push(b'd');
        b.extend(0.5f32.to_be_bytes());
        b.extend(key("mute"));
        b.extend([b'i', 0]);
        b.extend(key("n"));
        b.extend([b'i', 0xfe]);
        b.extend(key("big"));
        b.push(b'I');
        b.extend(1000i16.to_be_bytes());
        b.extend(b"}}}");
        b.extend(key("l"));
        b.extend([b'[', b'U', 200, b'S', b'i', 1, b'x', b']']);
        b.push(b'}');
        let d = decode(&b).unwrap();
        assert_eq!(d.get("id").and_then(Ub::as_str), Some("Synchronize"));
        let v = d.get("children").unwrap().get("line").unwrap().get("values").unwrap();
        assert_eq!(v.get("volume"), Some(&Ub::Float(0.5)));
        assert_eq!(v.get("mute"), Some(&Ub::Int(0)));
        assert_eq!(v.get("n"), Some(&Ub::Int(-2)));
        assert_eq!(v.get("big"), Some(&Ub::Int(1000)));
        assert_eq!(d.get("l"), Some(&Ub::Arr(vec![Ub::Int(200), Ub::Str("x".into())])));
    }

    #[test]
    fn errors_instead_of_panicking_on_garbage() {
        assert_eq!(decode(b"[]"), Err(UbError::NotObject));
        assert!(matches!(decode(b"{"), Err(UbError::Eof(_))));
        let mut b = vec![b'{'];
        b.extend(key("k"));
        b.push(b'?');
        assert!(matches!(decode(&b), Err(UbError::Marker(b'?', _))));
        // string length larger than the buffer
        let mut b = vec![b'{'];
        b.extend(key("k"));
        b.extend([b'S', b'U', 200, b'a']);
        assert!(matches!(decode(&b), Err(UbError::Eof(_))));
        let deep = (*b"{").into_iter().chain((0..100).flat_map(|_| [b'i', 1, b'a', b'{'])).collect::<Vec<u8>>();
        assert_eq!(decode(&deep), Err(UbError::TooDeep));
    }
}
