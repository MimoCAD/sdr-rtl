//! The configuration EEPROM: its layout as the firmware reads it at boot
//! (`derivation/eeprom.md`, every fact cited there by ROM address) and
//! as the bench confirmed it (findings 17), the image type that parses
//! and composes it, and the host's access to it through block 3.
//!
//! What the firmware does with the part (eeprom.md §2-§7): it reads
//! bytes 0x00-0x0D at boot and uses the EEPROM only when bytes 0-1 hold
//! the signature 0x28 0x32; then idVendor / idProduct, a serial flag, a
//! configuration byte, and from 0x09 the manufacturer, product and
//! (flagged) serial string descriptors, each placed by the one before's
//! bLength. Independently of the signature, a 0x5C at 0x4E makes the
//! following bytes a patch list the firmware writes into its own data
//! space before enumeration (eeprom.md §5). Anything it rejects falls
//! back to the ROM's defaults (eeprom.md §7). Changes take effect at
//! the next power cycle (eeprom.md §5, §8).
//!
//! The host's path (eeprom.md §8; findings 17): block 3 Get / Set,
//! wValue = offset << 8 | 0xA0, at most 16 bytes per request. Block 0
//! must never address 0xA0: its page write puts the page number into
//! the slave's register 0, which on the EEPROM is the signature's first
//! byte — the bench dongle's byte 0 read 0x00 for exactly that reason
//! (findings 17), and the request layer refuses such a request
//! ([`block0_addresses_eeprom`], `usb::Link::get` / `set`).

use alloc::string::String;
use alloc::vec::Vec;
use alloc::{format, vec};
use core::ops::Range;
use core::time::Duration;

use sdr_core::{Error, Result};

use crate::BACKEND;
use crate::protocol::{BLOCK_DEMOD, BLOCK_TUNER, block_of};

// ---- On the bus (eeprom.md §1, §8; findings 5c, 17) ----

/// The EEPROM's I²C slave, 8-bit form: the only fixed I²C address in the
/// ROM (0x0FB8, 0x16F6, 0x1756; eeprom.md header). An M24C02 with E2..E0
/// grounded on the reference circuit (documents.md, Fig. 9).
pub const EEPROM_I2C_ADDRESS: u8 = 0xA0;
/// The read/write bit of an 8-bit I²C address. The firmware's I²C
/// routines send the slave with this bit cleared for the address phase
/// (eeprom.md §1: 0x0FBA sends slave & 0xFE, 0x127C likewise), so 0xA1
/// reaches the same part as 0xA0.
pub const I2C_RW_BIT: u8 = 0x01;
/// The block the EEPROM is reached through: block 3, the I²C access
/// without block 0's page write (findings 5c, handler 0x0788) — the
/// tuner's block, here addressed to the EEPROM's slave.
pub const EEPROM_BLOCK: u16 = BLOCK_TUNER;
/// One I²C transaction moves at most 16 bytes (eeprom.md §1: callers cap
/// at 0x10, 0x0C5E-0x0C69; findings 5c: transactions split at 16, 0x17D4,
/// 0x1985). Every host request here is one transaction.
pub const EEPROM_REQUEST_MAX: usize = 16;
/// The unit of a write: an 8-byte chunk aligned to 8, one control
/// transfer each. The firmware splits a Set into ≤ 16-byte I²C writes
/// with the offset advancing by 16 and no delay between them, so a
/// second write inside one request meets the part busy in its write
/// cycle and the request STALLs (eeprom.md §8, 0x0814 → 0x1935, 0x085E).
/// The part's write page is 8 bytes, measured (findings 17): a 16-byte
/// write at 0x30 wrapped, its second half landing back on 0x30–0x37, and
/// the read-back reported 5 bytes differing. An 8-byte chunk is one page.
pub const EEPROM_WRITE_CHUNK: usize = 8;
/// The wait after each chunk's write for the part's internal write
/// cycle. **Unmeasured**: the M24C02's write-cycle time is not in
/// documents.md (eeprom.md §9 item 2). Findings 17's repair waited 20 ms
/// after its one-byte write and read the byte back as written.
pub const EEPROM_WRITE_CYCLE: Duration = Duration::from_millis(20);
/// The register (EEPROM offset) is one byte (eeprom.md §1), so 256
/// offsets are addressable whatever the part holds.
pub const EEPROM_ADDRESS_SPACE: usize = 256;
/// A 128-byte part: the bench dongle's upper half read back as a copy of
/// the lower (findings 17: "bytes 0x80-0xFF repeat 0x00-0x7F", the
/// addresses wrap).
pub const EEPROM_SMALL_PART: usize = 128;

/// wValue for an EEPROM access at `offset`: offset << 8 | 0xA0 (findings
/// 5c: block 3's register = wValue high byte, slave = wValue low byte;
/// eeprom.md §8; findings 17's bench read and repair).
pub const fn wvalue_eeprom(offset: u8) -> u16 {
    ((offset as u16) << 8) | EEPROM_I2C_ADDRESS as u16
}

/// Whether a request is a block 0 access to the EEPROM's slave — the
/// hazard: block 0 writes the page number (wIndex & 0x0F) into register
/// 0x00 of the slave in wValue's low byte whenever it differs from one
/// cached page shared by all slaves (findings 5c, 0x072A-0x0769;
/// eeprom.md §8), and the EEPROM's register 0 is the signature's 0x28.
/// Findings 17: the bench dongle's byte 0 read 0x00 after step 2's
/// block-0 sweep of every address. Get and Set both do the page write.
pub const fn block0_addresses_eeprom(value: u16, index: u16) -> bool {
    block_of(index) == BLOCK_DEMOD && (value as u8) & !I2C_RW_BIT == EEPROM_I2C_ADDRESS
}

// ---- The layout (eeprom.md §3) ----

