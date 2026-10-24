//! rom51: a minimal MCS-51 (8051) disassembler, written from the
//! architecture's public instruction set (Intel's MCS-51 opcode map: 255
//! opcodes, 1-3 bytes each) with nothing chip-specific in it. Only the
//! standard 8051/8052 special function registers are named; every other
//! direct address prints as a number.
//!
//! It walks the code from the reset and interrupt vectors (and any
//! `--entry` given), following jumps, calls and branches (recursive
//! descent), so bytes no path reaches print as data rather than as
//! mis-decoded instructions. Within a straight run it tracks a constant
//! DPTR (set by MOV DPTR,#data16, stepped by INC DPTR) and prints the
//! external-data address each MOVX touches.
//!
//! ```text
//! cargo run --release -p sdr-rtl --example rom51 -- IMAGE [--base ADDR] [--entry ADDR]... [--xref]
//! ```
//! `--xref` prints, after the listing, every MOVX address with the
//! instructions that touch it, every call target with its callers, and
//! every indirect jump (JMP @A+DPTR) that the walk could not follow.
//!
//! The tool's OUTPUT, when run on a vendor's firmware, belongs wherever
//! that firmware is kept; this source carries no part of any image.

use std::collections::{BTreeMap, BTreeSet};

/// The interrupt vectors of the 8051 (reset, INT0, T0, INT1, T1, serial)
/// and the 8052's timer 2.
const VECTORS: [u16; 7] = [0x0000, 0x0003, 0x000B, 0x0013, 0x001B, 0x0023, 0x002B];

/// The standard 8051/8052 special function registers.
fn sfr_name(a: u8) -> Option<&'static str> {
    Some(match a {
        0x80 => "P0",
        0x81 => "SP",
        0x82 => "DPL",
        0x83 => "DPH",
        0x87 => "PCON",
        0x88 => "TCON",
        0x89 => "TMOD",
        0x8A => "TL0",
        0x8B => "TL1",
        0x8C => "TH0",
        0x8D => "TH1",
        0x90 => "P1",
        0x98 => "SCON",
        0x99 => "SBUF",
        0xA0 => "P2",
        0xA8 => "IE",
        0xB0 => "P3",
        0xB8 => "IP",
        0xC8 => "T2CON",
        0xCA => "RCAP2L",
        0xCB => "RCAP2H",
        0xCC => "TL2",
        0xCD => "TH2",
        0xD0 => "PSW",
        0xE0 => "ACC",
        0xF0 => "B",
        _ => return None,
    })
}

/// The standard named bits (TCON, SCON, IE, IP, PSW, T2CON).
fn bit_name(b: u8) -> Option<&'static str> {
    Some(match b {
        0x88 => "IT0",
        0x89 => "IE0",
        0x8A => "IT1",
        0x8B => "IE1",
        0x8C => "TR0",
        0x8D => "TF0",
        0x8E => "TR1",
        0x8F => "TF1",
        0x98 => "RI",
        0x99 => "TI",
        0xA8 => "EX0",
        0xA9 => "ET0",
        0xAA => "EX1",
        0xAB => "ET1",
        0xAC => "ES",
        0xAD => "ET2",
        0xAF => "EA",
        0xD0 => "P",
        0xD2 => "OV",
        0xD3 => "RS0",
        0xD4 => "RS1",
        0xD5 => "F0",
        0xD6 => "AC",
        0xD7 => "CY",
        0xCA => "TR2",
        0xCF => "TF2",
        _ => return None,
    })
}

fn direct(a: u8) -> String {
    match sfr_name(a) {
        Some(n) => n.to_string(),
        None => format!("{a:02X}h"),
    }
}

fn bit(b: u8) -> String {
    if let Some(n) = bit_name(b) {
        return n.to_string();
    }
    if b < 0x80 {
        format!("{:02X}h.{}", 0x20 + b / 8, b % 8)
    } else {
        format!("{}.{}", direct(b & 0xF8), b & 7)
    }
}

/// How control leaves an instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flow {
    /// Falls through to the next instruction.
    Next,
    /// Jumps only (no fall-through).
    Jump(u16),
    /// Branches: the target or the next instruction.
    Branch(u16),
    /// Calls: the target, then the next instruction.
    Call(u16),
    /// Returns, or jumps somewhere the walk cannot know.
    Stop,
    /// JMP @A+DPTR.
    Indirect,
}

