//! The RTL2832U's vendor control protocol and the register addresses
//! this driver uses, each cited from the document it comes from or from
//! the bench step that established it (`derivation/documents.md`,
//! `derivation/findings.md`). Page numbers are the documents' printed
//! numbers ([RTL2832U] printed page = PDF page - 6). The request
//! encodings are pure functions so the forms the bench proved can be
//! tested without a device.

// ---- Vendor control protocol ([RTL2832U] Tables 27-28, pp. 36-37) ----

/// bmRequestType for a Get: vendor, device, IN (Table 27). nusb builds
/// the byte from `ControlType::Vendor`, `Recipient::Device` and the
/// transfer's direction; the constant is the document's value.
pub const REQTYPE_GET: u8 = 0xC0;
/// bmRequestType for a Set: vendor, device, OUT (Table 27).
pub const REQTYPE_SET: u8 = 0x40;
/// bRequest is printed "x" (don't care) for every command (Table 27).
pub const BREQUEST: u8 = 0x00;

/// wIndex[11:8], the block (Table 28).
pub const BLOCK_DEMOD: u16 = 0;
pub const BLOCK_USB: u16 = 1;
pub const BLOCK_SYS: u16 = 2;
pub const BLOCK_TUNER: u16 = 3;
/// wIndex[7:4], the access (Table 28).
pub const ACCESS_GET: u16 = 0;
pub const ACCESS_SET: u16 = 1;

/// wIndex = block << 8 | access << 4 | page (Table 28; the page is for
/// the demodulator block only, 0-4).
pub const fn windex(block: u16, access: u16, page: u16) -> u16 {
    (block << 8) | (access << 4) | page
}

/// The block a wIndex names: the firmware's dispatcher takes wIndex's
/// high byte as the block (findings 5c, 0x06F7).
pub const fn block_of(index: u16) -> u16 {
    index >> 8
}

/// The USB and system blocks' wValue bases (Table 27: wValue =
/// (base << 8) + offset, printed as the bases 0x2000 and 0x3000).
pub const USB_BASE: u16 = 0x2000;
pub const SYS_BASE: u16 = 0x3000;

/// wValue for a system register: base + offset. Findings step 1A: of the
/// five candidate forms (base+offset, offset, offset<<8, offset<<8|base>>8,
/// swap(base+offset)) only base+offset reads the documented power-on
/// values (5 of 6 8-bit registers, 8 of 14 in all after a power cycle;
/// every other form at most 1), and 32-bit registers are little-endian.
pub const fn wvalue_sys(offset: u16) -> u16 {
    SYS_BASE + offset
}

/// wValue for a USB-block register, the same form (findings step 1A).
pub const fn wvalue_usb(offset: u16) -> u16 {
    USB_BASE + offset
}

/// The byte the demodulator's register offset is paired with in wValue.
/// Table 27 prints only "Reg's offset in page0~4"; findings step 1B/1C:
/// of 512 forms, offset << 8 | 0x20 (tied with | 0x21) reads 58 of the 59
/// non-zero documented reset bytes once the demodulator PLL is enabled,
/// every other form at most 3. 0x20 and 0x21 are the 7-bit I2C address
/// 0x10 of the demodulator's own slave ([RTL2832U] Fig. 5, p. 15) with
/// the low bit either way; 0x20 is the write placement Table 27 uses for
/// the tuner's address, and the form step 1D's writes verified by
/// read-back.
pub const DEMOD_I2C_WRITE: u8 = 0x20;

/// wValue for a demodulator register: offset << 8 | 0x20 (step 1B/1C).
pub const fn wvalue_demod(offset: u8) -> u16 {
    ((offset as u16) << 8) | DEMOD_I2C_WRITE as u16
}

/// The tuner's I2C address: 7-bit 0x1A, write 0x34, read 0x35 ([R820T]
/// Table 3-1, p. 15). Findings step 2: an address sweep through the tuner
/// block answered at 7-bit 0x1A with R0 = 0x96, and the low wValue bit is
/// ignored (the direction comes from bmRequestType).
pub const TUNER_I2C_WRITE: u8 = 0x34;
pub const TUNER_I2C_READ: u8 = 0x35;

