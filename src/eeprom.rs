//! The dongle's configuration EEPROM: a 24C02 (256 bytes) on the
//! RTL2832U's I²C master at address 0xA0, holding what the chip serves
//! at USB enumeration — VID/PID, the manufacturer/product/serial
//! strings, and two config bits. Rewriting the serial is how several
//! dongles on one host are told apart — serial each by the antenna it is
//! wired to, and `rtl:serial=UHF` opens the dongle on the UHF antenna.
//! Layout is Realtek's, documented by librtlsdr's rtl_eeprom
//! (interface facts only, per this crate's policy):
//!
//! ```text
//! [0..2]  signature 0x28 0x32
//! [2..4]  vendor id, little-endian
//! [4..6]  product id, little-endian
//! [6]     0xA5 when a serial string is present
//! [7]     bit 0 remote wakeup · bit 1 IR endpoint enabled
//! [9..]   manufacturer, product, serial strings back to back, each in
//!         USB string-descriptor form: length (2·chars + 2), 0x03, then
//!         the characters as UTF-16LE (ASCII byte, 0x00)
//! ```
//!
//! The EEPROM is not behind the tuner's I²C repeater — no bracketing.
//! Nothing here touches the streaming path; descriptors are read at
//! enumeration, so a rewrite takes effect on the next replug.

use std::time::Duration;

use sdr_core::{Error, Result};
use crate::usb::{Rtl2832, RtlUsb};

pub const EEPROM_SIZE: usize = 256;
/// The two bytes every RTL2832U configuration EEPROM opens with.
const SIGNATURE: [u8; 2] = [0x28, 0x32];
/// USB descriptor type of a string descriptor (USB 2.0 Table 9-5).
const DESCRIPTOR_TYPE_STRING: u8 = 0x03;
/// The 24C02's 8-bit I2C address on the RTL2832U's master.
const EEPROM_I2C_ADDR: u16 = 0xa0;
/// Where the three string descriptors begin.
const STR_START: usize = 0x09;
/// Octet 6: whether a serial string is present.
const SERIAL_FLAG_AT: usize = 6;
const SERIAL_PRESENT: u8 = 0xa5;
const SERIAL_ABSENT: u8 = 0x00;
/// Octet 7: the two documented config bits (real units carry others).
const CONFIG_AT: usize = 7;
const CONFIG_REMOTE_WAKEUP: u8 = 1 << 0;
const CONFIG_IR_ENABLED: u8 = 1 << 1;
/// 24C02 internal write-cycle time (t_WR, 5 ms max) — pace byte writes.
/// The part's write cycle. Datasheets quote 5 ms typical for 24C0x
/// parts and some quote 10 ms max; 5 ms produced one `endpoint stalled`
/// in three full rewrites on one dongle, which is the part refusing a transfer while still busy. 10 ms, and a single
/// retry after a longer pause, cost nothing per byte that matters.
const WRITE_CYCLE: Duration = Duration::from_millis(10);
const RETRY_PAUSE: Duration = Duration::from_millis(25);

fn eeprom_err(context: &'static str, message: String) -> Error {
    Error::Device {
        backend: "rtl",
        context,
        message,
    }
}

/// Parsed identity block of the configuration EEPROM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtlEeprom {
    pub vid: u16,
    pub pid: u16,
    pub manufacturer: String,
    pub product: String,
    pub serial: String,
    pub remote_wakeup: bool,
    pub ir_enabled: bool,
}

impl RtlEeprom {
    /// Parse a raw EEPROM image. Fails loudly on a missing signature —
    /// a blank or absent EEPROM must never be "fixed" by a write built
    /// on garbage.
    pub fn parse(image: &[u8; EEPROM_SIZE]) -> Result<RtlEeprom> {
        if image[..2] != SIGNATURE {
            return Err(eeprom_err(
                "eeprom parse",
                format!(
                    "bad signature {:#04x} {:#04x} (want {:#04x} {:#04x}) — blank or absent EEPROM",
                    image[0], image[1], SIGNATURE[0], SIGNATURE[1]
                ),
            ));
        }
        let mut pos = STR_START;
        let manufacturer = read_string(image, &mut pos)?;
        let product = read_string(image, &mut pos)?;
        let serial = read_string(image, &mut pos)?;
        Ok(RtlEeprom {
            vid: u16::from_le_bytes([image[2], image[3]]),
            pid: u16::from_le_bytes([image[4], image[5]]),
            manufacturer,
            product,
            serial,
            remote_wakeup: image[CONFIG_AT] & CONFIG_REMOTE_WAKEUP != 0,
            ir_enabled: image[CONFIG_AT] & CONFIG_IR_ENABLED != 0,
        })
    }

