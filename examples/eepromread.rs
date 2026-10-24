//! Read the configuration EEPROM (I²C slave 0xA0) and parse it by the
//! layout derived from the firmware (findings 17). Read only.
//!
//!   eepromread
//!
//! Built with `--features bench`. The host path the firmware serves
//! (findings 17): block 3 Get, wIndex 0x0300, wValue = offset << 8 | 0xA0,
//! at most 16 bytes a request (0x17FC → 0x0FBA). Never block 0 for 0xA0:
//! its page write would overwrite byte 0. 256 bytes in 16-byte reads; a
//! hex dump and the fields: signature, IDs, serial flag, configuration
//! bits, the three string descriptors, the patch list.

use sdr_rtl::blocking::Rtl;
use sdr_rtl::protocol::{ACCESS_GET, BLOCK_TUNER, windex};

const EEPROM_SLAVE: u16 = 0xA0;
const EEPROM_BYTES: u16 = 256;
const CHUNK: u16 = 16;

fn string_at(e: &[u8], at: usize) -> (String, usize) {
    let len = e.get(at).copied().unwrap_or(0) as usize;
    if len < 2 || at + len > e.len() {
        return (format!("<bLength {len}>"), len.max(1));
    }
    let units: Vec<u16> = e[at + 2..at + len]
        .chunks(2)
        .map(|c| u16::from_le_bytes([c[0], *c.get(1).unwrap_or(&0)]))
        .collect();
    (
        format!(
            "{:?} (bLength {len}, bDescriptorType 0x{:02X})",
            String::from_utf16_lossy(&units),
            e[at + 1]
        ),
        len,
    )
}

fn main() {
    let rtl = match Rtl::open("") {
        Ok(r) => r,
        Err(e) => {
            eprintln!("eepromread: {e}");
            std::process::exit(1);
        }
    };
    let mut e = Vec::new();
    for off in (0..EEPROM_BYTES).step_by(CHUNK as usize) {
        match rtl.link().get(
            off << 8 | EEPROM_SLAVE,
            windex(BLOCK_TUNER, ACCESS_GET, 0),
            CHUNK,
            "EEPROM read",
        ) {
            Ok(v) => e.extend_from_slice(&v),
            Err(err) => {
                eprintln!("eepromread: offset 0x{off:02X}: {err}");
                std::process::exit(1);
            }
        }
    }
    for (row, c) in e.chunks(16).enumerate() {
        println!(
            "{:02X}: {}",
            row * 16,
            c.iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    let sig_ok = e[0] == 0x28 && e[1] == 0x32;
    println!(
        "signature 0x{:02X} 0x{:02X}: {}",
        e[0],
        e[1],
        if sig_ok {
            "valid (0x28 0x32)"
        } else {
            "not valid: the firmware uses its defaults"
        }
    );
    println!(
        "idVendor 0x{:04X} idProduct 0x{:04X}",
        u16::from_le_bytes([e[2], e[3]]),
        u16::from_le_bytes([e[4], e[5]])
    );
    println!(
        "serial flag 0x{:02X} ({})",
        e[6],
        if e[6] == 0xA5 { "present" } else { "absent" }
    );
    println!(
        "configuration 0x{:02X}: remote wake-up {}, interface 1 {}, GPIO3 out {}, DEMOD_CTL_1 bit 0 {}, DEMOD_CTL_1 bit 1 clear {}",
        e[7],
        e[7] & 1,
        e[7] >> 1 & 1,
        e[7] >> 2 & 1,
        e[7] >> 3 & 1,
        e[7] >> 4 & 1
    );
    let mut at = 9;
    for name in ["manufacturer", "product", "serial"] {
        let (s, len) = string_at(&e, at);
        println!("{name} at 0x{at:02X}: {s}");
        at += len;
    }
    println!(
        "patch list: marker 0x{:02X} ({}), length {}",
        e[0x4E],
        if e[0x4E] == 0x5C { "present" } else { "absent" },
        e[0x4F]
    );
}