struct Insn {
    len: u16,
    text: String,
    flow: Flow,
    /// MOV DPTR,#data16.
    sets_dptr: Option<u16>,
    inc_dptr: bool,
    /// MOVX through DPTR: Some(true) write, Some(false) read.
    movx_dptr: Option<bool>,
    /// Any other write of DPL/DPH (the tracked constant is lost).
    clobbers_dptr: bool,
}

fn rel(pc: u16, len: u16, r: u8) -> u16 {
    pc.wrapping_add(len).wrapping_add(r as i8 as i16 as u16)
}

fn decode(mem: &[u8], base: u16, pc: u16) -> Option<Insn> {
    let at = |o: u16| -> Option<u8> {
        mem.get(pc.wrapping_sub(base).wrapping_add(o) as usize)
            .copied()
    };
    let op = at(0)?;
    let b1 = at(1).unwrap_or(0);
    let b2 = at(2).unwrap_or(0);
    let r = op & 7;
    let ri = op & 1;
    let mut i = Insn {
        len: 1,
        text: String::new(),
        flow: Flow::Next,
        sets_dptr: None,
        inc_dptr: false,
        movx_dptr: None,
        clobbers_dptr: false,
    };
    let (len, text): (u16, String) = match op {
        0x00 => (1, "NOP".into()),
        _ if op & 0x1F == 0x01 => {
            let t = (pc.wrapping_add(2) & 0xF800) | ((op as u16 >> 5) << 8) | b1 as u16;
            i.flow = Flow::Jump(t);
            (2, format!("AJMP {t:04X}h"))
        }
        _ if op & 0x1F == 0x11 => {
            let t = (pc.wrapping_add(2) & 0xF800) | ((op as u16 >> 5) << 8) | b1 as u16;
            i.flow = Flow::Call(t);
            (2, format!("ACALL {t:04X}h"))
        }
        0x02 => {
            let t = (b1 as u16) << 8 | b2 as u16;
            i.flow = Flow::Jump(t);
            (3, format!("LJMP {t:04X}h"))
        }
        0x03 => (1, "RR A".into()),
        0x04 => (1, "INC A".into()),
        0x05 => (2, format!("INC {}", direct(b1))),
        0x06 | 0x07 => (1, format!("INC @R{ri}")),
        0x08..=0x0F => (1, format!("INC R{r}")),
        0x10 => {
            let t = rel(pc, 3, b2);
            i.flow = Flow::Branch(t);
            (3, format!("JBC {},{t:04X}h", bit(b1)))
        }
        0x12 => {
            let t = (b1 as u16) << 8 | b2 as u16;
            i.flow = Flow::Call(t);
            (3, format!("LCALL {t:04X}h"))
        }
        0x13 => (1, "RRC A".into()),
        0x14 => (1, "DEC A".into()),
        0x15 => (2, format!("DEC {}", direct(b1))),
        0x16 | 0x17 => (1, format!("DEC @R{ri}")),
        0x18..=0x1F => (1, format!("DEC R{r}")),
        0x20 => {
            let t = rel(pc, 3, b2);
            i.flow = Flow::Branch(t);
            (3, format!("JB {},{t:04X}h", bit(b1)))
        }
        0x22 => {
            i.flow = Flow::Stop;
            (1, "RET".into())
        }
        0x23 => (1, "RL A".into()),
        0x30 => {
            let t = rel(pc, 3, b2);
            i.flow = Flow::Branch(t);
            (3, format!("JNB {},{t:04X}h", bit(b1)))
        }
        0x32 => {
            i.flow = Flow::Stop;
            (1, "RETI".into())
        }
        0x33 => (1, "RLC A".into()),
        0x40 | 0x50 | 0x60 | 0x70 | 0x80 => {
            let t = rel(pc, 2, b1);
            let m = match op {
                0x40 => "JC",
                0x50 => "JNC",
                0x60 => "JZ",
                0x70 => "JNZ",
                _ => "SJMP",
            };
            i.flow = if op == 0x80 {
                Flow::Jump(t)
            } else {
                Flow::Branch(t)
            };
            (2, format!("{m} {t:04X}h"))
        }
        // The arithmetic/logic rows: ADD 2x, ADDC 3x, ORL 4x, ANL 5x,
        // XRL 6x, SUBB 9x — columns 4 #data, 5 direct, 6-7 @Ri, 8-F Rn.
        _ if matches!(op >> 4, 0x2 | 0x3 | 0x4 | 0x5 | 0x6 | 0x9) && op & 0x0F >= 4 => {
            let m = match op >> 4 {
                0x2 => "ADD",
                0x3 => "ADDC",
                0x4 => "ORL",
                0x5 => "ANL",
                0x6 => "XRL",
                _ => "SUBB",
            };
            match op & 0x0F {
                4 => (2, format!("{m} A,#{b1:02X}h")),
                5 => (2, format!("{m} A,{}", direct(b1))),
                6 | 7 => (1, format!("{m} A,@R{ri}")),
                _ => (1, format!("{m} A,R{r}")),
            }
        }
        0x42 | 0x52 | 0x62 => {
            let m = ["ORL", "ANL", "XRL"][(op >> 4) as usize - 4];
            i.clobbers_dptr = b1 == 0x82 || b1 == 0x83;
            (2, format!("{m} {},A", direct(b1)))
        }
        0x43 | 0x53 | 0x63 => {
            let m = ["ORL", "ANL", "XRL"][(op >> 4) as usize - 4];
            i.clobbers_dptr = b1 == 0x82 || b1 == 0x83;
            (3, format!("{m} {},#{b2:02X}h", direct(b1)))
        }
        0x72 => (2, format!("ORL C,{}", bit(b1))),
        0x73 => {
            i.flow = Flow::Indirect;
            (1, "JMP @A+DPTR".into())
        }
        0x74 => (2, format!("MOV A,#{b1:02X}h")),
        0x75 => {
            i.clobbers_dptr = b1 == 0x82 || b1 == 0x83;
            (3, format!("MOV {},#{b2:02X}h", direct(b1)))
        }
        0x76 | 0x77 => (2, format!("MOV @R{ri},#{b1:02X}h")),
        0x78..=0x7F => (2, format!("MOV R{r},#{b1:02X}h")),
        0x82 => (2, format!("ANL C,{}", bit(b1))),
        0x83 => (1, "MOVC A,@A+PC".into()),
        0x84 => (1, "DIV AB".into()),
        0x85 => {
            // Encoded source first, destination second.
            i.clobbers_dptr = b2 == 0x82 || b2 == 0x83;
            (3, format!("MOV {},{}", direct(b2), direct(b1)))
        }
        0x86 | 0x87 => {
            i.clobbers_dptr = b1 == 0x82 || b1 == 0x83;
            (2, format!("MOV {},@R{ri}", direct(b1)))
        }
        0x88..=0x8F => {
            i.clobbers_dptr = b1 == 0x82 || b1 == 0x83;
            (2, format!("MOV {},R{r}", direct(b1)))
        }
        0x90 => {
            let v = (b1 as u16) << 8 | b2 as u16;
            i.sets_dptr = Some(v);
            (3, format!("MOV DPTR,#{v:04X}h"))
        }
        0x92 => (2, format!("MOV {},C", bit(b1))),
        0x93 => (1, "MOVC A,@A+DPTR".into()),
        0xA0 => (2, format!("ORL C,/{}", bit(b1))),
        0xA2 => (2, format!("MOV C,{}", bit(b1))),
        0xA3 => {
            i.inc_dptr = true;
            (1, "INC DPTR".into())
        }
        0xA4 => (1, "MUL AB".into()),
        0xA5 => (1, "DB A5h ; reserved opcode".into()),
        0xA6 | 0xA7 => (2, format!("MOV @R{ri},{}", direct(b1))),
        0xA8..=0xAF => (2, format!("MOV R{r},{}", direct(b1))),
        0xB0 => (2, format!("ANL C,/{}", bit(b1))),
        0xB2 => (2, format!("CPL {}", bit(b1))),
        0xB3 => (1, "CPL C".into()),
        0xB4..=0xBF => {
            let t = rel(pc, 3, b2);
            i.flow = Flow::Branch(t);
            let lhs = match op {
                0xB4 => format!("A,#{b1:02X}h"),
                0xB5 => format!("A,{}", direct(b1)),
                0xB6 | 0xB7 => format!("@R{ri},#{b1:02X}h"),
                _ => format!("R{r},#{b1:02X}h"),
            };
            (3, format!("CJNE {lhs},{t:04X}h"))
        }
        0xC0 => (2, format!("PUSH {}", direct(b1))),
        0xC2 => (2, format!("CLR {}", bit(b1))),
        0xC3 => (1, "CLR C".into()),
        0xC4 => (1, "SWAP A".into()),
        0xC5 => {
            i.clobbers_dptr = b1 == 0x82 || b1 == 0x83;
            (2, format!("XCH A,{}", direct(b1)))
        }
        0xC6 | 0xC7 => (1, format!("XCH A,@R{ri}")),
        0xC8..=0xCF => (1, format!("XCH A,R{r}")),
        0xD0 => {
            i.clobbers_dptr = b1 == 0x82 || b1 == 0x83;
            (2, format!("POP {}", direct(b1)))
        }
        0xD2 => (2, format!("SETB {}", bit(b1))),
        0xD3 => (1, "SETB C".into()),
        0xD4 => (1, "DA A".into()),
        0xD5 => {
            let t = rel(pc, 3, b2);
            i.flow = Flow::Branch(t);
            i.clobbers_dptr = b1 == 0x82 || b1 == 0x83;
            (3, format!("DJNZ {},{t:04X}h", direct(b1)))
        }
        0xD6 | 0xD7 => (1, format!("XCHD A,@R{ri}")),
        0xD8..=0xDF => {
            let t = rel(pc, 2, b1);
            i.flow = Flow::Branch(t);
            (2, format!("DJNZ R{r},{t:04X}h"))
        }
        0xE0 => {
            i.movx_dptr = Some(false);
            (1, "MOVX A,@DPTR".into())
        }
        0xE2 | 0xE3 => (1, format!("MOVX A,@R{ri}")),
        0xE4 => (1, "CLR A".into()),
        0xE5 => (2, format!("MOV A,{}", direct(b1))),
        0xE6 | 0xE7 => (1, format!("MOV A,@R{ri}")),
        0xE8..=0xEF => (1, format!("MOV A,R{r}")),
        0xF0 => {
            i.movx_dptr = Some(true);
            (1, "MOVX @DPTR,A".into())
        }
        0xF2 | 0xF3 => (1, format!("MOVX @R{ri},A")),
        0xF4 => (1, "CPL A".into()),
        0xF5 => {
            i.clobbers_dptr = b1 == 0x82 || b1 == 0x83;
            (2, format!("MOV {},A", direct(b1)))
        }
        0xF6 | 0xF7 => (1, format!("MOV @R{ri},A")),
        0xF8..=0xFF => (1, format!("MOV R{r},A")),
        _ => (1, format!("DB {op:02X}h")),
    };
    // A 2- or 3-byte instruction must fit in the image.
    if (pc.wrapping_sub(base) as usize) + len as usize > mem.len() {
        return None;
    }
    i.len = len;
    i.text = text;
    Some(i)
}

