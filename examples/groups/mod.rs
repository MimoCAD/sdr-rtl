//! A register group file applied on top of the driver's own setup, for
//! the isolation A/B (which block of another configuration changes what
//! the receiver rejects): `--regs FILE --group NAME …` on `ifsweep` and
//! `record`. The file holds the values; this module holds none. It is
//! JSON (no dependency: the reader is below):
//!
//!   {"groups": [{"name": "...", "writes": [ <write>, ... ]}, ...]}
//!
//! with each write one of (numbers decimal):
//!   {"op":"demod","page":P,"reg":R,"data":[...]}       consecutive bytes from R
//!   {"op":"demod_bits","page":P,"reg":R,"mask":M,"value":V}
//!   {"op":"tuner","reg":R,"value":V}                    R820T register, whole byte
//!   {"op":"tuner_bits","reg":R,"mask":M,"value":V}
//!   {"op":"sys","off":O,"value":V}                      system block
//!   {"op":"sys_bits","off":O,"mask":M,"value":V}        (and "usb_bits")
//!   {"op":"usb","off":O,"data":[...]}                   USB block
//!   {"op":"taps","regs":[20 bytes]}                     page 1 0x1C-0x2F image
//!   {"op":"if_hz","hz":F}                               the IF the LO is placed for
//! A write may carry "freq_dependent": true and "tuned_hz": F; such writes
//! are left out unless `--freq-dependent` is given and F is within
//! FREQ_MATCH_HZ of the centre.
//!
//! How each kind is placed, since the stream's start sequence
//! (`stream::start_sequence`) rewrites its own registers:
//! - `if_hz`: through `demod::set_bench_if_hz` before the open, so the
//!   open's DDC, the start sequence and the LO all follow it (a DDC IF
//!   written into a running stream takes no effect, `iftest`).
//! - `taps`: the stream is opened with them (`rx_stream_with_taps`).
//! - everything else: written after the open, the tune and the gains,
//!   before the stream; demodulator and system writes whose register the
//!   start sequence also writes are written again after the start when
//!   `--after-start` is given. Demodulator, system and USB registers are
//!   read first and written back by `restore` (a later open does not
//!   rewrite them all); the tuner's whole image is rewritten by every open.
//! - `tuner_bits` on R16-R31 (which do not read back, findings step 4a)
//!   take the rest of the byte from `--base FILE`'s whole-byte write of
//!   that register (the driver's own setup in the same form).
//!
//! What is printed names groups, registers and counts, never a value.

#![allow(dead_code)]

use sdr_rtl::blocking::Rtl;
use sdr_rtl::protocol::{
    ACCESS_SET, BLOCK_SYS, DEMOD_CTL, FREQ_OFFSET_FIRST, FREQ_OFFSET_LAST, FREQ_OFFSET_PAGE,
    IFFREQ_MSB, IFFREQ_PAGE, RATE_STEADY_OFF, RATE_STEADY_PAGE, RSAMP_PAGE, SPEC_INV_OFF,
    SPEC_INV_PAGE, START_OFF, START_PAGE, STREAM_ENABLE_OFF, STREAM_ENABLE_PAGE, TAP_FIRST,
    TAP_PAGE, windex, wvalue_sys,
};
use sdr_rtl::taps::TAP_REGS;

/// How near the centre a frequency-dependent write's tune must be.
pub const FREQ_MATCH_HZ: f64 = 2.0e6;
/// The tuner's registers that read back (R0-R15, findings step 4a).
const TUNER_READBACK: usize = 16;
/// The resampler's four registers (page 1 0x9F-0xA2, `demod::rsamp_transfers`).
const RSAMP_FIRST: u8 = 0x9F;
const RSAMP_LAST: u8 = 0xA2;
/// The IF word's three registers from IFFREQ_MSB.
const IFFREQ_REGS: u8 = 3;

// ---- A minimal JSON reader ----