/// wValue for a tuner register write: (register << 8) + I2C address
/// (Table 27's form; findings step 2: wValue (19 << 8) | 0x34 with one
/// data byte is accepted, while Table 28's note — the register in the
/// data stage, wValue 0 — stalls).
pub const fn wvalue_tuner_write(reg: u8) -> u16 {
    ((reg as u16) << 8) | TUNER_I2C_WRITE as u16
}

/// wValue for a tuner read: 0x0034. A read starts from register 0 and
/// continues for wLength bytes with no address phase ([R820T] p. 17,
/// Fig. 3-4); findings step 2: 0x0034 answers (`69 01 D3 FF 07`), while
/// 0x3400, 0x3500 and 0x0000 stall in every state.
pub const fn wvalue_tuner_read() -> u16 {
    TUNER_I2C_WRITE as u16
}

/// Tuner read data arrives LSB first ("The data is transmitted from LSB
/// to MSB", [R820T] p. 17, Fig. 3-4 labels D0..D7), so every byte read
/// is bit-reversed; written bytes are not.
pub const fn tuner_read_byte(raw: u8) -> u8 {
    raw.reverse_bits()
}

/// R0 = 0x96, "fixed as reference check point for read mode" ([R820T]
/// p. 17); read raw it arrives as 0x69 (findings step 2).
pub const TUNER_R0_CHECK: u8 = 0x96;

// ---- System block ([RTL2832U] Tables 14-21, pp. 27-32) ----

/// DEMOD_CTL, offset 0x0000 (Table 15): bit 7 demodulator PLL enable,
/// bit 6 ADC_I enable, bit 5 demodulator reset (0 = held in reset,
/// 1 = released; reset value 1), bit 3 ADC_Q enable. Findings step 2:
/// the PLL bit is one of the two things tuner access needs. Findings
/// step 3 (block scan): bit 2 never reads back, so a read-back compares
/// only the bits written.
pub const DEMOD_CTL: u16 = 0x0000;
pub const DEMOD_CTL_PLL_EN: u8 = 1 << 7;
pub const DEMOD_CTL_ADC_I_EN: u8 = 1 << 6;
pub const DEMOD_CTL_RESET_RELEASE: u8 = 1 << 5;
pub const DEMOD_CTL_ADC_Q_EN: u8 = 1 << 3;

/// GPIO (Tables 16-21): GPO 0x3001 output value, GPOE 0x3003 output
/// enable, GPD 0x3004 direction (1 = input).
pub const GPO: u16 = 0x0001;
pub const GPOE: u16 = 0x0003;
pub const GPD: u16 = 0x0004;
/// GPIO3 doubles as the tuner's power output, "default value 1 to turn
/// off power" (p. 29). Findings step 2's ablation: not needed for tuner
/// access on the bench board (the GPIO does not switch the tuner's
/// supply there, findings step 4a); written so the state is the
/// documented one.
pub const GPIO_TUNER_POWER: u8 = 1 << 3;
/// GPIO4 is documented as antenna power, "default 0 = off" (pp. 27-32).
/// Not driven: the printed GPO reset 0x18 has this bit at 1 (and the
/// bench read GPO 0x18 at power-on, findings step 1A), which contradicts
/// the stated default, and nothing on the bench measured the pin. See
/// the bias-tee refusal in the crate root.
pub const GPIO_ANTENNA_POWER: u8 = 1 << 4;

// ---- USB block ([RTL2832U] Tables 29-34, pp. 38-40) ----

/// USB_SYSCTL (Table 29-30, p. 38; documents.md): bit 10 SIE reset, bit 3
/// full-packet mode ("send only max-size packets; NAK while the FIFO holds
/// less"), bit 0 DMA enable (reset 1). The vendor driver writes 0x09 at
/// its USB set-up (findings 9b), and its stream's packets were all 512
/// bytes (findings 9h); the bench's minimised sequence leaves it at 0x01.
pub const USB_SYSCTL: u16 = 0x0000;
pub const SYSCTL_DMA_ENABLE: u8 = 1 << 0;
pub const SYSCTL_FULL_PACKET: u8 = 1 << 3;