/// Bytes 0-1, the signature the firmware requires before it uses the
/// part (0x0BAB-0x0BBF; eeprom.md §2 step 3).
pub const SIGNATURE_OFF: usize = 0x00;
pub const SIGNATURE: [u8; 2] = [0x28, 0x32];
/// idVendor, little-endian (descriptor byte order; loop 0x0BC2-0x0BE9).
pub const VID_OFF: usize = 0x02;
/// idProduct, little-endian (same loop).
pub const PID_OFF: usize = 0x04;
/// The serial flag: 0xA5 = a serial string descriptor follows the
/// product's (0x0BEC-0x0BFE); any other value = no serial
/// (iSerialNumber 0).
pub const SERIAL_FLAG_OFF: usize = 0x06;
pub const SERIAL_PRESENT: u8 = 0xA5;
/// What [`Eeprom::compose`] writes when it removes a serial: any value
/// but 0xA5 means "no serial" (0x0BEC-0x0BFE); 0x00 is our choice.
pub const SERIAL_ABSENT: u8 = 0x00;
/// The configuration byte, copied whole into the firmware's bit
/// variables (0x0C0C-0x0C10); bits in [`EepromConfig`].
pub const CONFIG_OFF: usize = 0x07;
/// Not read in a valid record; a test record's I²C slave (eeprom.md §6).
pub const TEST_SLAVE_OFF: usize = 0x08;
/// The first string descriptor (the manufacturer's) starts here; the
/// product's and the serial's follow, each at the one before plus its
/// bLength (eeprom.md §3).
pub const STRINGS_OFF: usize = 0x09;
/// bLength and bDescriptorType precede a string descriptor's characters.
pub const DESCRIPTOR_HEADER: usize = 2;
/// bDescriptorType of a USB string descriptor; the ROM's own default
/// strings carry it (0x0E40, 0x0E50, 0x0E62), the firmware never checks
/// it (eeprom.md §2, §3).
pub const STRING_DESCRIPTOR_TYPE: u8 = 0x03;
/// The firmware's limit on the manufacturer's and the product's bLength
/// together (0x0C47-0x0C4E; eeprom.md §2 step 4); over it, all three
/// strings are the ROM's defaults.
pub const MANUFACTURER_PRODUCT_MAX: usize = 70;
/// The firmware's limit on the serial's bLength (0x0CE5-0x0CEA).
pub const SERIAL_MAX: usize = 40;
/// The patch-list marker and where it sits (0x16F8-0x170B; eeprom.md §5),
/// independent of the signature.
pub const PATCH_MARKER_OFF: usize = 0x4E;
pub const PATCH_MARKER: u8 = 0x5C;
/// The patch list's length in bytes (0x1710-0x171F), capped at 178.
pub const PATCH_LENGTH_OFF: usize = 0x4F;
pub const PATCH_LENGTH_CAP: usize = 178;
/// The patch records start here, 3 bytes each (0x1722-0x1724).
pub const PATCH_RECORDS_OFF: usize = 0x50;
/// A test record is selected by byte 0 = 0x12 **or** byte 1 = 0x34
/// (0x0AC0-0x0AD1; eeprom.md §6): it can write the ROM's CRC to the slave
/// named by byte 8 and always ends in the defaults. Never written.
pub const TEST_RECORD_BYTE0: u8 = 0x12;
pub const TEST_RECORD_BYTE1: u8 = 0x34;

// Every string the firmware accepts ends inside a 128-byte part, so the
// limits alone decide whether an identity fits (0x09 + 70 + 40 = 0x77).
const _: () = assert!(STRINGS_OFF + MANUFACTURER_PRODUCT_MAX + SERIAL_MAX <= EEPROM_SMALL_PART);

// ---- The ROM's defaults (eeprom.md §7) ----

/// idVendor / idProduct of the ROM's device descriptor (0x0DD0), what a
/// blank or invalid part enumerates with.
pub const ROM_VID: u16 = 0x0BDA;
pub const ROM_PID: u16 = 0x2832;
/// The ROM's default strings: manufacturer (0x0E40), product (0x0E50),
/// serial (0x0E62).
pub const ROM_MANUFACTURER: &str = "Generic";
pub const ROM_PRODUCT: &str = "RTL2832U";
pub const ROM_SERIAL: &str = "77771111153705700";

/// The configuration byte (eeprom.md §3, "Configuration byte E[7]").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EepromConfig(pub u8);

impl EepromConfig {
    /// Bit 0: the configuration descriptors advertise remote wake-up and
    /// SET_FEATURE(DEVICE_REMOTE_WAKEUP) is accepted (0x014F, 0x05A1).
    pub const REMOTE_WAKEUP: u8 = 1 << 0;
    /// Bit 1: a second interface with no endpoints (0x006C, 0x0148).
    pub const SECOND_INTERFACE: u8 = 1 << 1;
    /// Bit 2: the GPIO set-up's master switch — GPIO3 an output driven
    /// high, USB 0x2010 bit 5 set (0x1AF7-0x1B35). Bits 3-4 act only
    /// with it.
    pub const GPIO_SETUP: u8 = 1 << 2;
    /// Bit 3 (with bit 2): DEMOD_CTL_1 ← 0x01 (0x1AFA-0x1B02).
    pub const DEMOD_CTL_1_SET: u8 = 1 << 3;
    /// Bit 4 (with bits 2 and 3): DEMOD_CTL_1 bit 1 cleared (0x1B03-0x1B09).
    pub const DEMOD_CTL_1_BIT1_CLEAR: u8 = 1 << 4;
    /// The bits the defaults OR in (0x0DCB-0x0DCD; eeprom.md §7).
    pub const DEFAULTS_FORCED: u8 = Self::REMOTE_WAKEUP | Self::GPIO_SETUP;

    pub fn remote_wakeup(self) -> bool {
        self.0 & Self::REMOTE_WAKEUP != 0
    }

    pub fn second_interface(self) -> bool {
        self.0 & Self::SECOND_INTERFACE != 0
    }

    pub fn gpio_setup(self) -> bool {
        self.0 & Self::GPIO_SETUP != 0
    }

    /// Bit 3, effective only with bit 2.
    pub fn demod_ctl_1_set(self) -> bool {
        self.gpio_setup() && self.0 & Self::DEMOD_CTL_1_SET != 0
    }

    /// Bit 4, effective only with bits 2 and 3.
    pub fn demod_ctl_1_bit1_clear(self) -> bool {
        self.demod_ctl_1_set() && self.0 & Self::DEMOD_CTL_1_BIT1_CLEAR != 0
    }
}

/// The identity an image holds: the fields the firmware reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EepromIdentity {
    pub vid: u16,
    pub pid: u16,
    /// The serial flag is 0xA5.
    pub serial_present: bool,
    pub config: EepromConfig,
    pub manufacturer: String,
    pub product: String,
    /// Empty when `serial_present` is false.
    pub serial: String,
}

