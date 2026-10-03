//! The RTL2832U: its vendor control protocol, its USB and system
//! registers, and the demodulator registers SDR mode programs.
//!
//! Point of origin: Realtek, "RTL2832U Datasheet" Rev. 1.4 (read from
//! its page images) — Table 27/28 (vendor commands
//! and the `wIndex` layout), Tables 3-11 and 13 (demodulator registers
//! by page, offset and bits), Table 15 (DEMOD_CTL), Tables 30-33 (USB
//! system and endpoint A). Register bits the datasheet does not list are
//! named from the Linux kernel's `rtl2832` register table, then from
//! librtlsdr's comments; a bit no source names is named for its page,
//! register and position. Sequences and values follow rtl-sdr-rs
//! (MPL-2.0) and librtlsdr.

/// USB VID/PID pairs this backend claims. The two Realtek IDs cover every
/// RTL-SDR-branded dongle (Blog V3/V4 included); the DVB-T OEM zoo is out
/// of scope until someone owns one.
pub const KNOWN_IDS: &[(u16, u16, &str)] = &[
    (0x0bda, 0x2832, "Generic RTL2832U"),
    (0x0bda, 0x2838, "RTL2832U DVB-T (RTL-SDR)"),
    // A dongle whose EEPROM rewrite wrote the string descriptors but
    // left the VID:PID bytes ERASED (0xFF everywhere — that is what an
    // unprogrammed EEPROM reads as). The chip underneath is a normal
    // RTL2832U; the erased ID just makes it invisible without this
    // entry, and on Linux it needs a udev rule for ffff:ffff too.
    (0xffff, 0xffff, "RTL2832U (erased-ID EEPROM)"),
];

// ---- the vendor control protocol (Table 27, Table 28) ------------------------

/// `wIndex` nibble 2, the block (0 is the demodulator, whose accesses
/// carry the page instead): the USB and system register blocks, and the
/// standard I2C command block (the tuner and the EEPROM behind the
/// repeater).
pub const BLOCK_USB: u16 = 1;
pub const BLOCK_SYS: u16 = 2;
pub const BLOCK_IIC: u16 = 6;
/// `wIndex` nibble 1, the access: 0 Get, 1 Set.
pub const WINDEX_SET: u16 = 0x0010;
/// The low byte of a demodulator access's `wValue`, under the offset in
/// its high byte. Table 27 gives only "Reg's offset"; librtlsdr and
/// rtl-sdr-rs send this with every demodulator access.
pub const DEMOD_VALUE_TAG: u16 = 0x20;
/// Where the block sits in `wIndex` (nibble 2), and the offset in a
/// demodulator access's `wValue` (its high byte).
pub const WINDEX_BLOCK_SHIFT: u16 = 8;
pub const DEMOD_OFFSET_SHIFT: u16 = 8;

/// The USB and system blocks' register addresses: the datasheet's
/// offsets (Tables 15, 30-33) above these bases (librtlsdr's `USBB` /
/// `SYSB` addresses; Table 27's "(BaseAdd)<<8 + OffsetAdd").
const USB_BASE: u16 = 0x2000;
const SYS_BASE: u16 = 0x3000;

// ---- system block (§10) -----------------------------------------------------------

/// DEMOD_CTL (Table 15).
pub const DEMOD_CTL: u16 = SYS_BASE + 0x0000;
pub const DEMOD_CTL_PLL_ENABLE: u8 = 1 << 7;
pub const DEMOD_CTL_ADC_I_ENABLE: u8 = 1 << 6;
/// 0 holds the demodulator in hardware reset, 1 releases it.
pub const DEMOD_CTL_RESET_RELEASED: u8 = 1 << 5;
pub const DEMOD_CTL_ADC_Q_ENABLE: u8 = 1 << 3;
/// GPIO output value, input value, output enable, direction (§10.2).
pub const GPO: u16 = SYS_BASE + 0x0001;
pub const GPOE: u16 = SYS_BASE + 0x0003;
pub const GPD: u16 = SYS_BASE + 0x0004;
/// Not in the datasheet's §10; librtlsdr's `DEMOD_CTL_1`, written 0x22
/// ("poweron demod").
pub const DEMOD_CTL_1: u16 = SYS_BASE + 0x000b;
pub const DEMOD_CTL_1_POWER_ON: u8 = 0x22;