#[derive(Clone, Debug)]
pub enum J {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    pub fn get(&self, key: &str) -> Option<&J> {
        match self {
            J::Obj(f) => f.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn num(&self) -> Option<f64> {
        match self {
            J::Num(n) => Some(*n),
            _ => None,
        }
    }
    pub fn str(&self) -> Option<&str> {
        match self {
            J::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn arr(&self) -> Option<&[J]> {
        match self {
            J::Arr(a) => Some(a),
            _ => None,
        }
    }
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn ws(&mut self) {
        while self.at < self.b.len() && self.b[self.at].is_ascii_whitespace() {
            self.at += 1;
        }
    }
    fn eat(&mut self, c: u8) -> Result<(), String> {
        self.ws();
        if self.b.get(self.at) == Some(&c) {
            self.at += 1;
            Ok(())
        } else {
            Err(format!(
                "JSON: expected '{}' at byte {}",
                c as char, self.at
            ))
        }
    }
    fn value(&mut self) -> Result<J, String> {
        self.ws();
        match self.b.get(self.at) {
            Some(b'{') => {
                self.at += 1;
                let mut f = Vec::new();
                self.ws();
                if self.b.get(self.at) == Some(&b'}') {
                    self.at += 1;
                    return Ok(J::Obj(f));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    self.eat(b':')?;
                    f.push((k, self.value()?));
                    self.ws();
                    match self.b.get(self.at) {
                        Some(b',') => self.at += 1,
                        Some(b'}') => {
                            self.at += 1;
                            return Ok(J::Obj(f));
                        }
                        _ => return Err(format!("JSON: bad object at byte {}", self.at)),
                    }
                }
            }
            Some(b'[') => {
                self.at += 1;
                let mut a = Vec::new();
                self.ws();
                if self.b.get(self.at) == Some(&b']') {
                    self.at += 1;
                    return Ok(J::Arr(a));
                }
                loop {
                    a.push(self.value()?);
                    self.ws();
                    match self.b.get(self.at) {
                        Some(b',') => self.at += 1,
                        Some(b']') => {
                            self.at += 1;
                            return Ok(J::Arr(a));
                        }
                        _ => return Err(format!("JSON: bad array at byte {}", self.at)),
                    }
                }
            }
            Some(b'"') => Ok(J::Str(self.string()?)),
            Some(b't') if self.b[self.at..].starts_with(b"true") => {
                self.at += 4;
                Ok(J::Bool(true))
            }
            Some(b'f') if self.b[self.at..].starts_with(b"false") => {
                self.at += 5;
                Ok(J::Bool(false))
            }
            Some(b'n') if self.b[self.at..].starts_with(b"null") => {
                self.at += 4;
                Ok(J::Null)
            }
            Some(_) => {
                let start = self.at;
                while self.at < self.b.len()
                    && matches!(
                        self.b[self.at],
                        b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9'
                    )
                {
                    self.at += 1;
                }
                std::str::from_utf8(&self.b[start..self.at])
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .map(J::Num)
                    .ok_or_else(|| format!("JSON: bad value at byte {start}"))
            }
            None => Err("JSON: unexpected end".into()),
        }
    }
    fn string(&mut self) -> Result<String, String> {
        self.eat(b'"')?;
        let mut s = String::new();
        loop {
            match self.b.get(self.at) {
                Some(b'"') => {
                    self.at += 1;
                    return Ok(s);
                }
                Some(b'\\') => {
                    self.at += 1;
                    let c = *self.b.get(self.at).ok_or("JSON: unexpected end")?;
                    self.at += 1;
                    match c {
                        b'n' => s.push('\n'),
                        b't' => s.push('\t'),
                        b'u' => {
                            let h = std::str::from_utf8(
                                self.b.get(self.at..self.at + 4).ok_or("JSON: short \\u")?,
                            )
                            .map_err(|e| e.to_string())?;
                            s.push(
                                u32::from_str_radix(h, 16)
                                    .ok()
                                    .and_then(char::from_u32)
                                    .unwrap_or('?'),
                            );
                            self.at += 4;
                        }
                        other => s.push(other as char),
                    }
                }
                Some(_) => {
                    let start = self.at;
                    while self.at < self.b.len() && !matches!(self.b[self.at], b'"' | b'\\') {
                        self.at += 1;
                    }
                    s.push_str(&String::from_utf8_lossy(&self.b[start..self.at]));
                }
                None => return Err("JSON: unterminated string".into()),
            }
        }
    }
}

pub fn parse(text: &str) -> Result<J, String> {
    let mut r = Reader {
        b: text.as_bytes(),
        at: 0,
    };
    r.value()
}

// ---- The writes ----

#[derive(Clone, Debug)]
pub enum Op {
    Demod {
        page: u8,
        reg: u8,
        data: Vec<u8>,
    },
    DemodBits {
        page: u8,
        reg: u8,
        mask: u8,
        value: u8,
    },
    Tuner {
        reg: u8,
        value: u8,
    },
    TunerBits {
        reg: u8,
        mask: u8,
        value: u8,
    },
    Sys {
        off: u16,
        value: u8,
    },
    SysBits {
        off: u16,
        mask: u8,
        value: u8,
    },
    UsbBits {
        off: u16,
        mask: u8,
        value: u8,
    },
    Usb {
        off: u16,
        data: Vec<u8>,
    },
    Taps([u8; TAP_REGS]),
    IfHz(f64),
}

#[derive(Clone, Debug)]
pub struct Write {
    pub group: String,
    pub op: Op,
    /// The tune a frequency-dependent write was taken at.
    pub tuned_hz: Option<f64>,
}

fn int(w: &J, k: &str) -> Result<u32, String> {
    w.get(k)
        .and_then(J::num)
        .map(|v| v as u32)
        .ok_or_else(|| format!("write lacks \"{k}\""))
}

fn bytes(w: &J, k: &str) -> Result<Vec<u8>, String> {
    w.get(k)
        .and_then(J::arr)
        .ok_or_else(|| format!("write lacks \"{k}\""))?
        .iter()
        .map(|v| {
            v.num()
                .map(|n| n as u8)
                .ok_or_else(|| format!("\"{k}\": not a number"))
        })
        .collect()
}

fn op_of(w: &J) -> Result<Op, String> {
    let kind = w.get("op").and_then(J::str).ok_or("write lacks \"op\"")?;
    Ok(match kind {
        "demod" => Op::Demod {
            page: int(w, "page")? as u8,
            reg: int(w, "reg")? as u8,
            data: bytes(w, "data")?,
        },
        "demod_bits" => Op::DemodBits {
            page: int(w, "page")? as u8,
            reg: int(w, "reg")? as u8,
            mask: int(w, "mask")? as u8,
            value: int(w, "value")? as u8,
        },
        "tuner" => Op::Tuner {
            reg: int(w, "reg")? as u8,
            value: int(w, "value")? as u8,
        },
        "tuner_bits" => Op::TunerBits {
            reg: int(w, "reg")? as u8,
            mask: int(w, "mask")? as u8,
            value: int(w, "value")? as u8,
        },
        "sys" => Op::Sys {
            off: int(w, "off")? as u16,
            value: int(w, "value")? as u8,
        },
        "sys_bits" => Op::SysBits {
            off: int(w, "off")? as u16,
            mask: int(w, "mask")? as u8,
            value: int(w, "value")? as u8,
        },
        "usb_bits" => Op::UsbBits {
            off: int(w, "off")? as u16,
            mask: int(w, "mask")? as u8,
            value: int(w, "value")? as u8,
        },
        "usb" => Op::Usb {
            off: int(w, "off")? as u16,
            data: bytes(w, "data")?,
        },
        "taps" => Op::Taps(
            bytes(w, "regs")?
                .try_into()
                .map_err(|_| format!("taps: {TAP_REGS} registers"))?,
        ),
        "if_hz" => Op::IfHz(w.get("hz").and_then(J::num).ok_or("if_hz lacks \"hz\"")?),
        other => return Err(format!("unknown op {other}")),
    })
}

/// Every write of a file, with its group's name.
pub fn load(path: &str) -> Result<Vec<Write>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let root = parse(&text)?;
    let mut out = Vec::new();
    for g in root
        .get("groups")
        .and_then(J::arr)
        .ok_or("no \"groups\" array")?
    {
        let name = g
            .get("name")
            .and_then(J::str)
            .ok_or("group lacks \"name\"")?
            .to_string();
        for w in g.get("writes").and_then(J::arr).unwrap_or(&[]) {
            let dependent = matches!(w.get("freq_dependent"), Some(J::Bool(true)));
            let tuned_hz = if dependent {
                Some(w.get("tuned_hz").and_then(J::num).unwrap_or(f64::NAN))
            } else {
                None
            };
            out.push(Write {
                group: name.clone(),
                op: op_of(w)?,
                tuned_hz,
            });
        }
    }
    Ok(out)
}

/// The group names of a file, in order.
pub fn names(path: &str) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let root = parse(&text)?;
    Ok(root
        .get("groups")
        .and_then(J::arr)
        .ok_or("no \"groups\" array")?
        .iter()
        .filter_map(|g| g.get("name").and_then(J::str).map(String::from))
        .collect())
}

/// What the command line asked for.
#[derive(Clone, Debug, Default)]
pub struct Args {
    pub file: Option<String>,
    /// Group names; "all" for every group of the file.
    pub groups: Vec<String>,
    /// Groups left out (of "all", for a leave-one-out).
    pub except: Vec<String>,
    pub base: Option<String>,
    pub freq_dependent: bool,
    pub after_start: bool,
}

impl Args {
    /// `--regs FILE`, `--group NAME` (repeatable; "all"), `--except NAME`
    /// (repeatable), `--base FILE`, `--freq-dependent`, `--after-start`.
    /// Returns the arguments left over.
    pub fn take(args: &[String]) -> (Args, Vec<String>) {
        let mut a = Args::default();
        let mut rest = Vec::new();
        let mut it = args.iter();
        while let Some(x) = it.next() {
            match x.as_str() {
                "--regs" => a.file = it.next().cloned(),
                "--group" => a.groups.extend(it.next().cloned()),
                "--except" => a.except.extend(it.next().cloned()),
                "--base" => a.base = it.next().cloned(),
                "--freq-dependent" => a.freq_dependent = true,
                "--after-start" => a.after_start = true,
                _ => rest.push(x.clone()),
            }
        }
        (a, rest)
    }
}

/// The chosen writes for a centre frequency, and how they will be placed.
#[derive(Debug, Default)]
pub struct Plan {
    pub groups: Vec<String>,
    pub if_hz: Option<f64>,
    pub taps: Option<[u8; TAP_REGS]>,
    pub writes: Vec<Write>,
    /// Frequency-dependent writes left out.
    pub skipped: usize,
    base: Vec<Write>,
    saved: Vec<(Op, Vec<u8>)>,
}

impl Plan {
    pub fn new(a: &Args, center: f64) -> Result<Plan, String> {
        let mut p = Plan::default();
        let Some(file) = &a.file else { return Ok(p) };
        let all = a.groups.iter().any(|g| g == "all");
        let known = names(file)?;
        // A name may spell its spaces as '_' (a command line split on
        // whitespace, as `rtl_ab_run --new-template` splits its words).
        let resolve = |g: &String| -> Result<String, String> {
            if g == "all" || known.contains(g) {
                return Ok(g.clone());
            }
            let spaced = g.replace('_', " ");
            if known.contains(&spaced) {
                return Ok(spaced);
            }
            Err(format!(
                "no group \"{g}\" in {file} (groups: {})",
                known.join(" | ")
            ))
        };
        let chosen = a
            .groups
            .iter()
            .map(resolve)
            .collect::<Result<Vec<_>, _>>()?;
        let except = a
            .except
            .iter()
            .map(resolve)
            .collect::<Result<Vec<_>, _>>()?;
        p.groups = known
            .iter()
            .filter(|n| (all || chosen.contains(n)) && !except.contains(n))
            .cloned()
            .collect();
        for w in load(file)? {
            if !p.groups.contains(&w.group) {
                continue;
            }
            if let Some(t) = w.tuned_hz
                && !(a.freq_dependent && (t - center).abs() <= FREQ_MATCH_HZ)
            {
                p.skipped += 1;
                continue;
            }
            match w.op {
                Op::IfHz(hz) => p.if_hz = Some(hz),
                Op::Taps(r) => p.taps = Some(r),
                _ => p.writes.push(w),
            }
        }
        if let Some(b) = &a.base {
            p.base = load(b)?
                .into_iter()
                .filter(|w| {
                    w.tuned_hz
                        .is_none_or(|t| (t - center).abs() <= FREQ_MATCH_HZ)
                })
                .collect();
        }
        Ok(p)
    }