impl EepromIdentity {
    /// What a blank, absent or invalid part enumerates with (eeprom.md
    /// §7): 0BDA:2832, "Generic" / "RTL2832U" / "77771111153705700"
    /// (the ROM descriptor's iSerialNumber is 3), configuration bits 0
    /// and 2.
    pub fn rom_defaults() -> EepromIdentity {
        EepromIdentity {
            vid: ROM_VID,
            pid: ROM_PID,
            serial_present: true,
            config: EepromConfig(EepromConfig::DEFAULTS_FORCED),
            manufacturer: ROM_MANUFACTURER.into(),
            product: ROM_PRODUCT.into(),
            serial: ROM_SERIAL.into(),
        }
    }
}

/// The fields [`Eeprom::compose`] changes; `None` keeps the image's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EepromChanges {
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    /// `Some("")` removes the serial (flag cleared, no descriptor).
    pub serial: Option<String>,
    /// Keep the image's patch list (marker at 0x4E, length, records)
    /// instead of zeroing it with the rest of the string area. The
    /// firmware writes the records into its own data space at every boot
    /// (eeprom.md §5); set this only to keep a list on purpose.
    pub allow_patch_list: bool,
}

/// Where the string descriptors of an image lie.
struct Strings {
    manufacturer: Range<usize>,
    product: Range<usize>,
    serial: Option<Range<usize>>,
}

impl Strings {
    fn end(&self) -> usize {
        self.serial.as_ref().map_or(self.product.end, |s| s.end)
    }
}

fn refused(what: String) -> Error {
    Error::Unsupported { backend: BACKEND, what }
}

/// A string descriptor's characters: UTF-16LE after the two-byte header
/// (eeprom.md §3); an odd last byte is ignored, the type is not checked
/// (the firmware checks neither).
fn decode(d: &[u8]) -> String {
    let (pairs, _odd) = d[DESCRIPTOR_HEADER..].as_chunks::<2>();
    let units: Vec<u16> = pairs.iter().map(|&c| u16::from_le_bytes(c)).collect();
    String::from_utf16_lossy(&units)
}

/// A complete USB string descriptor for `s`: bLength, 0x03, UTF-16LE.
fn descriptor(s: &str) -> Vec<u8> {
    let mut d = vec![0, STRING_DESCRIPTOR_TYPE];
    d.extend(s.encode_utf16().flat_map(u16::to_le_bytes));
    d[0] = d.len().min(u8::MAX as usize) as u8;
    d
}

/// The UTF-16 code units (characters, for the Basic Multilingual Plane)
/// a descriptor of `bytes` holds.
const fn chars_in(bytes: usize) -> usize {
    bytes.saturating_sub(DESCRIPTOR_HEADER) / 2
}

/// The part's size from a read of the whole 256-offset space: 128 when
/// the upper half repeats the lower (findings 17: the bench dongle's
/// 128-byte part wraps), else 256. A 256-byte part whose halves happen
/// to agree (an erased one, all 0xFF) is taken as 128 — the safe way
/// round: nothing is then written above 0x7F, where a 128-byte part's
/// addresses would wrap onto its header.
pub fn part_size(raw: &[u8]) -> usize {
    if raw.len() == EEPROM_ADDRESS_SPACE && raw[..EEPROM_SMALL_PART] == raw[EEPROM_SMALL_PART..] { EEPROM_SMALL_PART } else { raw.len() }
}

/// An image of the whole part, 128 or 256 bytes.
#[derive(Clone, Debug)]
pub struct Eeprom {
    bytes: Vec<u8>,
    /// The image may be written with a patch-list marker at 0x4E
    /// ([`Eeprom::allowing_patch_list`], `EepromChanges::allow_patch_list`).
    patch_list_allowed: bool,
}

/// Images compare by their bytes.
impl PartialEq for Eeprom {
    fn eq(&self, other: &Eeprom) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for Eeprom {}

impl Eeprom {
    /// An image from a read of all 256 offsets, cut to the part's size
    /// ([`part_size`]).
    pub fn from_read(raw: &[u8]) -> Result<Eeprom> {
        if raw.len() != EEPROM_ADDRESS_SPACE {
            return Err(refused(format!("an EEPROM read is all {EEPROM_ADDRESS_SPACE} offsets; got {} bytes", raw.len())));
        }
        Ok(Eeprom { bytes: raw[..part_size(raw)].to_vec(), patch_list_allowed: false })
    }

    /// An image of a part as given (a backup file, a test vector): 128 or
    /// 256 bytes, no size detection.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Eeprom> {
        if bytes.len() != EEPROM_SMALL_PART && bytes.len() != EEPROM_ADDRESS_SPACE {
            return Err(refused(format!("an EEPROM image is {EEPROM_SMALL_PART} or {EEPROM_ADDRESS_SPACE} bytes; got {}", bytes.len())));
        }
        Ok(Eeprom { bytes, patch_list_allowed: false })
    }