// ---- USB block (§11.4) ------------------------------------------------------------

/// USB_SYSCTL (Table 30).
pub const USB_SYSCTL: u16 = USB_BASE + 0x0000;
pub const USB_SYSCTL_FULL_PACKET_MODE: u8 = 1 << 3;
pub const USB_SYSCTL_DMA_ENABLE: u8 = 1 << 0;
/// USB_EPA_CTL (Table 32).
pub const USB_EPA_CTL: u16 = USB_BASE + 0x0148;
pub const USB_EPA_CTL_FIFO_RESET: u16 = 1 << 9;
pub const USB_EPA_CTL_STALL: u16 = 1 << 4;
/// USB_EPA_MAXPKT (Table 33): endpoint A's maximum packet, in bytes —
/// 512, the high-speed bulk size.
pub const USB_EPA_MAXPKT: u16 = USB_BASE + 0x0158;
pub const USB_EPA_MAX_PACKET_BYTES: u16 = 512;

/// The RTL2832U's crystal. The resampler ratio, IF programming and (on all
/// but the V4's R828D) the tuner PLL reference all derive from it.
pub const RTL_XTAL_HZ: u32 = 28_800_000;

/// Default baseband FIR coefficients (8 × i8 then 8 × i12), loaded into
/// [`FIR_COEFFICIENTS`] at init.
pub const DEFAULT_FIR: [i32; 16] = [
    -54, -36, -41, -40, -32, -14, 14, 53, // i8
    101, 156, 215, 273, 327, 372, 404, 421, // i12
];

/// Bulk IQ endpoint.
pub const EP_BULK_IN: u8 = 0x81;

// ---- demodulator registers ------------------------------------------------------

/// A demodulator register: page (0-4, the `wIndex` page nibble) and
/// offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DemodReg {
    pub page: u16,
    pub addr: u16,
}

const fn demod(page: u16, addr: u16) -> DemodReg {
    DemodReg { page, addr }
}

/// One field of a demodulator register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DemodField {
    pub reg: DemodReg,
    pub shift: u8,
    pub width: u8,
}

/// Field values bound for one demodulator register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DemodBits {
    pub reg: DemodReg,
    pub value: u8,
    pub mask: u8,
}

impl DemodField {
    const fn at(reg: DemodReg, hi: u8, lo: u8) -> DemodField {
        DemodField { reg, shift: lo, width: hi - lo + 1 }
    }

    pub const fn mask(self) -> u8 {
        (((1u16 << self.width) - 1) as u8) << self.shift
    }

    pub const fn set(self, value: u8) -> DemodBits {
        DemodBits { reg: self.reg, value: (value << self.shift) & self.mask(), mask: self.mask() }
    }
}

impl DemodBits {
    pub const fn with(self, other: DemodBits) -> DemodBits {
        assert!(self.reg.page == other.reg.page && self.reg.addr == other.reg.addr, "fields of different registers");
        assert!(self.mask & other.mask == 0, "overlapping fields");
        DemodBits { reg: self.reg, value: self.value | other.value, mask: self.mask | other.mask }
    }

    /// The register's whole byte: demodulator writes are whole-byte, so
    /// every bit must be bound.
    pub const fn byte(self) -> u8 {
        assert!(self.mask == 0xff, "a demodulator write must bind all eight bits");
        self.value
    }
}

/// librtlsdr reads this after every demodulator write — "the chip
/// latches demod writes on the next access". Page 10 lies outside the
/// datasheet's pages 0-4.
pub const DEMOD_WRITE_LATCH: DemodReg = demod(0x0a, 0x01);

// Page 0.