/// USB_EPA_FIFO_CFG (Table 34, p. 40), offset in the USB block; its
/// bits 31:24, BLK_DROP_COUNTER, "Counts blocks dropped (each is 188
/// bytes) due to full FIFO" — its fourth byte (32-bit registers read
/// little-endian, findings step 1A).
pub const USB_EPA_FIFO_CFG: u16 = 0x0160;
/// USB_EPA_CTL (Table 32, p. 40; documents.md): 16-bit, bit 9 "FIFO
/// reset", bit 5 flush, bit 4 stall, bit 0 FIFO valid. The vendor driver
/// writes 0x0210 then 0 at its USB set-up (findings 9b).
pub const USB_EPA_CTL: u16 = 0x0148;
pub const EPA_CTL_FIFO_RESET: u16 = 1 << 9;
pub const BLK_DROP_COUNTER: u16 = USB_EPA_FIFO_CFG + 3;
/// The counter stops here (findings 16: 255 after every stall).
pub const BLK_DROP_COUNTER_SATURATED: u8 = 0xFF;
/// The bytes in one counted block.
pub const BLK_DROP_BLOCK_BYTES: usize = 188;
/// USB block offset 0x1C, the test-mode register: a write there wedges
/// the chip (derivation/HAZARD-usb-0x1C.md). The request layer never
/// writes it.
pub const USB_HAZARD_OFF: u16 = 0x001C;

// ---- Demodulator (Tables 2-13, pp. 12-26) ----

/// IIC_repeat: page 1, 0x01, bit 3 (Table 3, p. 15). The note under
/// Table 27 says "the 7th bit of the first byte"; findings step 2: bit 3
/// opens the tuner path, bit 7 alone and bit 6 alone do not. Table 3 is
/// right. Not cleared by a STOP. The driver opens it around each tuner
/// access and closes it after (`usb::Link::with_tuner`), as the vendor
/// driver was observed to (findings 9a: page 1 0x01 = 0x18 / 0x10 around
/// each try) and as the bench's stream tests did, so the register rests
/// at 0x10 where the pause bit was measured (findings 10.2c).
pub const IIC_REPEAT_PAGE: u8 = 1;
pub const IIC_REPEAT_OFF: u8 = 0x01;
pub const IIC_REPEAT_BIT: u8 = 1 << 3;

/// AD_EN_reg (ADC_I) page 0, 0x08 bit 7; AD_EN_reg1 (ADC_Q) bit 6
/// (Table 4, p. 17). Low-IF input (9.1, p. 17): only AD_EN_reg1 = 1.
/// Named for the record; this driver writes neither these bits nor
/// en_bbin below: the raw-sample stream ran with both at their power-on
/// values (findings 9c, 9g, 10), and no bench run streamed with the
/// documented input-mode values.
pub const AD_EN_PAGE: u8 = 0;
pub const AD_EN_OFF: u8 = 0x08;
pub const AD_EN_REG: u8 = 1 << 7;
pub const AD_EN_REG1: u8 = 1 << 6;

/// en_bbin page 1, 0xB1 bit 0 (Table 4): 0 for a low-IF input (9.1).
pub const BBIN_PAGE: u8 = 1;
pub const BBIN_OFF: u8 = 0xB1;
pub const EN_BBIN: u8 = 1 << 0;

/// pset_iffreq page 1 {0x19, 0x1A, 0x1B} [21:0], 22-bit two's
/// complement, MSB first (Table 6, p. 20; 9.3, p. 19).
pub const IFFREQ_PAGE: u8 = 1;
pub const IFFREQ_MSB: u8 = 0x19;
pub const IFFREQ_BITS: u32 = 22;
/// The bits of the MSB byte the field occupies ([21:16]).
pub const IFFREQ_MSB_MASK: u8 = 0x3F;