    /// The part's size in bytes, 128 or 256.
    pub fn size(&self) -> usize {
        self.bytes.len()
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Bytes 0-1 hold the signature, so the firmware uses the part
    /// (eeprom.md §2 step 3).
    pub fn is_valid(&self) -> bool {
        self.bytes[SIGNATURE_OFF..SIGNATURE_OFF + SIGNATURE.len()] == SIGNATURE
    }

    /// Byte 0 = 0x12 or byte 1 = 0x34: the firmware would run its test
    /// record (eeprom.md §6).
    pub fn is_test_record(&self) -> bool {
        self.bytes[SIGNATURE_OFF] == TEST_RECORD_BYTE0 || self.bytes[SIGNATURE_OFF + 1] == TEST_RECORD_BYTE1
    }

    /// The image with bytes 0-1 set to the signature and nothing else
    /// changed: the repair of findings 17, whose dongle's byte 0 read
    /// 0x00 while the rest of its record was intact.
    pub fn with_signature(&self) -> Eeprom {
        let mut e = self.clone();
        e.bytes[SIGNATURE_OFF..SIGNATURE_OFF + SIGNATURE.len()].copy_from_slice(&SIGNATURE);
        e
    }

    /// The patch list's length in bytes as the firmware takes it (capped
    /// at 178, eeprom.md §5), or None without the marker at 0x4E.
    pub fn patch_list_len(&self) -> Option<usize> {
        (self.bytes[PATCH_MARKER_OFF] == PATCH_MARKER).then(|| (self.bytes[PATCH_LENGTH_OFF] as usize).min(PATCH_LENGTH_CAP))
    }

    /// The same image, cleared to be written with a patch-list marker at
    /// 0x4E ([`Eeprom::check_writable`]).
    pub fn allowing_patch_list(mut self) -> Eeprom {
        self.patch_list_allowed = true;
        self
    }

    /// Refuses an image the firmware would act on dangerously at boot: a
    /// test record (byte 0 = 0x12 or byte 1 = 0x34, eeprom.md §6 — "a
    /// driver writing an EEPROM must keep E[0] ≠ 0x12 and E[1] ≠ 0x34"),
    /// and, unless allowed, a patch-list marker at 0x4E (the following
    /// bytes would be written into the 8051's data space before
    /// enumeration, findings 17; eeprom.md §5).
    pub fn check_writable(&self) -> Result<()> {
        if self.is_test_record() {
            return Err(refused(format!(
                "bytes 0-1 0x{:02X} 0x{:02X} select the firmware's test record (byte 0 = 0x{TEST_RECORD_BYTE0:02X} or byte 1 = 0x{TEST_RECORD_BYTE1:02X}, eeprom.md §6); never written",
                self.bytes[0], self.bytes[1]
            )));
        }
        if !self.patch_list_allowed && self.patch_list_len().is_some() {
            return Err(refused(format!(
                "byte 0x{PATCH_MARKER_OFF:02X} is the patch-list marker 0x{PATCH_MARKER:02X}: the firmware would write the bytes from 0x{PATCH_RECORDS_OFF:02X} into its data space at every boot (findings 17, eeprom.md §5); allow the patch list explicitly to write it"
            )));
        }
        Ok(())
    }

    /// The string descriptors' places by the firmware's rules (eeprom.md
    /// §2 step 4, §3), or why it would fall back to its defaults. Every
    /// bLength must also be at least 2 (eeprom.md §9 item 9: with 0 the
    /// firmware re-reads the same place); the sum is checked as a plain
    /// sum, where the firmware's is 8-bit (§9 item 9) — lengths whose
    /// sum wraps are reported as over the limit, not modelled.
    fn strings(&self) -> Result<Strings> {
        let b = &self.bytes;
        let at = |name: &str, start: usize| -> Result<Range<usize>> {
            let len = *b.get(start).ok_or_else(|| refused(format!("the {name} descriptor would start at 0x{start:02X}, past the {}-byte part", b.len())))? as usize;
            if len < DESCRIPTOR_HEADER {
                return Err(refused(format!("the {name} descriptor at 0x{start:02X} has bLength {len}; a descriptor is at least {DESCRIPTOR_HEADER}")));
            }
            Ok(start..start + len)
        };
        let manufacturer = at("manufacturer", STRINGS_OFF)?;
        let product = at("product", manufacturer.end)?;
        let pair = manufacturer.len() + product.len();
        if pair > MANUFACTURER_PRODUCT_MAX {
            return Err(refused(format!(
                "manufacturer and product descriptors are {} + {} = {pair} bytes; the firmware takes at most {MANUFACTURER_PRODUCT_MAX} (ROM 0x0C47-0x0C4E)",
                manufacturer.len(),
                product.len()
            )));
        }
        let serial = if b[SERIAL_FLAG_OFF] == SERIAL_PRESENT {
            let s = at("serial", product.end)?;
            if s.len() > SERIAL_MAX {
                return Err(refused(format!("the serial descriptor is {} bytes; the firmware takes at most {SERIAL_MAX} (ROM 0x0CE5-0x0CEA)", s.len())));
            }
            Some(s)
        } else {
            None
        };
        // Within the limits every descriptor ends by 0x77, inside any part
        // (the assertion beside SERIAL_MAX).
        Ok(Strings { manufacturer, product, serial })
    }

    /// The offset just past the last string descriptor.
    pub fn strings_end(&self) -> Result<usize> {
        Ok(self.strings()?.end())
    }

    /// The identity the image holds, read by the layout whether or not
    /// the signature is valid ([`Eeprom::is_valid`] says whether the
    /// firmware uses it): findings 17's dongle held its whole record
    /// behind a broken byte 0. Err when the strings break the firmware's
    /// rules (it would serve its default strings).
    pub fn identity(&self) -> Result<EepromIdentity> {
        let b = &self.bytes;
        let s = self.strings()?;
        Ok(EepromIdentity {
            vid: u16::from_le_bytes([b[VID_OFF], b[VID_OFF + 1]]),
            pid: u16::from_le_bytes([b[PID_OFF], b[PID_OFF + 1]]),
            serial_present: s.serial.is_some(),
            config: EepromConfig(b[CONFIG_OFF]),
            manufacturer: decode(&b[s.manufacturer]),
            product: decode(&b[s.product]),
            serial: s.serial.map(|r| decode(&b[r])).unwrap_or_default(),
        })
    }

    /// The identity the firmware enumerates with from this image
    /// (eeprom.md §2, §7): the ROM's defaults for an invalid signature or
    /// a test record; for a valid record whose strings break the rules,
    /// the record's IDs and serial flag with the default strings and
    /// configuration bits 0 and 2 forced on; otherwise the record.
    pub fn served_identity(&self) -> EepromIdentity {
        if !self.is_valid() || self.is_test_record() {
            return EepromIdentity::rom_defaults();
        }
        self.identity().unwrap_or_else(|_| {
            let b = &self.bytes;
            let serial_present = b[SERIAL_FLAG_OFF] == SERIAL_PRESENT;
            EepromIdentity {
                vid: u16::from_le_bytes([b[VID_OFF], b[VID_OFF + 1]]),
                pid: u16::from_le_bytes([b[PID_OFF], b[PID_OFF + 1]]),
                serial_present,
                config: EepromConfig(b[CONFIG_OFF] | EepromConfig::DEFAULTS_FORCED),
                manufacturer: ROM_MANUFACTURER.into(),
                product: ROM_PRODUCT.into(),
                serial: if serial_present { ROM_SERIAL.into() } else { String::new() },
            }
        })
    }

    /// A new image with only the given fields changed. The starting
    /// identity is the image's own ([`Eeprom::identity`]) when its
    /// strings follow the rules, else the one the dongle enumerates with
    /// now ([`Eeprom::served_identity`]: a blank part starts from the
    /// ROM's defaults and keeps behaving as it did). The result:
    ///
    /// - bytes 0-1 the signature, always — a record without it is never
    ///   used (eeprom.md §2), so composing one would change nothing;
    /// - idVendor / idProduct, the serial flag (0xA5 with a serial, else
    ///   unchanged if already not 0xA5, else 0x00) and the starting
    ///   identity's configuration byte; byte 8 kept;
    /// - the three descriptors from 0x09, each after the one before's
    ///   bLength, then **zeros to the end of the part**, so nothing of an
    ///   earlier, longer record survives — a patch list included, unless
    ///   `allow_patch_list` keeps the image's own.
    ///
    /// Refused, naming the limit: manufacturer + product over 70 bytes as
    /// descriptors, a serial over 40 (eeprom.md §2 step 4); a string
    /// byte landing on 0x4E as the marker 0x5C (it would be run as a
    /// patch list, findings 17 — with descriptors from 0x09 and even
    /// lengths only a character's high byte can land there, a character
    /// U+5C00-U+5CFF); strings reaching a patch list being kept. Every
    /// identity within the limits fits a 128-byte part.
    pub fn compose(&self, changes: &EepromChanges) -> Result<Eeprom> {
        let base = self.identity().unwrap_or_else(|_| self.served_identity());
        let manufacturer = changes.manufacturer.clone().unwrap_or(base.manufacturer);
        let product = changes.product.clone().unwrap_or(base.product);
        let serial = match &changes.serial {
            Some(s) if s.is_empty() => None,
            Some(s) => Some(s.clone()),
            None => base.serial_present.then_some(base.serial),
        };
        let m = descriptor(&manufacturer);
        let p = descriptor(&product);
        let pair = m.len() + p.len();
        if pair > MANUFACTURER_PRODUCT_MAX {
            return Err(refused(format!(
                "manufacturer {manufacturer:?} and product {product:?} are {} + {} = {pair} bytes as string descriptors; the firmware takes at most {MANUFACTURER_PRODUCT_MAX} together (ROM 0x0C47-0x0C4E, eeprom.md §2), {} characters in all",
                m.len(),
                p.len(),
                chars_in(MANUFACTURER_PRODUCT_MAX - DESCRIPTOR_HEADER)
            )));
        }
        let s = serial.as_deref().map(descriptor);
        if let Some(d) = &s
            && d.len() > SERIAL_MAX
        {
            return Err(refused(format!(
                "serial {:?} is {} bytes as a string descriptor; the firmware takes at most {SERIAL_MAX} (ROM 0x0CE5-0x0CEA, eeprom.md §2), {} characters",
                serial.as_deref().unwrap_or_default(),
                d.len(),
                chars_in(SERIAL_MAX)
            )));
        }

        let mut b = self.bytes.clone();
        b[SIGNATURE_OFF..SIGNATURE_OFF + SIGNATURE.len()].copy_from_slice(&SIGNATURE);
        b[VID_OFF..VID_OFF + 2].copy_from_slice(&changes.vid.unwrap_or(base.vid).to_le_bytes());
        b[PID_OFF..PID_OFF + 2].copy_from_slice(&changes.pid.unwrap_or(base.pid).to_le_bytes());
        b[SERIAL_FLAG_OFF] = match (&s, b[SERIAL_FLAG_OFF]) {
            (Some(_), _) => SERIAL_PRESENT,
            (None, SERIAL_PRESENT) => SERIAL_ABSENT,
            (None, other) => other,
        };
        b[CONFIG_OFF] = base.config.0;
        let strings: Vec<u8> = [m, p, s.unwrap_or_default()].concat();
        let end = STRINGS_OFF + strings.len();
        b[STRINGS_OFF..end].copy_from_slice(&strings);
        b[end..].fill(0);

        let kept = if changes.allow_patch_list { self.patch_list_len() } else { None };
        if let Some(n) = kept {
            if end > PATCH_MARKER_OFF {
                return Err(refused(format!(
                    "the strings end at 0x{end:02X}, over the patch list kept at 0x{PATCH_MARKER_OFF:02X}: they must end by 0x{PATCH_MARKER_OFF:02X} ({} bytes of descriptors) to keep it (eeprom.md §3, overlap)",
                    PATCH_MARKER_OFF - STRINGS_OFF
                )));
            }
            let list_end = (PATCH_RECORDS_OFF + n).min(b.len());
            b[PATCH_MARKER_OFF..list_end].copy_from_slice(&self.bytes[PATCH_MARKER_OFF..list_end]);
        } else if b[PATCH_MARKER_OFF] == PATCH_MARKER {
            return Err(refused(format!(
                "the strings put 0x{PATCH_MARKER:02X} at 0x{PATCH_MARKER_OFF:02X}, the patch-list marker: the firmware would run the bytes after it as register patches at boot (findings 17, eeprom.md §5); a character U+5C00-U+5CFF lands there — move or replace it"
            )));
        }
        Ok(Eeprom { bytes: b, patch_list_allowed: kept.is_some() })
    }
}

/// The [`EEPROM_WRITE_CHUNK`]-aligned chunks in which two images of one part differ,
/// by start offset, ascending: what a write sends.
pub fn changed_chunks(before: &Eeprom, after: &Eeprom) -> Vec<usize> {
    (0..after.size().min(before.size()))
        .step_by(EEPROM_WRITE_CHUNK)
        .filter(|&off| {
            let r = off..(off + EEPROM_WRITE_CHUNK).min(after.size());
            before.bytes[r.clone()] != after.bytes[r]
        })
        .collect()
}

/// What a write did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EepromWriteReport {
    /// The part as read before the write.
    pub before: Eeprom,
    /// The [`EEPROM_WRITE_CHUNK`]-byte chunks written, by start offset (none when the part
    /// already held the image).
    pub chunks: Vec<usize>,
    /// The bytes that changed.
    pub bytes_changed: usize,
}