/// Page 0, 0x06: `opt_adc_iq` (Table 4: 0 default ADC_I / ADC_Q datapath,
/// 1 exchanged) under the kernel's `MPEG_IO_OPT_2_2`.
const P0_06: DemodReg = demod(0, 0x06);
pub const MPEG_IO_OPT_2_2: DemodField = DemodField::at(P0_06, 7, 7);
pub const P0_06_BIT6: DemodField = DemodField::at(P0_06, 6, 6);
pub const OPT_ADC_IQ: DemodField = DemodField::at(P0_06, 5, 4);
pub const P0_06_LOW_BITS: DemodField = DemodField::at(P0_06, 3, 0);
/// Page 0, 0x08: the ADC enables (Table 4). `AD_EN_reg` is ADC_I,
/// `AD_EN_reg1` ADC_Q; §9.1's IF mode sets only the latter.
const P0_08: DemodReg = demod(0, 0x08);
pub const AD_EN_REG: DemodField = DemodField::at(P0_08, 7, 7);
pub const AD_EN_REG1: DemodField = DemodField::at(P0_08, 6, 6);
pub const P0_08_LOW_BITS: DemodField = DemodField::at(P0_08, 5, 0);
/// Page 0, 0x0d (kernel: `REG_GPE`, `REG_MONSEL`, `REG_MON`); librtlsdr
/// writes it to stop the 4.096 MHz clock on TP_CK0.
const P0_0D: DemodReg = demod(0, 0x0d);
pub const REG_GPE: DemodField = DemodField::at(P0_0D, 7, 7);
pub const P0_0D_MIDDLE_BITS: DemodField = DemodField::at(P0_0D, 6, 3);
pub const REG_MONSEL: DemodField = DemodField::at(P0_0D, 2, 2);
pub const REG_MON: DemodField = DemodField::at(P0_0D, 1, 0);
/// Page 0, 0x19: the operating mode. In no table; librtlsdr's writes —
/// 0x05 "enable SDR mode, disable DAGC (bit 5)", 0x25 its DAGC on,
/// 0x03 its test mode — name the bits.
const P0_19: DemodReg = demod(0, 0x19);
pub const P0_19_HIGH_BITS: DemodField = DemodField::at(P0_19, 7, 6);
pub const DAGC_MODE: DemodField = DemodField::at(P0_19, 5, 5);
pub const P0_19_MIDDLE_BITS: DemodField = DemodField::at(P0_19, 4, 3);
/// The bit test mode clears.
pub const SDR_MODE: DemodField = DemodField::at(P0_19, 2, 2);
/// The ADC's 8-bit counter in place of samples.
pub const TEST_COUNTER: DemodField = DemodField::at(P0_19, 1, 1);
/// Set in every librtlsdr write.
pub const P0_19_BIT0: DemodField = DemodField::at(P0_19, 0, 0);
/// Page 0, 0x61: the MPEG PID filter (Table 13).
const P0_61: DemodReg = demod(0, 0x61);
pub const ENABLE_PID: DemodField = DemodField::at(P0_61, 7, 7);
pub const MODE_PID: DemodField = DemodField::at(P0_61, 6, 6);
pub const ERR_PASS: DemodField = DemodField::at(P0_61, 5, 5);
pub const P0_61_LOW_BITS: DemodField = DemodField::at(P0_61, 4, 0);

// Page 1.