    /// Patch this identity into an existing image, leaving every byte
    /// past the string area as it was (unknown vendor data survives).
    /// Returns the offset just past the last descriptor — `compose`
    /// zeros from there to the end of the part.
    pub fn write_into(&self, image: &mut [u8; EEPROM_SIZE]) -> Result<usize> {
        let strings = [&self.manufacturer, &self.product, &self.serial];
        for s in strings {
            if !s.is_ascii() || s.bytes().any(|b| b.is_ascii_control()) {
                return Err(eeprom_err(
                    "eeprom serialize",
                    format!("string {s:?} is not printable ASCII"),
                ));
            }
        }
        let need: usize = STR_START + strings.iter().map(|s| 2 * s.len() + 2).sum::<usize>();
        if need > EEPROM_SIZE {
            return Err(eeprom_err(
                "eeprom serialize",
                format!("strings need {need} bytes, EEPROM holds {EEPROM_SIZE}"),
            ));
        }
        image[..2].copy_from_slice(&SIGNATURE);
        image[2..4].copy_from_slice(&self.vid.to_le_bytes());
        image[4..6].copy_from_slice(&self.pid.to_le_bytes());
        image[SERIAL_FLAG_AT] = if self.serial.is_empty() { SERIAL_ABSENT } else { SERIAL_PRESENT };
        // Only the two documented bits — real units carry others (the
        // bench dongle has 0x16 here) that must survive a rewrite.
        let config = (if self.remote_wakeup { CONFIG_REMOTE_WAKEUP } else { 0 }) | (if self.ir_enabled { CONFIG_IR_ENABLED } else { 0 });
        image[CONFIG_AT] = (image[CONFIG_AT] & !(CONFIG_REMOTE_WAKEUP | CONFIG_IR_ENABLED)) | config;
        let mut pos = STR_START;
        for s in strings {
            image[pos] = (2 * s.len() + 2) as u8;
            image[pos + 1] = DESCRIPTOR_TYPE_STRING;
            for (i, b) in s.bytes().enumerate() {
                image[pos + 2 + 2 * i] = b;
                image[pos + 3 + 2 * i] = 0; // UTF-16LE high byte of ASCII
            }
            pos += 2 * s.len() + 2;
        }
        Ok(pos)
    }

    /// How many bytes this chip really has, judged from a read of it:
    /// 128 when the upper half mirrors the lower, else 256.
    pub fn span(image: &[u8; EEPROM_SIZE]) -> usize {
        addressable(image)
    }
}

fn read_string(image: &[u8; EEPROM_SIZE], pos: &mut usize) -> Result<String> {
    let start = *pos;
    let (Some(&len), Some(&tag)) = (image.get(start), image.get(start + 1)) else {
        return Err(eeprom_err(
            "eeprom parse",
            format!("string descriptor at {start:#04x} runs past the EEPROM"),
        ));
    };
    let len = len as usize;
    if tag != DESCRIPTOR_TYPE_STRING || len < 2 || !len.is_multiple_of(2) || start + len > EEPROM_SIZE {
        return Err(eeprom_err(
            "eeprom parse",
            format!("malformed string descriptor at {start:#04x} (len {len}, tag {tag:#04x})"),
        ));
    }
    // The characters sit in the even bytes; take them as Latin-1 so an
    // oddball factory image still round-trips visibly instead of erroring.
    let s = image[start + 2..start + len]
        .iter()
        .step_by(2)
        .map(|&b| b as char)
        .collect();
    *pos = start + len;
    Ok(s)
}

/// Read the full EEPROM: seek to 0, then sequential current-address
/// reads (the 24C02 auto-increments) — librtlsdr's proven access
/// pattern, byte at a time.
pub(crate) fn read(usb: &RtlUsb) -> Result<[u8; EEPROM_SIZE]> {
    usb.i2c_write(EEPROM_I2C_ADDR, &[0])?;
    let mut image = [0u8; EEPROM_SIZE];
    for byte in image.iter_mut() {
        *byte = *usb
            .i2c_read(EEPROM_I2C_ADDR, 1)?
            .first()
            .ok_or_else(|| eeprom_err("eeprom read", "empty I2C read".into()))?;
    }
    Ok(image)
}

/// Which of the three identity strings a rewrite touches. `None` = keep
/// what the chip holds. This is the whole contract of a rewrite: read
/// first, change only what was asked for.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Fields<'a> {
    pub manufacturer: Option<&'a str>,
    pub product: Option<&'a str>,
    pub serial: Option<&'a str>,
}