/// All 256 offsets read, [`EEPROM_REQUEST_MAX`] bytes a request.
#[cfg(feature = "std")]
pub(crate) async fn read_raw(link: &crate::usb::Link) -> Result<Vec<u8>> {
    let mut raw = Vec::with_capacity(EEPROM_ADDRESS_SPACE);
    for off in (0..EEPROM_ADDRESS_SPACE).step_by(EEPROM_REQUEST_MAX) {
        raw.extend(link.eeprom_read(off as u8, EEPROM_REQUEST_MAX).await?);
    }
    Ok(raw)
}

/// The write behind `Rtl::write_eeprom`: the part read once, the chunks
/// that differ written one control transfer each with
/// [`EEPROM_WRITE_CYCLE`] after each, the whole part read back and
/// compared.
#[cfg(feature = "std")]
pub(crate) async fn write(link: &crate::usb::Link, new: &Eeprom) -> Result<EepromWriteReport> {
    new.check_writable()?;
    let before = Eeprom::from_read(&read_raw(link).await?)?;
    if before.size() != new.size() {
        return Err(refused(format!(
            "the part reads as {} bytes (findings 17: a 128-byte part repeats in the upper half), the image is {}; compose the image from this part's read",
            before.size(),
            new.size()
        )));
    }
    let chunks = changed_chunks(&before, new);
    for (i, &off) in chunks.iter().enumerate() {
        let data = &new.bytes[off..off + EEPROM_WRITE_CHUNK];
        if let Err(e) = link.eeprom_write(off as u8, data).await {
            return Err(Error::Device {
                backend: BACKEND,
                context: "EEPROM write",
                message: format!("chunk 0x{off:02X} ({} of {}, the ones before it written): {e}", i + 1, chunks.len()),
            });
        }
        sdr_core::time::sleep(EEPROM_WRITE_CYCLE).await;
    }
    let back = read_raw(link).await?;
    let differ: Vec<String> = (0..new.size()).filter(|&i| back[i] != new.bytes[i]).map(|i| format!("0x{i:02X}: wrote 0x{:02X}, read 0x{:02X}", new.bytes[i], back[i])).collect();
    if !differ.is_empty() {
        return Err(Error::Device { backend: BACKEND, context: "EEPROM read-back", message: format!("{} bytes differ: {}", differ.len(), differ.join("; ")) });
    }
    let bytes_changed = (0..new.size()).filter(|&i| before.bytes[i] != new.bytes[i]).count();
    Ok(EepromWriteReport { before, chunks, bytes_changed })
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use alloc::string::ToString;
    use super::*;
    use crate::protocol::{ACCESS_GET, ACCESS_SET, windex};

    fn blank(size: usize) -> Eeprom {
        Eeprom::from_bytes(vec![0xFF; size]).unwrap()
    }

    fn changes(m: Option<&str>, p: Option<&str>, s: Option<&str>) -> EepromChanges {
        EepromChanges { manufacturer: m.map(Into::into), product: p.map(Into::into), serial: s.map(Into::into), ..Default::default() }
    }

    /// Findings 17's bench record with neutral strings of its strings'
    /// lengths (7, 7, 3): 0bda:2838, serial flag 0xA5, configuration
    /// 0x16, "Example" / "Product" / "SN1".
    fn bench_record() -> Eeprom {
        let mut b = vec![0u8; EEPROM_SMALL_PART];
        b[..9].copy_from_slice(&[0x28, 0x32, 0xDA, 0x0B, 0x38, 0x28, SERIAL_PRESENT, 0x16, 0x00]);
        let strings = [descriptor("Example"), descriptor("Product"), descriptor("SN1")].concat();
        b[STRINGS_OFF..STRINGS_OFF + strings.len()].copy_from_slice(&strings);
        Eeprom::from_bytes(b).unwrap()
    }

    /// eeprom.md §8: block 3 Get 0x0300 / Set 0x0310, wValue offset << 8 | 0xA0.
    #[test]
    fn requests_are_block_3_at_0xa0() {
        assert_eq!(windex(EEPROM_BLOCK, ACCESS_GET, 0), 0x0300);
        assert_eq!(windex(EEPROM_BLOCK, ACCESS_SET, 0), 0x0310);
        assert_eq!(wvalue_eeprom(0x00), 0x00A0);
        assert_eq!(wvalue_eeprom(0x4E), 0x4EA0);
        assert_eq!(wvalue_eeprom(0xF0), 0xF0A0);
    }

    /// Block 0 at 0xA0 (or 0xA1) is the hazard; block 3 at 0xA0 and
    /// block 0 at the demodulator's 0x20 are not.
    #[test]
    fn block0_to_the_eeprom_is_recognised() {
        assert!(block0_addresses_eeprom(0x00A0, windex(BLOCK_DEMOD, ACCESS_GET, 0)));
        assert!(block0_addresses_eeprom(0x10A1, windex(BLOCK_DEMOD, ACCESS_SET, 3)));
        assert!(!block0_addresses_eeprom(wvalue_eeprom(0), windex(EEPROM_BLOCK, ACCESS_GET, 0)));
        assert!(!block0_addresses_eeprom(wvalue_eeprom(0), windex(EEPROM_BLOCK, ACCESS_SET, 0)));
        assert!(!block0_addresses_eeprom(0x0120, windex(BLOCK_DEMOD, ACCESS_SET, 1)));
    }

    /// Findings 17: a 128-byte part repeats in 0x80-0xFF.
    #[test]
    fn a_mirrored_read_is_a_128_byte_part() {
        let low = bench_record().bytes().to_vec();
        let mirrored = [low.clone(), low.clone()].concat();
        assert_eq!(part_size(&mirrored), EEPROM_SMALL_PART);
        assert_eq!(Eeprom::from_read(&mirrored).unwrap(), bench_record());
        let mut distinct = mirrored.clone();
        distinct[0xC0] ^= 1;
        assert_eq!(Eeprom::from_read(&distinct).unwrap().size(), EEPROM_ADDRESS_SPACE);
        assert_eq!(Eeprom::from_read(&[0xFF; EEPROM_ADDRESS_SPACE]).unwrap().size(), EEPROM_SMALL_PART, "equal halves: the safe size");
        assert!(Eeprom::from_read(&low).is_err(), "a read is all 256 offsets");
        assert!(Eeprom::from_bytes(vec![0; 100]).is_err());
    }

    #[test]
    fn the_bench_record_parses() {
        let e = bench_record();
        assert!(e.is_valid());
        let id = e.identity().unwrap();
        assert_eq!((id.vid, id.pid), (0x0BDA, 0x2838));
        assert!(id.serial_present);
        assert_eq!((id.manufacturer.as_str(), id.product.as_str(), id.serial.as_str()), ("Example", "Product", "SN1"));
        assert_eq!(id.config, EepromConfig(0x16));
        assert!(id.config.second_interface() && id.config.gpio_setup() && !id.config.remote_wakeup());
        assert!(!id.config.demod_ctl_1_set() && !id.config.demod_ctl_1_bit1_clear(), "bit 4 alone acts only with bit 3");
        // 0x09 + 16 + 16 + 8 (findings 17: "chained by bLength exactly").
        assert_eq!(e.strings_end().unwrap(), 0x31);
        assert_eq!(e.served_identity(), id);
    }

    /// Findings 17: byte 0 at 0x00 hides an intact record; the repair
    /// sets the signature and nothing else.
    #[test]
    fn with_signature_repairs_a_broken_byte_0() {
        let mut b = bench_record().bytes().to_vec();
        b[0] = 0x00;
        let broken = Eeprom::from_bytes(b).unwrap();
        assert!(!broken.is_valid());
        assert_eq!(broken.served_identity(), EepromIdentity::rom_defaults());
        assert_eq!(broken.identity().unwrap(), bench_record().identity().unwrap(), "the record is still readable");
        let repaired = broken.with_signature();
        assert!(repaired.is_valid());
        assert_eq!(repaired, bench_record());
        // Composing with no change does the same.
        assert_eq!(broken.compose(&EepromChanges::default()).unwrap(), bench_record());
    }

    #[test]
    fn compose_round_trips_and_changes_only_what_is_given() {
        let e = bench_record();
        let new = e.compose(&changes(None, None, Some("SN2"))).unwrap();
        let id = new.identity().unwrap();
        assert_eq!((id.manufacturer.as_str(), id.product.as_str(), id.serial.as_str()), ("Example", "Product", "SN2"));
        assert_eq!((id.vid, id.pid, id.config), (0x0BDA, 0x2838, EepromConfig(0x16)));
        assert_eq!(new.bytes()[..STRINGS_OFF], e.bytes()[..STRINGS_OFF]);
        assert_eq!(changed_chunks(&e, &new), vec![0x28]);

        let ids = e.compose(&EepromChanges { vid: Some(0x1234), pid: Some(0x5678), ..Default::default() }).unwrap();
        assert_eq!(&ids.bytes()[VID_OFF..PID_OFF + 2], &[0x34, 0x12, 0x78, 0x56]);
        assert_eq!(ids.identity().unwrap().serial, "SN1");

        let all = e.compose(&changes(Some("Ägypten"), Some("Ω"), Some("123"))).unwrap();
        let id = all.identity().unwrap();
        assert_eq!((id.manufacturer.as_str(), id.product.as_str(), id.serial.as_str()), ("Ägypten", "Ω", "123"));
        assert_eq!(all.bytes()[STRINGS_OFF + 1], STRING_DESCRIPTOR_TYPE);
    }

    #[test]
    fn an_empty_serial_removes_it() {
        let new = bench_record().compose(&changes(None, None, Some(""))).unwrap();
        assert_eq!(new.bytes()[SERIAL_FLAG_OFF], SERIAL_ABSENT);
        let id = new.identity().unwrap();
        assert!(!id.serial_present);
        assert_eq!(id.serial, "");
        assert_eq!(new.strings_end().unwrap(), 0x29);
        assert!(new.bytes()[0x29..].iter().all(|&b| b == 0), "the old serial is zeroed");
        // And back.
        let again = new.compose(&changes(None, None, Some("SN1"))).unwrap();
        assert_eq!(again, bench_record());
    }

    /// The string area is zeroed from the last descriptor to the end of
    /// the part, whatever was there (an earlier record, a longer name).
    #[test]
    fn the_tail_is_zeroed_to_the_end_of_the_part() {
        for size in [EEPROM_SMALL_PART, EEPROM_ADDRESS_SPACE] {
            let mut b = vec![0xEE; size];
            b[..bench_record().size()].copy_from_slice(bench_record().bytes());
            b[0x31..].fill(0xEE);
            let e = Eeprom::from_bytes(b).unwrap();
            let new = e.compose(&changes(None, None, Some("A"))).unwrap();
            let end = new.strings_end().unwrap();
            assert_eq!(end, 0x2D);
            assert_eq!(new.size(), size);
            assert!(new.bytes()[end..].iter().all(|&b| b == 0), "{size}-byte part");
        }
    }

    /// A blank part enumerates with the ROM's defaults (eeprom.md §7);
    /// composing on it starts from them.
    #[test]
    fn a_blank_part_starts_from_the_rom_defaults() {
        let e = blank(EEPROM_SMALL_PART);
        assert!(!e.is_valid());
        assert!(e.identity().is_err(), "bLength 0xFF breaks the limits");
        assert_eq!(e.served_identity(), EepromIdentity::rom_defaults());
        let new = e.compose(&changes(None, None, Some("SN2"))).unwrap();
        assert!(new.is_valid());
        let id = new.identity().unwrap();
        assert_eq!((id.vid, id.pid), (ROM_VID, ROM_PID));
        assert_eq!((id.manufacturer.as_str(), id.product.as_str(), id.serial.as_str()), (ROM_MANUFACTURER, ROM_PRODUCT, "SN2"));
        assert_eq!(id.config, EepromConfig(EepromConfig::REMOTE_WAKEUP | EepromConfig::GPIO_SETUP));
        assert_eq!(new.bytes()[TEST_SLAVE_OFF], 0xFF, "byte 8 is kept");
        // The ROM's own descriptors are 16 and 18 bytes (0x0E40, 0x0E50)
        // and the default serial 36 (0x0E62's 17 characters).
        assert_eq!(descriptor(ROM_MANUFACTURER).len(), 16);
        assert_eq!(descriptor(ROM_PRODUCT).len(), 18);
        assert_eq!(descriptor(ROM_SERIAL).len(), 36);
        assert!(blank(EEPROM_SMALL_PART).compose(&EepromChanges::default()).unwrap().identity().unwrap().serial == ROM_SERIAL);
    }

    /// A valid record whose strings break the limits is served with its
    /// IDs and the default strings, bits 0 and 2 forced (eeprom.md §7).
    #[test]
    fn over_long_strings_are_served_as_the_defaults() {
        let mut b = bench_record().bytes().to_vec();
        b[STRINGS_OFF] = 72;
        let e = Eeprom::from_bytes(b).unwrap();
        assert!(e.identity().is_err());
        let served = e.served_identity();
        assert_eq!((served.vid, served.pid), (0x0BDA, 0x2838));
        assert_eq!(served.config, EepromConfig(0x16 | EepromConfig::DEFAULTS_FORCED));
        assert_eq!((served.manufacturer.as_str(), served.serial.as_str()), (ROM_MANUFACTURER, ROM_SERIAL));
    }

    #[test]
    fn the_firmwares_limits_are_refused_by_name() {
        let e = bench_record();
        // 70 bytes of manufacturer + product: 33 characters in all.
        let at_limit = e.compose(&changes(Some(&"M".repeat(16)), Some(&"P".repeat(17)), None)).unwrap();
        assert_eq!(at_limit.identity().unwrap().product.len(), 17);
        let over = e.compose(&changes(Some(&"M".repeat(17)), Some(&"P".repeat(17)), None)).unwrap_err().to_string();
        assert!(over.contains("at most 70") && over.contains("33 characters"), "{over}");
        // 40 bytes of serial: 19 characters.
        assert!(e.compose(&changes(None, None, Some(&"S".repeat(19)))).is_ok());
        let over = e.compose(&changes(None, None, Some(&"S".repeat(20)))).unwrap_err().to_string();
        assert!(over.contains("at most 40") && over.contains("19 characters"), "{over}");
        // The longest identity ends at 0x77, inside a 128-byte part.
        let longest = blank(EEPROM_SMALL_PART).compose(&changes(Some(&"M".repeat(16)), Some(&"P".repeat(17)), Some(&"S".repeat(19)))).unwrap();
        assert_eq!(longest.strings_end().unwrap(), 0x77);
        // A descriptor shorter than its own header is refused on parse.
        let mut b = bench_record().bytes().to_vec();
        b[0x19] = 0;
        assert!(Eeprom::from_bytes(b).unwrap().identity().unwrap_err().to_string().contains("bLength 0"));
    }

    /// A string byte can land on 0x4E as 0x5C only as the high byte of a
    /// character U+5C00-U+5CFF: the product's 32nd character after a
    /// one-character manufacturer (0x09 + 4 + 2 + 2·31 + 1 = 0x4E).
    #[test]
    fn a_marker_from_the_strings_is_refused() {
        let product: String = std::iter::repeat_n('P', 31).chain(std::iter::once('\u{5C41}')).collect();
        let c = changes(Some("A"), Some(&product), Some(""));
        let err = bench_record().compose(&c).unwrap_err().to_string();
        assert!(err.contains("0x4E") && err.contains("0x5C"), "{err}");
        let c = EepromChanges { allow_patch_list: true, ..c };
        assert!(bench_record().compose(&c).is_err(), "the strings are never a patch list");
        // The same length with an ASCII character is fine: its high byte is 0.
        let product: String = "P".repeat(32);
        let ok = bench_record().compose(&changes(Some("A"), Some(&product), Some(""))).unwrap();
        assert_eq!(ok.bytes()[PATCH_MARKER_OFF], 0x00);
        assert!(ok.check_writable().is_ok());
    }

    /// A patch list is removed with the tail unless kept on purpose; an
    /// image holding one is written only when allowed (findings 17).
    #[test]
    fn a_patch_list_is_written_only_when_allowed() {
        let mut b = bench_record().bytes().to_vec();
        b[PATCH_MARKER_OFF] = PATCH_MARKER;
        b[PATCH_LENGTH_OFF] = 3;
        b[PATCH_RECORDS_OFF..PATCH_RECORDS_OFF + 3].copy_from_slice(&[0x30, 0x03, 0x01]);
        b[0x60] = 0xEE;
        let with_list = Eeprom::from_bytes(b).unwrap();
        assert_eq!(with_list.patch_list_len(), Some(3));
        assert!(with_list.check_writable().is_err());
        assert!(with_list.clone().allowing_patch_list().check_writable().is_ok());

        let removed = with_list.compose(&changes(None, None, Some("SN2"))).unwrap();
        assert_eq!(removed.patch_list_len(), None);
        assert!(removed.bytes()[0x31..].iter().all(|&b| b == 0));

        let kept = with_list.compose(&EepromChanges { serial: Some("SN2".into()), allow_patch_list: true, ..Default::default() }).unwrap();
        assert_eq!(&kept.bytes()[PATCH_MARKER_OFF..PATCH_RECORDS_OFF + 3], &[PATCH_MARKER, 3, 0x30, 0x03, 0x01]);
        assert_eq!(kept.bytes()[0x60], 0, "past the list the tail is zeroed");
        assert!(kept.check_writable().is_ok());
        // Strings reaching 0x4E would overwrite the list kept.
        let long = EepromChanges { manufacturer: Some("M".repeat(16)), product: Some("P".repeat(17)), allow_patch_list: true, ..Default::default() };
        assert!(with_list.compose(&long).unwrap_err().to_string().contains("patch list kept"));
        // The cap: a length byte over 178 counts as 178 (eeprom.md §5).
        let mut b = with_list.bytes().to_vec();
        b[PATCH_LENGTH_OFF] = 0xFF;
        assert_eq!(Eeprom::from_bytes(b).unwrap().patch_list_len(), Some(PATCH_LENGTH_CAP));
    }

    /// eeprom.md §6: a driver keeps byte 0 ≠ 0x12 and byte 1 ≠ 0x34.
    #[test]
    fn a_test_record_is_never_written() {
        for (i, v) in [(0, TEST_RECORD_BYTE0), (1, TEST_RECORD_BYTE1)] {
            let mut b = bench_record().bytes().to_vec();
            b[i] = v;
            let e = Eeprom::from_bytes(b).unwrap();
            assert!(e.is_test_record());
            assert!(e.check_writable().unwrap_err().to_string().contains("test record"));
            assert_eq!(e.served_identity(), EepromIdentity::rom_defaults());
            assert!(e.compose(&EepromChanges::default()).unwrap().check_writable().is_ok(), "compose writes the signature");
        }
    }

    #[test]
    fn changed_chunks_are_aligned_pages() {
        let a = blank(EEPROM_SMALL_PART);
        let mut b = a.bytes().to_vec();
        b[0x11] = 0;
        b[0x17] = 0;
        b[0x1F] = 0;
        b[0x7F] = 0;
        let b = Eeprom::from_bytes(b).unwrap();
        assert_eq!(changed_chunks(&a, &b), vec![0x10, 0x18, 0x78]);
        assert!(changed_chunks(&a, &a).is_empty());
        assert!(changed_chunks(&a, &b).iter().all(|off| off % EEPROM_WRITE_CHUNK == 0 && off + EEPROM_WRITE_CHUNK <= b.size()));
    }
}
