//! Write bytes into the configuration EEPROM (I²C slave 0xA0) and read
//! them back (findings 17).
//!
//!   eepromwrite OFFSET BYTE [BYTE …]   (hex, at most 16 bytes)
//!
//! Built with `--features bench`. One block 3 Set (wIndex 0x0310, wValue =
//! offset << 8 | 0xA0) — never block 0, whose page write would land on
//! byte 0 — then `WRITE_CYCLE` for the EEPROM to finish, then a block 3 Get
//! of the same bytes, compared. Takes effect at the next power cycle.

use std::time::Duration;

use sdr_rtl::blocking::Rtl;
use sdr_rtl::protocol::{ACCESS_GET, ACCESS_SET, BLOCK_TUNER, windex};

const EEPROM_SLAVE: u16 = 0xA0;
const MAX_BYTES: usize = 16;
/// An EEPROM's internal write cycle; the M24C02's own figure is not in
/// documents.md (findings 17, open), so a generous wait.
const WRITE_CYCLE: Duration = Duration::from_millis(20);

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let hex = |s: &str| u16::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|_| format!("bad hex {s}"));
    let off = hex(args.first().ok_or("usage: eepromwrite OFFSET BYTE [BYTE …]")?)?;
    let bytes: Vec<u8> = args[1..].iter().map(|s| hex(s).map(|v| v as u8)).collect::<Result<_, _>>()?;
    if bytes.is_empty() || bytes.len() > MAX_BYTES || off as usize + bytes.len() > 256 {
        return Err(format!("1-{MAX_BYTES} bytes within the EEPROM"));
    }
    let rtl = Rtl::open("").map_err(|e| e.to_string())?;
    let link = rtl.link();
    let before = link.get(off << 8 | EEPROM_SLAVE, windex(BLOCK_TUNER, ACCESS_GET, 0), bytes.len() as u16, "EEPROM read").map_err(|e| e.to_string())?;
    link.set(off << 8 | EEPROM_SLAVE, windex(BLOCK_TUNER, ACCESS_SET, 0), &bytes, "EEPROM write").map_err(|e| e.to_string())?;
    std::thread::sleep(WRITE_CYCLE);
    let after = link.get(off << 8 | EEPROM_SLAVE, windex(BLOCK_TUNER, ACCESS_GET, 0), bytes.len() as u16, "EEPROM read").map_err(|e| e.to_string())?;
    println!("offset 0x{off:02X}: before {before:02X?}, wrote {bytes:02X?}, read back {after:02X?}: {}", if after == bytes { "ok" } else { "MISMATCH" });
    if after != bytes {
        return Err("read back differs".into());
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("eepromwrite: {e}");
        std::process::exit(1);
    }
}