/// Spec_inv page 1, 0x15 bit 0 (Table 6): "with a normal high-side-mixing
/// tuner the demodulator requires an inverse spectrum" (9.3).
pub const SPEC_INV_PAGE: u8 = 1;
pub const SPEC_INV_OFF: u8 = 0x15;
pub const SPEC_INV: u8 = 1 << 0;

/// rsamp_ratio page 1 {0x9F..0xA2} [27:2] (Table 7, p. 20), 26 bits, MSB
/// first; byte 0x9F's upper nibble belongs to cfreq_off_ratio
/// {0x9D..0x9F} [23:4] (Table 12) and is kept.
pub const RSAMP_PAGE: u8 = 1;
pub const RSAMP_MSB: u8 = 0x9F;
pub const RSAMP_SHIFT: u32 = 2;
pub const RSAMP_BITS: u32 = 26;
/// The write masks of the four bytes, MSB first: 0x9F [3:0], 0xA0, 0xA1,
/// 0xA2 [7:2].
pub const RSAMP_MASKS: [u8; 4] = [0x0F, 0xFF, 0xFF, 0xFC];

/// The analog AGC loops, page 1 0x04 (Table 2, p. 12): en_if_agc bit 7,
/// en_rf_agc bit 6, aagc_hold bit 5.
pub const AAGC_PAGE: u8 = 1;
pub const AAGC_OFF: u8 = 0x04;
pub const EN_IF_AGC: u8 = 1 << 7;
pub const EN_RF_AGC: u8 = 1 << 6;
pub const AAGC_HOLD: u8 = 1 << 5;

/// en_dagc page 1, 0x11 bit 0 (Table 10, p. 22).
pub const DAGC_PAGE: u8 = 1;
pub const DAGC_OFF: u8 = 0x11;
pub const EN_DAGC: u8 = 1 << 0;

/// polar_rf_agc / polar_if_agc page 0, 0x0E bits 1 / 0 (Table 2, p. 12):
/// "Inverse the AGC_RF / AGC_IF Sigma-Delta Pin". Findings step 4b: with
/// polar_if_agc = 0 the IF loop closes through the tuner's VAGC pin and
/// settles; with 1 it is positive feedback.
pub const AGC_POLARITY_PAGE: u8 = 0;
pub const AGC_POLARITY_OFF: u8 = 0x0E;
pub const POLAR_RF_AGC: u8 = 1 << 1;
pub const POLAR_IF_AGC: u8 = 1 << 0;

/// 2^22, the scale of the resampler and IF formulas (9.3, 9.4).
pub const TWO_POW_22: f64 = 4_194_304.0;

// ---- The raw-sample stream (findings steps 9 and 10) ----
//
// None of these registers is named by [RTL2832U] beyond what is cited;
// their roles are what the bench measured (findings 10.4), nothing more.

/// DEMOD_CTL as the stream's first write: the PLL, both ADCs and the
/// reset released in one write, 0xE8 (Table 15's bits; findings 10.4:
/// "DEMOD_CTL ← 0xE8 as one write equals 0xA8 then 0xE8"; without the
/// ADC bits every byte is 0x80).
pub const DEMOD_CTL_STREAM: u8 = DEMOD_CTL_PLL_EN | DEMOD_CTL_ADC_I_EN | DEMOD_CTL_RESET_RELEASE | DEMOD_CTL_ADC_Q_EN;
/// DEMOD_CTL's bit 2 never reads back (findings step 3's block scan):
/// a read-back compares every other bit.
pub const DEMOD_CTL_NOT_READ_BACK: u8 = 1 << 2;

/// Page 1 0x16-0x18: the power-on 3F CE CC is a frequency offset (with
/// 0x16-0x17 left at power-on the tone moved +87.5 kHz, with 0x18 left
/// it moved 2 bins; findings 10.4); the stream writes 0 to all three, as
/// 0x16-0x17 in one transfer and 0x18 in another (findings 9b items 8-9).
pub const FREQ_OFFSET_PAGE: u8 = 1;
pub const FREQ_OFFSET_FIRST: u8 = 0x16;
pub const FREQ_OFFSET_LAST: u8 = 0x18;
pub const FREQ_OFFSET_CLEARED: u8 = 0x00;

