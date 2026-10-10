//! The EEPROM API on the bench (findings 17), through the public API:
//! `Rtl::read_eeprom`, `Eeprom::identity`, `Rtl::write_eeprom`.
//!
//!   eepromcheck
//!
//! Read the part and parse it; then a write round trip that leaves the
//! identity alone: one free byte past the strings (0x33 on the bench
//! dongle, whose strings end at 0x31) to 0x55 and read back, then to 0x00
//! again and read back. Prints what each step saw.

use sdr_rtl::Rtl;
use sdr_rtl::eeprom::Eeprom;

const PROBE: usize = 0x33;
const PROBE_VALUE: u8 = 0x55;

fn run() -> Result<(), String> {
    let mut rtl = Rtl::open("").map_err(|e| e.to_string())?;
    let e = rtl.read_eeprom().map_err(|e| e.to_string())?;
    let id = e.identity().map_err(|e| e.to_string())?;
    println!("part {} bytes, signature valid {}, identity {id:?}", e.bytes().len(), e.is_valid());
    if e.bytes()[PROBE] != 0 || e.strings_end().map(|end| end > PROBE).unwrap_or(true) {
        return Err(format!("0x{PROBE:02X} is not a free zero byte past the strings on this part; not writing"));
    }
    let mut probe = e.bytes().to_vec();
    probe[PROBE] = PROBE_VALUE;
    let probe = Eeprom::from_bytes(probe).map_err(|e| e.to_string())?;
    let r = rtl.write_eeprom(&probe).map_err(|e| e.to_string())?;
    let after = rtl.read_eeprom().map_err(|e| e.to_string())?;
    println!("wrote 0x{PROBE_VALUE:02X} at 0x{PROBE:02X}: {r:?}; reads 0x{:02X}", after.bytes()[PROBE]);
    let r = rtl.write_eeprom(&e).map_err(|e| e.to_string())?;
    let back = rtl.read_eeprom().map_err(|e| e.to_string())?;
    println!("restored: {r:?}; reads 0x{:02X}; whole part as before: {}", back.bytes()[PROBE], back.bytes() == e.bytes());
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("eepromcheck: {e}");
        std::process::exit(1);
    }
}
