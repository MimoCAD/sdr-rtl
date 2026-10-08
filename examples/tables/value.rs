//! A JSON reader for the bench records: a value tree and a recursive
//! descent parser over the record text. No crate; the records are the
//! harness's own writer's output (examples/harness/json.rs), so the
//! grammar needed is RFC 8259's with nothing beyond it. Objects keep
//! their fields in file order and may repeat a key (the record headers
//! print `bench` twice); a lookup returns the first.

use std::fmt;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    /// A literal without '.', 'e' or 'E'.
    Int(i64),
    Num(f64),
    Str(String),
    Arr(Vec<Value>),
    Obj(Vec<(String, Value)>),
}

#[derive(Debug)]
pub struct ParseError {
    pub offset: usize,
    pub what: &'static str,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JSON parse error at byte {}: {}", self.offset, self.what)
    }
}

impl Value {
    pub fn parse(text: &str) -> Result<Value, ParseError> {
        let mut p = Parser { b: text.as_bytes(), i: 0 };
        p.ws();
        let v = p.value()?;
        p.ws();
        if p.i != p.b.len() {
            return Err(p.err("trailing characters"));
        }
        Ok(v)
    }

    /// A field of an object (the first with that name); None otherwise.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// `get` along a path of field names.
    pub fn path(&self, keys: &[&str]) -> Option<&Value> {
        keys.iter().try_fold(self, |v, k| v.get(k))
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Num(x) if x.fract() == 0.0 && x.abs() < 9.0e15 => Some(*x as i64),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Num(x) => Some(*x),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&[Value]> {
        match self {
            Value::Arr(items) => Some(items),
            _ => None,
        }
    }

    // Typed field readers: a missing or mistyped field is an error naming
    // the field, so a record that changed shape stops the generator
    // instead of producing a table with gaps.

    pub fn field_i64(&self, key: &str) -> Result<i64, String> {
        self.get(key).and_then(Value::as_i64).ok_or_else(|| format!("field {key}: integer expected"))
    }

    pub fn field_f64(&self, key: &str) -> Result<f64, String> {
        self.get(key).and_then(Value::as_f64).ok_or_else(|| format!("field {key}: number expected"))
    }

    pub fn field_str(&self, key: &str) -> Result<&str, String> {
        self.get(key).and_then(Value::as_str).ok_or_else(|| format!("field {key}: string expected"))
    }

    pub fn field_bool(&self, key: &str) -> Result<bool, String> {
        self.get(key).and_then(Value::as_bool).ok_or_else(|| format!("field {key}: boolean expected"))
    }

    pub fn field_arr(&self, key: &str) -> Result<&[Value], String> {
        self.get(key).and_then(Value::as_arr).ok_or_else(|| format!("field {key}: array expected"))
    }

    pub fn field(&self, key: &str) -> Result<&Value, String> {
        self.get(key).ok_or_else(|| format!("field {key}: missing"))
    }
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn err(&self, what: &'static str) -> ParseError {
        ParseError { offset: self.i, what }
    }

    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn expect(&mut self, c: u8) -> Result<(), ParseError> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            Err(self.err("unexpected character"))
        }
    }

    fn literal(&mut self, word: &[u8], v: Value) -> Result<Value, ParseError> {
        if self.b[self.i..].starts_with(word) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(self.err("unknown literal"))
        }
    }

    fn value(&mut self) -> Result<Value, ParseError> {
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string().map(Value::Str),
            Some(b't') => self.literal(b"true", Value::Bool(true)),
            Some(b'f') => self.literal(b"false", Value::Bool(false)),
            Some(b'n') => self.literal(b"null", Value::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            _ => Err(self.err("value expected")),
        }
    }

    fn object(&mut self) -> Result<Value, ParseError> {
        self.expect(b'{')?;
        let mut fields = Vec::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Value::Obj(fields));
        }
        loop {
            self.ws();
            let k = self.string()?;
            self.ws();
            self.expect(b':')?;
            self.ws();
            let v = self.value()?;
            fields.push((k, v));
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Value::Obj(fields));
                }
                _ => return Err(self.err("',' or '}' expected")),
            }
        }
    }

    fn array(&mut self) -> Result<Value, ParseError> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Value::Arr(items));
        }
        loop {
            self.ws();
            items.push(self.value()?);
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Value::Arr(items));
                }
                _ => return Err(self.err("',' or ']' expected")),
            }
        }
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let c = self.peek().ok_or_else(|| self.err("unterminated string"))?;
            self.i += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let e = self.peek().ok_or_else(|| self.err("unterminated escape"))?;
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let cp = self.hex4()?;
                            // A surrogate pair is two \u escapes.
                            let ch = if (0xD800..0xDC00).contains(&cp) {
                                if !self.b[self.i..].starts_with(b"\\u") {
                                    return Err(self.err("lone high surrogate"));
                                }
                                self.i += 2;
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return Err(self.err("bad low surrogate"));
                                }
                                0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00)
                            } else {
                                cp
                            };
                            out.push(char::from_u32(ch).ok_or_else(|| self.err("bad code point"))?);
                        }
                        _ => return Err(self.err("unknown escape")),
                    }
                }
                _ => {
                    // Multi-byte UTF-8: copy the whole scalar.
                    let start = self.i - 1;
                    let len = utf8_len(c);
                    let end = start + len;
                    if end > self.b.len() {
                        return Err(self.err("truncated UTF-8"));
                    }
                    let s = std::str::from_utf8(&self.b[start..end]).map_err(|_| self.err("bad UTF-8"))?;
                    out.push_str(s);
                    self.i = end;
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        if self.i + 4 > self.b.len() {
            return Err(self.err("short \\u escape"));
        }
        let s = std::str::from_utf8(&self.b[self.i..self.i + 4]).map_err(|_| self.err("bad \\u escape"))?;
        let v = u32::from_str_radix(s, 16).map_err(|_| self.err("bad \\u escape"))?;
        self.i += 4;
        Ok(v)
    }

    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        let mut float = false;
        while let Some(c) = self.peek() {
            match c {
                b'0'..=b'9' => {}
                b'.' | b'e' | b'E' | b'+' | b'-' => float = true,
                _ => break,
            }
            self.i += 1;
        }
        let s = std::str::from_utf8(&self.b[start..self.i]).map_err(|_| self.err("bad number"))?;
        if float {
            s.parse::<f64>().map(Value::Num).map_err(|_| self.err("bad number"))
        } else {
            match s.parse::<i64>() {
                Ok(i) => Ok(Value::Int(i)),
                Err(_) => s.parse::<f64>().map(Value::Num).map_err(|_| self.err("bad number")),
            }
        }
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_record_shapes() {
        let v = Value::parse(r#"{"k":"bt","f":42000000,"m":-1.5e3,"a":[1,2,{"x":null}],"s":"a\"bé\n","t":true,"bench":1,"bench":2}"#).unwrap();
        assert_eq!(v.field_str("k").unwrap(), "bt");
        assert_eq!(v.field_i64("f").unwrap(), 42_000_000);
        assert_eq!(v.field_f64("m").unwrap(), -1500.0);
        assert_eq!(v.field_arr("a").unwrap().len(), 3);
        assert_eq!(v.field_str("s").unwrap(), "a\"bé\n");
        assert!(v.field_bool("t").unwrap());
        assert_eq!(v.field_i64("bench").unwrap(), 1);
        assert!(v.path(&["a", "x"]).is_none());
        assert!(Value::parse("[1,]").is_err());
        assert!(Value::parse("{\"a\":1} x").is_err());
    }
}