/// Page 1, 0x01: the soft reset (kernel `SOFT_RST`; librtlsdr's comment
/// calls it "bit 3", its value is bit 2) and the I2C repeater (Table 3,
/// `IIC_repeat`). Bit 4 is set in every librtlsdr write; no source
/// names it.
const P1_01: DemodReg = demod(1, 0x01);
pub const P1_01_HIGH_BITS: DemodField = DemodField::at(P1_01, 7, 5);
pub const P1_01_BIT4: DemodField = DemodField::at(P1_01, 4, 4);
pub const IIC_REPEAT: DemodField = DemodField::at(P1_01, 3, 3);
pub const SOFT_RESET: DemodField = DemodField::at(P1_01, 2, 2);
pub const P1_01_LOW_BITS: DemodField = DemodField::at(P1_01, 1, 0);
/// Page 1, 0x04: the analog AGC loops (Table: `en_if_agc`, `en_rf_agc`,
/// `aagc_hold`; kernel `LOOP_GAIN2_3_0`).
const P1_04: DemodReg = demod(1, 0x04);
pub const EN_IF_AGC: DemodField = DemodField::at(P1_04, 7, 7);
pub const EN_RF_AGC: DemodField = DemodField::at(P1_04, 6, 6);
pub const AAGC_HOLD: DemodField = DemodField::at(P1_04, 5, 5);
pub const LOOP_GAIN2_3_0: DemodField = DemodField::at(P1_04, 4, 1);
pub const P1_04_BIT0: DemodField = DemodField::at(P1_04, 0, 0);
/// Page 1, 0x11: `en_dagc` (Table 10).
const P1_11: DemodReg = demod(1, 0x11);
pub const P1_11_HIGH_BITS: DemodField = DemodField::at(P1_11, 7, 1);
pub const EN_DAGC: DemodField = DemodField::at(P1_11, 0, 0);
/// Page 1, 0x15: `En_aci` and `Spec_inv` (Table 6).
const P1_15: DemodReg = demod(1, 0x15);
pub const P1_15_HIGH_BITS: DemodField = DemodField::at(P1_15, 7, 2);
pub const EN_ACI: DemodField = DemodField::at(P1_15, 1, 1);
pub const SPEC_INV: DemodField = DemodField::at(P1_15, 0, 0);
/// Page 1, 0x16-0x18: in no table; librtlsdr's "DDC shift" registers,
/// cleared at init (0x16-0x17 also by one two-byte write, its "adjust
/// channel rejection").
pub const DDC_SHIFT: DemodReg = demod(1, 0x16);
pub const DDC_SHIFT_LEN: u16 = 3;
/// Page 1, 0x19-0x1b: `pset_iffreq[21:0]` (Table 6), MSB first — the
/// IF as a 22-bit two's-complement fraction of the crystal (§9.3:
/// floor(f_IF / f_crystal × 2^22), negated).
pub const PSET_IFFREQ: DemodReg = demod(1, 0x19);
pub const PSET_IFFREQ_FRACTION_BITS: u32 = 22;
/// Its first octet carries the field's top six bits.
pub const PSET_IFFREQ_TOP_BITS: u16 = 0x3f;
/// Page 1, 0x1c-0x2f: the baseband FIR's 20 coefficient octets — 8 × i8
/// then 8 × i12 packed (librtlsdr; in no table).
pub const FIR_COEFFICIENTS: DemodReg = demod(1, 0x1c);
/// Page 1, 0x3e-0x3f: librtlsdr's sample frequency correction (its
/// ppm), high then low (in no table).
pub const SAMPLE_FREQ_CORRECTION_HIGH: DemodReg = demod(1, 0x3e);
pub const SAMPLE_FREQ_CORRECTION_LOW: DemodReg = demod(1, 0x3f);
/// Page 1, 0x93-0x94: librtlsdr's "FSM state-holding register" (in no
/// table), written 0xf0 then 0x0f at init.
pub const FSM_STATE_HOLD_HIGH: DemodReg = demod(1, 0x93);
pub const FSM_STATE_HOLD_LOW: DemodReg = demod(1, 0x94);
pub const FSM_STATE_HOLD: [u8; 2] = [0xf0, 0x0f];
/// Page 1, 0x9f-0xa2: `rsamp_ratio[27:2]` (Table 7), MSB first — the
/// crystal over the sample rate in 2^22 steps (§9.4).
pub const RSAMP_RATIO: DemodReg = demod(1, 0x9f);
pub const RSAMP_RATIO_LEN: u16 = 4;
pub const RSAMP_RATIO_FRACTION_BITS: u32 = 22;
/// The field's bits [27:2] of the 32-bit span.
pub const RSAMP_RATIO_FIELD: u32 = 0x0fff_fffc;
/// Bit 27, which librtlsdr also sets at bit 28 to form the "real ratio"
/// the chip divides by.
pub const RSAMP_RATIO_TOP_BIT: u32 = 1 << 27;
/// Page 1, 0xb1: the Zero-IF input and its DC / IQ correction (Tables
/// 4 and 5).
const P1_B1: DemodReg = demod(1, 0xb1);
pub const P1_B1_HIGH_BITS: DemodField = DemodField::at(P1_B1, 7, 5);
pub const EN_IQ_EST: DemodField = DemodField::at(P1_B1, 4, 4);
pub const EN_IQ_COMP: DemodField = DemodField::at(P1_B1, 3, 3);
pub const P1_B1_BIT2: DemodField = DemodField::at(P1_B1, 2, 2);
pub const EN_DC_EST: DemodField = DemodField::at(P1_B1, 1, 1);
pub const EN_BBIN: DemodField = DemodField::at(P1_B1, 0, 0);