    /// Before the open: the IF.
    pub fn before_open(&self) {
        if let Some(hz) = self.if_hz {
            sdr_rtl::demod::set_bench_if_hz(Some(hz));
        }
    }

    /// A tuner register R16-R31 from the base file: its whole-byte write
    /// and field writes folded together; every bit outside `mask` (the
    /// bits about to be written) must be covered.
    fn base_tuner(&self, reg: u8, mask: u8) -> Result<u8, String> {
        let (mut v, mut covered) = (0u8, 0u8);
        for w in &self.base {
            match w.op {
                Op::Tuner { reg: r, value } if r == reg => (v, covered) = (value, 0xFF),
                Op::TunerBits {
                    reg: r,
                    mask: m,
                    value,
                } if r == reg => (v, covered) = ((v & !m) | (value & m), covered | m),
                _ => {}
            }
        }
        if covered | mask != 0xFF {
            return Err(format!(
                "tuner_bits on R{reg}: it does not read back and --base does not cover the rest of it"
            ));
        }
        Ok(v)
    }

    /// After the open, the tune and the gains, before the stream: every
    /// write, demodulator/system/USB registers read first (for `restore`).
    pub fn apply(&mut self, rtl: &Rtl) -> Result<(), String> {
        let link = rtl.link();
        let e = |e: sdr_core::Error| e.to_string();
        for w in &self.writes {
            match &w.op {
                Op::Demod { page, reg, data } => {
                    let was = link.demod_read(*page, *reg, data.len() as u16).map_err(e)?;
                    self.saved.push((w.op.clone(), was));
                    link.demod_write(*page, *reg, data).map_err(e)?;
                }
                Op::DemodBits {
                    page,
                    reg,
                    mask,
                    value,
                } => {
                    let was = link.demod_read(*page, *reg, 1).map_err(e)?;
                    self.saved.push((w.op.clone(), was));
                    link.demod_rmw8(*page, *reg, *mask, *value).map_err(e)?;
                }
                Op::Sys { off, value } => {
                    let was = link.sys_read8(*off).map_err(e)?;
                    self.saved.push((w.op.clone(), vec![was]));
                    link.set(
                        wvalue_sys(*off),
                        windex(BLOCK_SYS, ACCESS_SET, 0),
                        &[*value],
                        "system register write",
                    )
                    .map_err(e)?;
                }
                Op::Usb { off, data } => {
                    let was = link.usb_read_n(*off, data.len() as u16).map_err(e)?;
                    self.saved.push((w.op.clone(), was));
                    link.usb_write_n(*off, data).map_err(e)?;
                }
                Op::SysBits { off, mask, value } => {
                    let was = link.sys_read8(*off).map_err(e)?;
                    self.saved.push((w.op.clone(), vec![was]));
                    link.set(
                        wvalue_sys(*off),
                        windex(BLOCK_SYS, ACCESS_SET, 0),
                        &[(was & !mask) | (value & mask)],
                        "system register write",
                    )
                    .map_err(e)?;
                }
                Op::UsbBits { off, mask, value } => {
                    let was = link.usb_read8(*off).map_err(e)?;
                    self.saved.push((w.op.clone(), vec![was]));
                    link.usb_write8(*off, (was & !mask) | (value & mask))
                        .map_err(e)?;
                }
                Op::Tuner { reg, value } => link
                    .with_tuner(|l| l.tuner_write(*reg, *value))
                    .map_err(e)?,
                Op::TunerBits { reg, mask, value } => {
                    let was = if (*reg as usize) < TUNER_READBACK {
                        link.with_tuner(|l| l.tuner_read(TUNER_READBACK as u16))
                            .map_err(e)?[*reg as usize]
                    } else {
                        self.base_tuner(*reg, *mask)?
                    };
                    link.with_tuner(|l| l.tuner_write(*reg, (was & !mask) | (value & mask)))
                        .map_err(e)?;
                }
                Op::Taps(_) | Op::IfHz(_) => {}
            }
        }
        Ok(())
    }

