//! A minimal JSON value and writer, so the measurement records need no
//! dependency. Objects keep insertion order.

use std::fmt::Write as _;

#[derive(Clone, Debug)]
pub enum J {
    Null,
    Bool(bool),
    Int(i64),
    Num(f64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    pub fn obj() -> J {
        J::Obj(Vec::new())
    }

    /// Add (or append) a field to an object; panics on a non-object.
    pub fn set(&mut self, key: &str, value: impl Into<J>) -> &mut J {
        match self {
            J::Obj(fields) => fields.push((key.to_string(), value.into())),
            _ => panic!("J::set on a non-object"),
        }
        self
    }

    /// Builder form of [`J::set`].
    pub fn with(mut self, key: &str, value: impl Into<J>) -> J {
        self.set(key, value);
        self
    }

    pub fn push(&mut self, value: impl Into<J>) {
        match self {
            J::Arr(items) => items.push(value.into()),
            _ => panic!("J::push on a non-array"),
        }
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            J::Null => out.push_str("null"),
            J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            J::Int(i) => {
                let _ = write!(out, "{i}");
            }
            J::Num(x) => {
                if x.is_finite() {
                    let _ = write!(out, "{x}");
                } else {
                    out.push_str("null");
                }
            }
            J::Str(s) => write_str(out, s),
            J::Arr(items) => {
                out.push('[');
                for (n, item) in items.iter().enumerate() {
                    if n > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            J::Obj(fields) => {
                out.push('{');
                for (n, (k, v)) in fields.iter().enumerate() {
                    if n > 0 {
                        out.push(',');
                    }
                    write_str(out, k);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A byte as the string "0xNN" (records keep register values readable).
pub fn hex8(v: u8) -> J {
    J::Str(format!("0x{v:02X}"))
}

pub fn hex16(v: u16) -> J {
    J::Str(format!("0x{v:04X}"))
}

pub fn hex32(v: u32) -> J {
    J::Str(format!("0x{v:08X}"))
}

pub fn hex_bytes(bytes: &[u8]) -> J {
    J::Arr(bytes.iter().map(|b| hex8(*b)).collect())
}

impl From<bool> for J {
    fn from(v: bool) -> J {
        J::Bool(v)
    }
}
impl From<i64> for J {
    fn from(v: i64) -> J {
        J::Int(v)
    }
}
impl From<u64> for J {
    fn from(v: u64) -> J {
        J::Int(v as i64)
    }
}
impl From<u32> for J {
    fn from(v: u32) -> J {
        J::Int(v as i64)
    }
}
impl From<u16> for J {
    fn from(v: u16) -> J {
        J::Int(v as i64)
    }
}
impl From<u8> for J {
    fn from(v: u8) -> J {
        J::Int(v as i64)
    }
}
impl From<usize> for J {
    fn from(v: usize) -> J {
        J::Int(v as i64)
    }
}
impl From<i32> for J {
    fn from(v: i32) -> J {
        J::Int(v as i64)
    }
}
impl From<f64> for J {
    fn from(v: f64) -> J {
        J::Num(v)
    }
}
impl From<&str> for J {
    fn from(v: &str) -> J {
        J::Str(v.to_string())
    }
}
impl From<String> for J {
    fn from(v: String) -> J {
        J::Str(v)
    }
}
impl From<Vec<J>> for J {
    fn from(v: Vec<J>) -> J {
        J::Arr(v)
    }
}
impl<T: Into<J>> From<Option<T>> for J {
    fn from(v: Option<T>) -> J {
        match v {
            Some(v) => v.into(),
            None => J::Null,
        }
    }
}