/// Build the image to write from what the chip holds plus the requested
/// changes — the one place the rules live, so every tool that rewrites
/// an EEPROM applies the same ones:
///
/// 1. parse `current` (a chip that does not parse is refused, never
///    "fixed" by guessing);
/// 2. override only the fields given;
/// 3. refuse an identity that does not fit THIS PART — the real span
///    (128 on a mirrored part), naming the overshoot and how many serial
///    characters would fit;
/// 4. serialize, then WRITE THE WHOLE STRING AREA: zeros from the end of
///    the last descriptor to the end of the part. The bytes past the descriptors are only ever the tails of earlier,
///    longer names, and a definite image is worth more than preserving
///    them. (The serializer `write_into` still preserves; this composer
///    is where the policy is applied.)
pub fn compose(current: &[u8; EEPROM_SIZE], f: &Fields<'_>) -> Result<(RtlEeprom, [u8; EEPROM_SIZE])> {
    let mut info = RtlEeprom::parse(current)?;
    if let Some(m) = f.manufacturer {
        info.manufacturer = m.into();
    }
    if let Some(pr) = f.product {
        info.product = pr.into();
    }
    if let Some(sr) = f.serial {
        info.serial = sr.into();
    }
    let span = addressable(current);
    let need = STR_START + [&info.manufacturer, &info.product, &info.serial]
        .iter()
        .map(|s| 2 * s.len() + 2)
        .sum::<usize>();
    if need > span {
        let fixed = STR_START + (2 * info.manufacturer.len() + 2) + (2 * info.product.len() + 2);
        let serial_max = span.saturating_sub(fixed + 2) / 2;
        return Err(eeprom_err(
            "eeprom compose",
            format!(
                "identity needs {need} bytes but this part has {span}: {} over. With manufacturer {:?} and product {:?} the serial can be at most {serial_max} characters",
                need - span, info.manufacturer, info.product
            ),
        ));
    }
    let mut image = *current;
    let end = info.write_into(&mut image)?;
    image[end..span].fill(0);
    Ok((info, image))
}

/// The span of cells a part actually has. A 128-byte part answers 0x80..
/// with a mirror of 0x00.. (measured on a real dongle), so a
/// full-image write on such a part must stop at 0x80: writing "0x00 to
/// 0xc8" lands on 0x48 and silently undoes the byte written there a
/// moment earlier, and a full-image verify reports the mirror as a
/// mismatch that no write can ever clear.
pub(crate) fn addressable(current: &[u8; EEPROM_SIZE]) -> usize {
    let (lo, hi) = current.split_at(EEPROM_SIZE / 2);
    if lo == hi { EEPROM_SIZE / 2 } else { EEPROM_SIZE }
}