    /// Whether a demodulator register is one the start sequence writes.
    fn start_owned(page: u8, reg: u8) -> bool {
        (page == FREQ_OFFSET_PAGE && (FREQ_OFFSET_FIRST..=FREQ_OFFSET_LAST).contains(&reg))
            || (page == IFFREQ_PAGE && (IFFREQ_MSB..IFFREQ_MSB + IFFREQ_REGS).contains(&reg))
            || (page == SPEC_INV_PAGE && reg == SPEC_INV_OFF)
            || (page == RSAMP_PAGE && (RSAMP_FIRST..=RSAMP_LAST).contains(&reg))
            || (page == TAP_PAGE && (TAP_FIRST..TAP_FIRST + TAP_REGS as u8).contains(&reg))
            || (page == STREAM_ENABLE_PAGE && reg == STREAM_ENABLE_OFF)
            || (page == RATE_STEADY_PAGE && reg == RATE_STEADY_OFF)
            || (page == START_PAGE && reg == START_OFF)
    }

    /// The writes whose registers the start sequence rewrites.
    pub fn start_owned_writes(&self) -> Vec<String> {
        self.writes
            .iter()
            .filter_map(|w| match &w.op {
                Op::Demod { page, reg, data }
                    if (0..data.len()).any(|k| Self::start_owned(*page, reg + k as u8)) =>
                {
                    Some(format!("{}: page {page} reg {reg}+{}", w.group, data.len()))
                }
                Op::DemodBits { page, reg, .. } if Self::start_owned(*page, *reg) => {
                    Some(format!("{}: page {page} reg {reg} (bits)", w.group))
                }
                Op::Sys { off, .. } if *off == DEMOD_CTL => {
                    Some(format!("{}: system DEMOD_CTL", w.group))
                }
                _ => None,
            })
            .collect()
    }