/// Page 1 0x15 as the stream writes it: Spec_inv = 1, En_aci = 0, every
/// other bit 0 (Table 6; findings 9b item 11; without it the tone is
/// mirrored, findings 10.4).
pub const SPEC_INV_STREAM: u8 = SPEC_INV;

/// The 20 tap registers, page 1 0x1C-0x2F (findings 10.3; named by no
/// document). Demodulator page 1 — not the USB block, whose offset 0x1C
/// is the test-mode hazard and is never written by this driver.
pub const TAP_PAGE: u8 = 1;
pub const TAP_FIRST: u8 = 0x1C;

/// Page 0 0x19: bit 0 is the stream's enable (0x24: 0 bytes), bit 2
/// changes the content (0x21: near-constant output); the stream writes
/// 0x25, the observed value (findings 9b item 16; 10.4; power-on 0x20).
pub const STREAM_ENABLE_PAGE: u8 = 0;
pub const STREAM_ENABLE_OFF: u8 = 0x19;
pub const STREAM_ENABLE_VALUE: u8 = 0x25;

/// Page 1 0x93 ← 0xF0 (or 0x94 ← 0x0F, either alone) keeps the rate
/// steady and the level full; without both the rate wandered 3.35-4.13
/// MB/s and the level dropped (findings 10.4, "rate steadiness").
pub const RATE_STEADY_PAGE: u8 = 1;
pub const RATE_STEADY_OFF: u8 = 0x93;
pub const RATE_STEADY_VALUE: u8 = 0xF0;

/// Page 0 0x06 bit 7 starts the stream; cleared it stops it, set again
/// it restarts it within one 16 KiB transfer, ≤ 3.1 ms, with no power
/// cycle (findings 9b item 22; 10.1; 10.4). The start writes the whole
/// byte 0x80 (opt_adc_iq [5:4] = 0, Table 4: I and Q not swapped), as
/// observed and benched; the stop clears bit 7 alone.
pub const START_PAGE: u8 = 0;
pub const START_OFF: u8 = 0x06;
pub const START_BIT: u8 = 1 << 7;
pub const START_VALUE: u8 = START_BIT;