/// Write a full image and prove it landed.
///
/// One whole-image read up front decides which bytes differ; each of
/// those is written and the part's write cycle waited out; then a full
/// readback is compared. The write is only reported done when the chip
/// proves it holds the image.
///
/// ⚠ NO PER-BYTE PRE-READ. The first version of this function did what
/// librtlsdr does — set the address, read one byte, skip if equal, else
/// write — and on one dongle every write was dropped, on macOS and on
/// Linux alike, while librtlsdr's identical sequence in C succeeded and a
/// second dongle accepted either form (measured A/B on the same chips). Reading the byte
/// immediately before writing it is what that part will not tolerate
/// from this stack; comparing against a read taken up front does not
/// touch the cell between decision and write, and the part accepts it.
pub(crate) fn write(usb: &RtlUsb, image: &[u8; EEPROM_SIZE]) -> Result<()> {
    let current = read(usb)?;
    let span = addressable(&current);
    for addr in 0..span {
        if current[addr] == image[addr] {
            continue;
        }
        if usb.i2c_write(EEPROM_I2C_ADDR, &[addr as u8, image[addr]]).is_err() {
            // A stalled transfer is the part still busy from the previous
            // byte. Give it a full cycle and try once more; a second
            // failure is real and propagates.
            std::thread::sleep(RETRY_PAUSE);
            usb.i2c_write(EEPROM_I2C_ADDR, &[addr as u8, image[addr]])?;
        }
        std::thread::sleep(WRITE_CYCLE);
    }
    let readback = read(usb)?;
    if let Some(addr) = (0..span).find(|&i| readback[i] != image[i]) {
        return Err(eeprom_err(
            "eeprom write",
            format!(
                "readback mismatch at {addr:#04x}: wrote {:#04x}, read {:#04x}",
                image[addr], readback[addr]
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    fn image_with(serial: &str) -> [u8; super::EEPROM_SIZE] {
        let mut img = [0u8; super::EEPROM_SIZE];
        super::RtlEeprom {
            vid: 0x0bda, pid: 0x2838,
            manufacturer: "Realtek".into(), product: "RTL2838".into(), serial: serial.into(),
            remote_wakeup: false, ir_enabled: true,
        }
        .write_into(&mut img)
        .unwrap();
        img
    }

    /// A serial-only rewrite changes the serial and nothing else.
    #[test]
    fn compose_changes_only_the_given_field() {
        let cur = image_with("0001");
        let (info, img) = super::compose(&cur, &super::Fields { serial: Some("ANT-01"), ..Default::default() }).unwrap();
        assert_eq!((info.manufacturer.as_str(), info.product.as_str(), info.serial.as_str()), ("Realtek", "RTL2838", "ANT-01"));
        assert_eq!(super::RtlEeprom::parse(&img).unwrap(), info);
        assert_eq!(img[..6], cur[..6], "signature and ids untouched");
    }

    /// The whole string area is written: after a SHORTER name, the old
    /// name's tail is zeros, to the end of the part.
    #[test]
    fn compose_writes_zeros_to_the_end_of_the_part() {
        let mut cur = image_with("A-LONG-SERIAL-NAME");
        cur[0x60] = 0xEE; // stray garbage in dead space
        let (_, img) = super::compose(&cur, &super::Fields { serial: Some("X"), ..Default::default() }).unwrap();
        let end = super::STR_START + 16 + 16 + 4;
        assert!(img[end..].iter().all(|&b| b == 0), "zeros from the last descriptor to 0x100");
    }

    /// A mirrored part has 128 real cells: the fit check and the zero
    /// fill both stop there, and the alias half is left alone.
    #[test]
    fn compose_respects_a_mirrored_part() {
        let mut cur = image_with("0001");
        cur.copy_within(0..128, 128); // the mirror a 128-byte part shows
        let too_long = "S".repeat(60); // 9 + 16 + 16 + 122 = 163 > 128
        let e = super::compose(&cur, &super::Fields { serial: Some(&too_long), ..Default::default() }).unwrap_err().to_string();
        assert!(e.contains("this part has 128"), "{e}");
        assert!(e.contains("at most 42 characters"), "{e}");
        let (_, img) = super::compose(&cur, &super::Fields { serial: Some("ANT-01"), ..Default::default() }).unwrap();
        assert!(img[super::STR_START + 16 + 16 + 14..128].iter().all(|&b| b == 0));
        assert_eq!(img[128..], cur[128..], "the alias half is not written");
    }

    /// A 128-byte part answers 0x80.. with a mirror of 0x00..; the
    /// writer must confine itself to the real cells or it undoes its
    /// own writes through the alias (measured on a real part).
    #[test]
    fn addressable_span_follows_the_mirror() {
        let mut img = [0u8; super::EEPROM_SIZE];
        for (i, b) in img.iter_mut().enumerate() {
            *b = (i % 128) as u8;
        }
        assert_eq!(super::addressable(&img), 128, "mirrored → 128 real cells");
        img[0xc8] = 0xff;
        assert_eq!(super::addressable(&img), 256, "any difference → a full 256-byte part");
    }


    use super::*;

    fn info() -> RtlEeprom {
        RtlEeprom {
            vid: 0x0bda,
            pid: 0x2838,
            manufacturer: "Realtek".into(),
            product: "RTL2838".into(),
            serial: "UHF".into(),
            remote_wakeup: false,
            ir_enabled: true,
        }
    }

    #[test]
    fn round_trip() {
        let mut image = [0xffu8; EEPROM_SIZE];
        let info = info();
        info.write_into(&mut image).unwrap();
        assert_eq!(RtlEeprom::parse(&image).unwrap(), info);
        // Bytes past the string area are preserved, not zeroed.
        assert_eq!(image[EEPROM_SIZE - 1], 0xff);
        // Undocumented bits of the config byte survive a rewrite.
        assert_eq!(image[7] & !0x03, 0xfc);
    }

    #[test]
    fn blank_eeprom_refuses_to_parse() {
        assert!(RtlEeprom::parse(&[0xff; EEPROM_SIZE]).is_err());
    }

    #[test]
    fn oversized_strings_refuse_to_serialize() {
        let mut info = info();
        info.product = "x".repeat(200);
        assert!(info.write_into(&mut [0u8; EEPROM_SIZE]).is_err());
    }

    #[test]
    fn non_ascii_refuses_to_serialize() {
        let mut info = info();
        info.serial = "über".into();
        assert!(info.write_into(&mut [0u8; EEPROM_SIZE]).is_err());
    }

    #[test]
    fn truncated_string_descriptor_is_an_error() {
        let mut image = [0u8; EEPROM_SIZE];
        info().write_into(&mut image).unwrap();
        // Corrupt the manufacturer descriptor to claim it runs off the end.
        image[STR_START] = 0xfe;
        assert!(RtlEeprom::parse(&image).is_err());
    }
}