fn parse_addr(s: &str) -> Option<u16> {
    let s = s.trim_end_matches('h');
    match s.strip_prefix("0x") {
        Some(h) => u16::from_str_radix(h, 16).ok(),
        None => u16::from_str_radix(s, 16).ok(),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!(
            "usage: rom51 IMAGE [--base ADDR] [--entry ADDR]... [--xref]   (addresses in hex)"
        );
        std::process::exit(2);
    };
    let mem = match std::fs::read(path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("rom51: {path}: {e}");
            std::process::exit(1);
        }
    };
    let mut base = 0u16;
    let mut entries: Vec<u16> = VECTORS.to_vec();
    let mut xref = false;
    let mut inline: BTreeMap<u16, u16> = BTreeMap::new();
    let mut k = 1;
    while k < args.len() {
        match args[k].as_str() {
            "--inline" if k + 1 < args.len() => {
                let parsed = args[k + 1]
                    .split_once(':')
                    .and_then(|(a, n)| Some((parse_addr(a)?, n.parse::<u16>().ok()?)));
                let Some((a, n)) = parsed else {
                    eprintln!("rom51: bad --inline {} (want ADDR:N)", args[k + 1]);
                    std::process::exit(2);
                };
                inline.insert(a, n);
                k += 2;
            }
            "--base" | "--entry" if k + 1 < args.len() => {
                let Some(a) = parse_addr(&args[k + 1]) else {
                    eprintln!("rom51: bad address {}", args[k + 1]);
                    std::process::exit(2);
                };
                if args[k] == "--base" {
                    base = a;
                } else {
                    entries.push(a);
                }
                k += 2;
            }
            "--xref" => {
                xref = true;
                k += 1;
            }
            other => {
                eprintln!("rom51: unknown argument {other}");
                std::process::exit(2);
            }
        }
    }
    let end = base as usize + mem.len();
    let inside = |a: u16| (a as usize) >= base as usize && (a as usize) < end;

    // Recursive descent: every reached instruction start.
    let mut code: BTreeMap<u16, Insn> = BTreeMap::new();
    let mut labels: BTreeSet<u16> = entries.iter().copied().collect();
    let mut callers: BTreeMap<u16, BTreeSet<u16>> = BTreeMap::new();
    let mut indirect: BTreeSet<u16> = BTreeSet::new();
    let mut outside: BTreeSet<(u16, u16)> = BTreeSet::new();
    let mut tables: BTreeMap<u16, Vec<u16>> = BTreeMap::new();
    let mut work: Vec<u16> = entries.clone();
    while let Some(start) = work.pop() {
        let mut pc = start;
        let mut dptr: Option<u16> = None;
        loop {
            if !inside(pc) {
                break;
            }
            if code.contains_key(&pc) {
                break;
            }
            let Some(ins) = decode(&mem, base, pc) else {
                break;
            };
            let mut next = pc.wrapping_add(ins.len);
            let flow = ins.flow;
            if let Some(v) = ins.sets_dptr {
                dptr = Some(v);
            } else if ins.inc_dptr {
                dptr = dptr.map(|d| d.wrapping_add(1));
            } else if ins.clobbers_dptr {
                dptr = None;
            }
            code.insert(pc, ins);
            if let Flow::Call(t) = flow {
                dptr = None;
                // A routine that reads its arguments from the bytes after
                // the call returns past them.
                if let Some(n) = inline.get(&t) {
                    next = next.wrapping_add(*n);
                }
            }
            // A JMP @A+DPTR on a constant DPTR that points at a run of
            // LJMP (3-byte) or AJMP (2-byte) instructions: a jump table;
            // each entry is walked as code.
            if flow == Flow::Indirect
                && let Some(t) = dptr
            {
                let op_at = |a: u16| {
                    if inside(a) {
                        Some(mem[(a - base) as usize])
                    } else {
                        None
                    }
                };
                let mut found = Vec::new();
                if op_at(t) == Some(0x02) {
                    let mut e = t;
                    while op_at(e) == Some(0x02) {
                        found.push(e);
                        e = e.wrapping_add(3);
                    }
                } else if op_at(t).is_some_and(|o| o & 0x1F == 0x01) {
                    let mut e = t;
                    while op_at(e).is_some_and(|o| o & 0x1F == 0x01) {
                        found.push(e);
                        e = e.wrapping_add(2);
                    }
                }
                for &e in &found {
                    labels.insert(e);
                    work.push(e);
                }
                if !found.is_empty() {
                    tables.insert(pc, found);
                }
            }
            let mut follow = |t: u16, from: u16, work: &mut Vec<u16>| {
                labels.insert(t);
                if inside(t) {
                    work.push(t);
                } else {
                    outside.insert((from, t));
                }
            };
            match flow {
                Flow::Next => pc = next,
                Flow::Jump(t) => {
                    follow(t, pc, &mut work);
                    break;
                }
                Flow::Branch(t) => {
                    follow(t, pc, &mut work);
                    pc = next;
                }
                Flow::Call(t) => {
                    callers.entry(t).or_default().insert(pc);
                    follow(t, pc, &mut work);
                    pc = next;
                }
                Flow::Stop => break,
                Flow::Indirect => {
                    indirect.insert(pc);
                    break;
                }
            }
        }
    }

    // The listing, with the DPTR constant tracked through straight runs.
    let mut movx: BTreeMap<u16, Vec<(u16, bool)>> = BTreeMap::new();
    let mut calls_with_dptr: BTreeMap<u16, Vec<u16>> = BTreeMap::new();
    let mut dptr: Option<u16> = None;
    let mut a = base as usize;
    let mut data_run: Vec<u8> = Vec::new();
    let mut data_at = 0usize;
    let flush = |run: &mut Vec<u8>, at: usize| {
        for (n, chunk) in run.chunks(16).enumerate() {
            let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02X}")).collect();
            println!("{:04X}:            DB {}", at + n * 16, hex.join(" "));
        }
        run.clear();
    };
    println!(
        "; rom51 listing of {path}: {} bytes at {base:04X}h, {} instructions reached from {} entries",
        mem.len(),
        code.len(),
        entries.len()
    );
    while a < end {
        let pc = a as u16;
        match code.get(&pc) {
            Some(ins) => {
                if !data_run.is_empty() {
                    flush(&mut data_run, data_at);
                }
                if labels.contains(&pc) {
                    dptr = None;
                    match callers.get(&pc) {
                        Some(c) => println!(
                            "\nL{pc:04X}:   ; called from {}",
                            c.iter()
                                .map(|x| format!("{x:04X}"))
                                .collect::<Vec<_>>()
                                .join(" ")
                        ),
                        None => println!("L{pc:04X}:"),
                    }
                }
                let bytes: Vec<String> = (0..ins.len)
                    .map(|o| format!("{:02X}", mem[a - base as usize + o as usize]))
                    .collect();
                let mut note = String::new();
                if let Some(v) = ins.sets_dptr {
                    dptr = Some(v);
                } else if ins.inc_dptr {
                    dptr = dptr.map(|d| d.wrapping_add(1));
                } else if ins.clobbers_dptr {
                    dptr = None;
                }
                if let Some(w) = ins.movx_dptr {
                    match dptr {
                        Some(d) => {
                            note = format!("  ; X:{d:04X}h {}", if w { "write" } else { "read" });
                            movx.entry(d).or_default().push((pc, w));
                        }
                        None => note = "  ; X:? (DPTR not constant here)".into(),
                    }
                }
                if let (Flow::Call(_), Some(d)) = (ins.flow, dptr) {
                    // The routine is entered with a constant DPTR (listed
                    // in the xref with the mark c).
                    note = format!("  ; with DPTR={d:04X}h");
                    calls_with_dptr.entry(d).or_default().push(pc);
                }
                println!("{pc:04X}: {:<9} {}{note}", bytes.join(" "), ins.text);
                if matches!(ins.flow, Flow::Call(_)) {
                    // A called routine may change DPTR.
                    dptr = None;
                }
                if matches!(ins.flow, Flow::Jump(_) | Flow::Stop | Flow::Indirect) {
                    dptr = None;
                }
                a += ins.len as usize;
            }
            None => {
                if data_run.is_empty() {
                    data_at = a;
                }
                data_run.push(mem[a - base as usize]);
                a += 1;
            }
        }
    }
    if !data_run.is_empty() {
        flush(&mut data_run, data_at);
    }

    if xref {
        println!(
            "\n; ---- MOVX @DPTR with a constant DPTR: address <- instructions (r read, w write, c a call entered with DPTR = the address)"
        );
        let addrs: BTreeSet<u16> = movx.keys().chain(calls_with_dptr.keys()).copied().collect();
        for d in addrs {
            let mut u: Vec<String> = movx
                .get(&d)
                .into_iter()
                .flatten()
                .map(|(pc, w)| format!("{pc:04X}{}", if *w { "w" } else { "r" }))
                .collect();
            u.extend(
                calls_with_dptr
                    .get(&d)
                    .into_iter()
                    .flatten()
                    .map(|pc| format!("{pc:04X}c")),
            );
            println!("; X:{d:04X}h  {}", u.join(" "));
        }
        println!("\n; ---- call targets <- callers");
        for (t, c) in &callers {
            println!(
                "; {t:04X}  <- {}",
                c.iter()
                    .map(|x| format!("{x:04X}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        println!(
            "\n; ---- JMP @A+DPTR: jump tables followed (entry addresses), and the rest not followed"
        );
        for pc in &indirect {
            match tables.get(pc) {
                Some(t) => println!(
                    "; {pc:04X} table {}",
                    t.iter()
                        .map(|x| format!("{x:04X}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
                None => println!("; {pc:04X} not followed"),
            }
        }
        println!("\n; ---- transfers to addresses outside the image (from -> to)");
        for (f, t) in &outside {
            println!("; {f:04X} -> {t:04X}");
        }
        let covered: usize = code.values().map(|i| i.len as usize).sum();
        println!("\n; reached {covered} of {} bytes as code", mem.len());
    }
}