    /// With `--after-start`: the start-owned demodulator writes again,
    /// into the running stream.
    pub fn after_start(&self, rtl: &Rtl) -> Result<usize, String> {
        let link = rtl.link();
        let mut n = 0;
        for w in &self.writes {
            match &w.op {
                Op::Demod { page, reg, data }
                    if (0..data.len()).any(|k| Self::start_owned(*page, reg + k as u8)) =>
                {
                    link.demod_write(*page, *reg, data)
                        .map_err(|e| e.to_string())?;
                    n += 1;
                }
                Op::DemodBits {
                    page,
                    reg,
                    mask,
                    value,
                } if Self::start_owned(*page, *reg) => {
                    link.demod_rmw8(*page, *reg, *mask, *value)
                        .map_err(|e| e.to_string())?;
                    n += 1;
                }
                _ => {}
            }
        }
        Ok(n)
    }

    /// Demodulator registers read back now against the plan: how many
    /// bytes differ, and where (no values).
    pub fn check(&self, rtl: &Rtl) -> Result<Vec<String>, String> {
        let link = rtl.link();
        let mut differ = Vec::new();
        for w in &self.writes {
            match &w.op {
                Op::Demod { page, reg, data } => {
                    let back = link
                        .demod_read(*page, *reg, data.len() as u16)
                        .map_err(|e| e.to_string())?;
                    for (k, (b, d)) in back.iter().zip(data).enumerate() {
                        if b != d {
                            differ.push(format!("page {page} reg {}", *reg as usize + k));
                        }
                    }
                }
                Op::DemodBits {
                    page,
                    reg,
                    mask,
                    value,
                } => {
                    let b = link.demod_read8(*page, *reg).map_err(|e| e.to_string())?;
                    if b & mask != value & mask {
                        differ.push(format!("page {page} reg {reg} (bits)"));
                    }
                }
                Op::Tuner { reg, value } if (*reg as usize) < TUNER_READBACK => {
                    let b = link
                        .with_tuner(|l| l.tuner_read(TUNER_READBACK as u16))
                        .map_err(|e| e.to_string())?[*reg as usize];
                    if b != *value {
                        differ.push(format!("tuner R{reg}"));
                    }
                }
                _ => {}
            }
        }
        Ok(differ)
    }