/// Page 1 0x01 bit 2 pauses the stream: 0x10 → 0x14 gave 0 bytes in
/// 10 × 10 ms, back to 0x10 resumed it (findings 10.2c; 9d). The same
/// register holds IIC_repeat (bit 3); 0x10 is its power-on value with
/// the repeater closed (findings step 2: 0x10 → 0x18 opens it).
pub const PAUSE_PAGE: u8 = 1;
pub const PAUSE_OFF: u8 = 0x01;
pub const PAUSE_BIT: u8 = 1 << 2;
/// Page 1 0x01 at power-on (findings step 2), the state pause and
/// resume were measured from.
pub const PAGE1_0X01_POWER_ON: u8 = 0x10;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// Table 28's wIndex: the tuner block's Set is 0x0310, the
    /// demodulator's Get of page 3 is 0x0003.
    #[test]
    fn windex_packs_block_access_page() {
        assert_eq!(windex(BLOCK_TUNER, ACCESS_SET, 0), 0x0310);
        assert_eq!(windex(BLOCK_DEMOD, ACCESS_GET, 3), 0x0003);
        assert_eq!(windex(BLOCK_SYS, ACCESS_GET, 0), 0x0200);
        assert_eq!(windex(BLOCK_USB, ACCESS_SET, 0), 0x0110);
        for block in [BLOCK_DEMOD, BLOCK_USB, BLOCK_SYS, BLOCK_TUNER] {
            assert_eq!(block_of(windex(block, ACCESS_SET, 4)), block);
        }
    }

    /// Findings step 1A: the system and USB blocks take base + offset.
    #[test]
    fn system_and_usb_wvalues_are_base_plus_offset() {
        assert_eq!(wvalue_sys(DEMOD_CTL), 0x3000);
        assert_eq!(wvalue_sys(GPO), 0x3001);
        assert_eq!(wvalue_sys(0x0048), 0x3048);
        assert_eq!(wvalue_usb(0x0000), 0x2000);
        assert_eq!(wvalue_usb(0x0144), 0x2144);
    }

    /// Findings step 1B/1C: the demodulator takes offset << 8 | 0x20.
    #[test]
    fn demodulator_wvalue_is_offset_high_address_low() {
        assert_eq!(wvalue_demod(0x9F), 0x9F20);
        assert_eq!(wvalue_demod(0x01), 0x0120);
        assert_eq!(wvalue_demod(0x00), 0x0020);
    }

    /// Findings step 2: writes at (register << 8) | 0x34, reads at 0x0034.
    #[test]
    fn tuner_wvalues_follow_table_27_not_table_28s_note() {
        assert_eq!(wvalue_tuner_write(19), 0x1334);
        assert_eq!(wvalue_tuner_write(0), 0x0034);
        assert_eq!(wvalue_tuner_read(), 0x0034);
        assert_eq!(TUNER_I2C_READ, TUNER_I2C_WRITE | 1);
    }

    /// [R820T] p. 17: the check byte 0x96 arrives as 0x69 and reverses
    /// back; reversal is its own inverse.
    #[test]
    fn tuner_read_bytes_are_bit_reversed() {
        assert_eq!(tuner_read_byte(0x69), TUNER_R0_CHECK);
        assert_eq!(tuner_read_byte(0x96), 0x69);
        for raw in 0..=255u8 {
            assert_eq!(tuner_read_byte(tuner_read_byte(raw)), raw);
        }
        // Findings step 2's first tuner read, raw: 69 01 D3 FF 07.
        let raw = [0x69u8, 0x01, 0xD3, 0xFF, 0x07];
        let regs: Vec<u8> = raw.iter().map(|&b| tuner_read_byte(b)).collect();
        assert_eq!(regs[0], 0x96);
        assert_eq!(regs, [0x96, 0x80, 0xCB, 0xFF, 0xE0]);
    }

    /// The stream's register values as findings 10.4 lists them.
    #[test]
    fn stream_values_are_the_bench_sequences() {
        assert_eq!(DEMOD_CTL_STREAM, 0xE8);
        assert_eq!(DEMOD_CTL_STREAM & DEMOD_CTL_NOT_READ_BACK, 0, "the bit that never reads back is written 0");
        assert_eq!(SPEC_INV_STREAM, 0x01);
        assert_eq!((STREAM_ENABLE_PAGE, STREAM_ENABLE_OFF, STREAM_ENABLE_VALUE), (0, 0x19, 0x25));
        assert_eq!((RATE_STEADY_PAGE, RATE_STEADY_OFF, RATE_STEADY_VALUE), (1, 0x93, 0xF0));
        assert_eq!((START_PAGE, START_OFF, START_VALUE), (0, 0x06, 0x80));
        assert_eq!((PAUSE_PAGE, PAUSE_OFF, PAGE1_0X01_POWER_ON | PAUSE_BIT), (1, 0x01, 0x14));
        assert_eq!(PAGE1_0X01_POWER_ON & IIC_REPEAT_BIT, 0, "the repeater is closed at power-on");
        assert_eq!((PAUSE_PAGE, PAUSE_OFF), (IIC_REPEAT_PAGE, IIC_REPEAT_OFF), "one register, two roles");
        assert_eq!(FREQ_OFFSET_LAST - FREQ_OFFSET_FIRST + 1, 3);
    }

    #[test]
    fn request_type_bytes_are_the_documents() {
        assert_eq!(REQTYPE_GET, 0xC0);
        assert_eq!(REQTYPE_SET, 0x40);
        assert_eq!(REQTYPE_GET & 0x80, 0x80, "a Get is device-to-host");
        assert_eq!(BREQUEST, 0);
    }
}