    /// The demodulator, system and USB registers as they were before
    /// `apply`, written back in reverse order.
    pub fn restore(&mut self, rtl: &Rtl) -> Result<(), String> {
        let link = rtl.link();
        for (op, was) in self.saved.drain(..).rev() {
            match op {
                Op::Demod { page, reg, .. } | Op::DemodBits { page, reg, .. } => {
                    link.demod_write(page, reg, &was)
                }
                Op::Sys { off, .. } | Op::SysBits { off, .. } => link.set(
                    wvalue_sys(off),
                    windex(BLOCK_SYS, ACCESS_SET, 0),
                    &was,
                    "system register write",
                ),
                Op::Usb { off, .. } | Op::UsbBits { off, .. } => link.usb_write_n(off, &was),
                _ => Ok(()),
            }
            .map_err(|e| e.to_string())?;
        }
        sdr_rtl::demod::set_bench_if_hz(None);
        Ok(())
    }

    /// One JSON line about the plan: groups, counts, nothing more.
    pub fn summary(&self) -> String {
        let q = |s: &str| format!("{s:?}");
        format!(
            "{{\"kind\":\"regs\",\"groups\":[{}],\"writes\":{},\"if_override\":{},\"taps_override\":{},\"freq_dependent_skipped\":{}}}",
            self.groups
                .iter()
                .map(|g| q(g))
                .collect::<Vec<_>>()
                .join(","),
            self.writes.len(),
            self.if_hz.is_some(),
            self.taps.is_some(),
            self.skipped
        )
    }
}
