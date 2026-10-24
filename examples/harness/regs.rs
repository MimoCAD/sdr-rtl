//! Every register address, field and reset value the harness uses, each
//! cited from the vendor document it comes from. Page numbers are the
//! documents' printed numbers ([RTL2832U] printed page = PDF page - 6).
//! Nothing here comes from any other source; values the documents do
//! not give are found by the searches in the step modules.

#![allow(dead_code)]

// ---- Vendor control protocol ([RTL2832U] Tables 27-28, pp. 36-37) ----

/// bmRequestType for Get commands (vendor, device, IN).
pub const REQTYPE_GET: u8 = 0xC0;
/// bmRequestType for Set commands (vendor, device, OUT).
pub const REQTYPE_SET: u8 = 0x40;
/// bRequest is printed "x" (don't care) for every command.
pub const BREQUEST: u8 = 0x00;

/// wIndex nibble [2]: the block ([RTL2832U] Table 28).
pub const BLOCK_DEMOD: u16 = 0;
pub const BLOCK_USB: u16 = 1;
pub const BLOCK_SYS: u16 = 2;
pub const BLOCK_TUNER: u16 = 3;
pub const BLOCK_ROM: u16 = 4;
pub const BLOCK_IR: u16 = 5;
pub const BLOCK_STD_I2C: u16 = 6;
/// wIndex nibble [1]: access.
pub const ACCESS_GET: u16 = 0;
pub const ACCESS_SET: u16 = 1;

/// wIndex = block << 8 | access << 4 | page ([RTL2832U] Table 28).
pub const fn windex(block: u16, access: u16, page: u16) -> u16 {
    (block << 8) | (access << 4) | page
}

// ---- USB block ([RTL2832U] Tables 29-34, pp. 38-40; base 0x2000) ----

pub const USB_BASE: u16 = 0x2000;
/// USB_SYSCTL, offset 0x0000 (Table 30).
pub const USB_SYSCTL: u16 = 0x0000;
pub const USB_SYSCTL_SIE_RESET: u32 = 1 << 10;
pub const USB_SYSCTL_FULL_PACKET: u32 = 1 << 3;
pub const USB_SYSCTL_DMA_EN: u32 = 1 << 0;
/// Reset value of USB_SYSCTL's documented bits: DMA = 1, others 0.
pub const USB_SYSCTL_RESET: u32 = USB_SYSCTL_DMA_EN;
pub const USB_SYSCTL_DOC_MASK: u32 =
    USB_SYSCTL_SIE_RESET | USB_SYSCTL_FULL_PACKET | USB_SYSCTL_DMA_EN;

/// USB_EPA_CFG, offset 0x0144 (Table 31).
pub const USB_EPA_CFG: u16 = 0x0144;
pub const EPA_CFG_ISO_MASK: u32 = 0b11 << 8;
pub const EPA_CFG_ENABLE: u32 = 1 << 7;
pub const EPA_CFG_TYPE_SHIFT: u32 = 5;
pub const EPA_CFG_TYPE_MASK: u32 = 0b11 << EPA_CFG_TYPE_SHIFT;
/// Endpoint transfer type 10b = Bulk (also the reset value).
pub const EPA_CFG_TYPE_BULK: u32 = 0b10;
pub const EPA_CFG_DIR_IN: u32 = 1 << 4;
pub const EPA_CFG_NUM_MASK: u32 = 0x0F;
/// Documented reset: enable 0, type 10b, iso 00, number 0 (bit 4 R, no reset).
pub const EPA_CFG_RESET: u32 = EPA_CFG_TYPE_BULK << EPA_CFG_TYPE_SHIFT;
pub const EPA_CFG_RESET_MASK: u32 =
    EPA_CFG_ISO_MASK | EPA_CFG_ENABLE | EPA_CFG_TYPE_MASK | EPA_CFG_NUM_MASK;

/// USB_EPA_CTL, offset 0x0148 (Table 32).
pub const USB_EPA_CTL: u16 = 0x0148;
pub const EPA_CTL_FIFO_RESET: u32 = 1 << 9;
pub const EPA_CTL_FLUSH: u32 = 1 << 5;
pub const EPA_CTL_STALL: u32 = 1 << 4;
pub const EPA_CTL_VALID: u32 = 1 << 0;
pub const EPA_CTL_RESET_MASK: u32 = EPA_CTL_FIFO_RESET | EPA_CTL_STALL;

/// USB_EPA_MAXPKT, offset 0x0158 (Table 33): bits 10:0, reset 0x40.
pub const USB_EPA_MAXPKT: u16 = 0x0158;
pub const EPA_MAXPKT_MASK: u32 = 0x7FF;
pub const EPA_MAXPKT_RESET: u32 = 0x40;

/// USB_EPA_FIFO_CFG, offset 0x0160 (Table 34): 3:0 size, reset 0xA.
pub const USB_EPA_FIFO_CFG: u16 = 0x0160;
pub const EPA_FIFO_SIZE_MASK: u32 = 0x0F;
pub const EPA_FIFO_SIZE_RESET: u32 = 0x0A;
pub const EPA_FIFO_DROP_SHIFT: u32 = 24;

// ---- System block ([RTL2832U] Tables 14-26, pp. 27-36; base 0x3000) ----

pub const SYS_BASE: u16 = 0x3000;
/// DEMOD_CTL, offset 0x0000 (Table 15).
pub const DEMOD_CTL: u16 = 0x0000;
pub const DEMOD_CTL_PLL_EN: u8 = 1 << 7;
pub const DEMOD_CTL_ADC_I_EN: u8 = 1 << 6;
/// 0 = hardware reset active, 1 = released (reset value 1).
pub const DEMOD_CTL_RESET_RELEASE: u8 = 1 << 5;
pub const DEMOD_CTL_ADC_Q_EN: u8 = 1 << 3;
pub const DEMOD_CTL_DOC_MASK: u8 =
    DEMOD_CTL_PLL_EN | DEMOD_CTL_ADC_I_EN | DEMOD_CTL_RESET_RELEASE | DEMOD_CTL_ADC_Q_EN;
pub const DEMOD_CTL_RESET: u8 = DEMOD_CTL_RESET_RELEASE;

/// GPIO registers (Tables 16-21).
pub const GPO: u16 = 0x0001;
pub const GPI: u16 = 0x0002;
pub const GPOE: u16 = 0x0003;
pub const GPD: u16 = 0x0004;
pub const GP_CFG0: u16 = 0x0007;
pub const GP_CFG1: u16 = 0x0008;
pub const DEMOD_CTL_1: u16 = 0x000B;
/// Reset values printed bit by bit in Tables 16, 18, 19, 20, 21.
pub const GPO_RESET: u8 = 0x18; // bits 4, 3
pub const GPOE_RESET: u8 = 0x19; // bits 4, 3, 0
pub const GPD_RESET: u8 = 0x0E; // bits 3, 2, 1 (1 = input)
pub const GP_CFG0_RESET: u8 = 0x96; // PAD3 2h, PAD2 1h, PAD1 1h, PAD0 2h
pub const GP_CFG1_RESET: u8 = 0x55; // PAD7..PAD4 1h
/// GPIO3: tuner power output, "Default value 1 to turn off power" (p. 29).
pub const GPIO_TUNER_POWER: u8 = 1 << 3;

/// I2C master (Tables 22-26).
pub const I2CCR: u16 = 0x0040;
pub const I2CMCR: u16 = 0x0044;
pub const I2CMSTR: u16 = 0x0048;
pub const I2CMSR: u16 = 0x004C;
pub const I2CMFR: u16 = 0x0050;
/// I2CCR FD10 [5:0], reset 0x13; "When powered on, software must write FD10".
pub const I2CCR_FD10_MASK: u32 = 0x3F;
pub const I2CCR_RESET: u32 = 0x13;
/// I2CMCR reset: TORE [24] = 1, TOR [23:16] = 0x3A, the rest 0.
pub const I2CMCR_RESET: u32 = (1 << 24) | (0x3A << 16);
pub const I2CMCR_DOC_MASK: u32 = 0xFFFF_07FF;
/// I2CMSTR reset: STA_SU_PC [27:20] = 9, FTPC [19:16] = 3, SHPC [15:8] = 9, SLPC [7:0] = 0x10.
pub const I2CMSTR_RESET: u32 = (9 << 20) | (3 << 16) | (9 << 8) | 0x10;
pub const I2CMSTR_DOC_MASK: u32 = 0x7FFF_FFFF;

// ---- Demodulator (Tables 2-13, pp. 12-26) ----

/// IIC_repeat: page 1, 0x01, bit 3 (Table 3, p. 15). Note2 under Table 27
/// says "the 7th bit of the first byte" instead.
pub const IIC_REPEAT_PAGE: u8 = 1;
pub const IIC_REPEAT_OFF: u8 = 0x01;
pub const IIC_REPEAT_BIT_TABLE3: u8 = 1 << 3;
pub const IIC_REPEAT_BIT_NOTE2: u8 = 1 << 7;
/// Note2's "7th bit" with bits counted from 1.
pub const IIC_REPEAT_BIT_NOTE2_FROM1: u8 = 1 << 6;

/// AD_EN_reg (ADC_I) page 0, 0x08[7]; AD_EN_reg1 (ADC_Q) 0x08[6] (Table 4).
pub const AD_EN_PAGE: u8 = 0;
pub const AD_EN_OFF: u8 = 0x08;
pub const AD_EN_REG: u8 = 1 << 7;
pub const AD_EN_REG1: u8 = 1 << 6;
/// en_bbin page 1, 0xB1[0], reset 1 (Table 4); en_dc_est [1], en_iq_comp [3],
/// en_iq_est [4], all reset 1 (Table 5).
pub const BBIN_PAGE: u8 = 1;
pub const BBIN_OFF: u8 = 0xB1;
pub const EN_BBIN: u8 = 1 << 0;
pub const BBIN_RESET_MASK: u8 = 0x1B;
pub const BBIN_RESET: u8 = 0x1B;

/// pset_iffreq page 1 {0x19..0x1B}[21:0], MSB first (Table 6).
pub const IFFREQ_PAGE: u8 = 1;
pub const IFFREQ_MSB: u8 = 0x19;
pub const IFFREQ_BITS: u32 = 22;

/// rsamp_ratio page 1 {0x9F..0xA2}[27:2], reset 0xC99999 (Table 7);
/// shares 0x9F with cfreq_off_ratio {0x9D..0x9F}[23:4], reset 0xAEBAF (Table 12).
pub const RSAMP_PAGE: u8 = 1;
pub const RSAMP_MSB: u8 = 0x9F;
pub const RSAMP_SHIFT: u32 = 2;
pub const RSAMP_BITS: u32 = 26;
pub const RSAMP_RESET: u32 = 0xC9_9999;
pub const CFREQ_MSB: u8 = 0x9D;
pub const CFREQ_SHIFT: u32 = 4;
pub const CFREQ_RESET: u32 = 0xA_EBAF;
/// The 28.8 MHz crystal ([RTL2832U] §9.4 examples, p. 10).
pub const XTAL_HZ: f64 = 28.8e6;
/// 2^22, the resampler and IF formulas' scale (§9.3, §9.4).
pub const TWO_POW_22: f64 = 4_194_304.0;

/// rsamp_ratio = floor(f_xtal / f_s * 2^22) (§9.4, p. 20).
pub fn rsamp_ratio(rate_hz: f64) -> u32 {
    (XTAL_HZ / rate_hz * TWO_POW_22).floor() as u32
}

/// One documented reset value: page (demodulator) or block, offset,
/// the bits the document fixes, and their value.
pub struct Default {
    pub name: &'static str,
    pub page: u8,
    pub offset: u8,
    pub mask: u8,
    pub value: u8,
    pub cite: &'static str,
}

const fn d(
    name: &'static str,
    page: u8,
    offset: u8,
    mask: u8,
    value: u8,
    cite: &'static str,
) -> Default {
    Default {
        name,
        page,
        offset,
        mask,
        value,
        cite,
    }
}

/// The demodulator reset values the documents print, as bytes. Multi-byte
/// fields are split MSB first ({MSB, LSB} in every table except Table 2,
/// whose multi-byte fields are read-only and not used here).
pub const DEMOD_DEFAULTS: &[Default] = &[
    // Table 2 / p. 13 (AAGC), page 1.
    d(
        "en_if_agc,en_rf_agc,aagc_hold,loop_gain2<3:0>",
        1,
        0x04,
        0xFE,
        0xC0,
        "Table 2 p.12",
    ),
    d("loop_gain2<4>", 1, 0x05, 0x80, 0x80, "Table 2 p.12"),
    d("vtop1", 1, 0x06, 0x3F, 0x10, "Table 2 p.12"),
    d("krf2", 1, 0x07, 0xFF, 0x0F, "Table 2 p.12"),
    d("if_agc_min", 1, 0x08, 0xFF, 0x80, "Table 2 p.12"),
    d("if_agc_max", 1, 0x09, 0xFF, 0x7F, "Table 2 p.13"),
    d("rf_agc_min", 1, 0x0A, 0xFF, 0x80, "Table 2 p.13"),
    d("rf_agc_max", 1, 0x0B, 0xFF, 0x7F, "Table 2 p.13"),
    d("loop_gain1", 1, 0xC7, 0x3E, 0x0C << 1, "Table 2 p.12"),
    d("loop_gain3", 1, 0xC8, 0x1F, 0x1A, "Table 2 p.12"),
    d("vtop2", 1, 0xC9, 0x3F, 0x30, "Table 2 p.12"),
    d("vtop3", 1, 0xCA, 0x3F, 0x28, "Table 2 p.12"),
    d("krf1", 1, 0xCB, 0xFF, 0x02, "Table 2 p.12"),
    d("krf3", 1, 0xCD, 0xFF, 0x20, "Table 2 p.12"),
    d("krf4", 1, 0xCE, 0xFF, 0x30, "Table 2 p.12"),
    d(
        "polar_rf_agc,polar_if_agc",
        0,
        0x0E,
        0x03,
        0x00,
        "Table 2 p.12",
    ),
    // Table 3.
    d("IIC_repeat", 1, 0x01, 0x08, 0x00, "Table 3 p.15"),
    // Table 4.
    d("AD_EN_reg,AD_EN_reg1", 0, 0x08, 0xC0, 0x00, "Table 4 p.17"),
    d("opt_adc_iq", 0, 0x06, 0x30, 0x00, "Table 4 p.17"),
    // Tables 4-5.
    d(
        "en_bbin,en_dc_est,en_iq_comp,en_iq_est",
        1,
        0xB1,
        0x1B,
        0x1B,
        "Tables 4-5 pp.17-18",
    ),
    // Table 6.
    d("Spec_inv,En_aci", 1, 0x15, 0x03, 0x02, "Table 6 p.20"),
    // Table 7 + Table 12: cfreq_off_ratio 0xAEBAF << 4 over 0x9D..0x9F,
    // rsamp_ratio 0xC99999 << 2 over 0x9F..0xA2.
    d(
        "cfreq_off_ratio[19:12]",
        1,
        0x9D,
        0xFF,
        0xAE,
        "Table 12 p.24",
    ),
    d(
        "cfreq_off_ratio[11:4]",
        1,
        0x9E,
        0xFF,
        0xBA,
        "Table 12 p.24",
    ),
    d(
        "cfreq_off_ratio[3:0],rsamp_ratio[25:22]",
        1,
        0x9F,
        0xFF,
        0xF3,
        "Tables 7,12 pp.20,24",
    ),
    d("rsamp_ratio[21:14]", 1, 0xA0, 0xFF, 0x26, "Table 7 p.20"),
    d("rsamp_ratio[13:6]", 1, 0xA1, 0xFF, 0x66, "Table 7 p.20"),
    d("rsamp_ratio[5:0]", 1, 0xA2, 0xFC, 0x64, "Table 7 p.20"),
    // Table 8.
    d("En_cci", 1, 0x40, 0x02, 0x02, "Table 8 p.21"),
    // Table 9.
    d("en_inc", 1, 0x5D, 0x01, 0x01, "Table 9 p.21"),
    d("inc_det_cnt_rst", 1, 0x5E, 0x40, 0x00, "Table 9 p.21"),
    // Table 10.
    d("en_dagc", 1, 0x11, 0x01, 0x01, "Table 10 p.22"),
    // Table 11.
    d(
        "dis_auto_scan,pset_mode_gi",
        1,
        0x5F,
        0x0F,
        0x06,
        "Table 11 p.22",
    ),
    // Table 13, page 0.
    d(
        "err_pass,mode_PID,enable_PID",
        0,
        0x61,
        0xE0,
        0xE0,
        "Table 13 p.25",
    ),
    d("en_PID[7:0]", 0, 0x62, 0xFF, 0x00, "Table 13 p.25"),
    d("en_PID[15:8]", 0, 0x63, 0xFF, 0x00, "Table 13 p.25"),
    d("en_PID[23:16]", 0, 0x64, 0xFF, 0x00, "Table 13 p.25"),
    d("en_PID[31:24]", 0, 0x65, 0xFF, 0x00, "Table 13 p.25"),
];

/// PID0..PID31 reset values (Table 13, pp. 25-26): 13-bit fields at
/// page 0 {0x66 + 2n, 0x67 + 2n}, MSB first.
pub const PID_RESET: [u16; 32] = [
    0x00, 0x01, 0x06, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D,
    0x1E, 0x1F, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x2B, 0x2C, 0x2D,
];
pub const PID_FIRST_OFF: u8 = 0x66;
pub const PID_PAGE: u8 = 0;
pub const PID_MSB_MASK: u8 = 0x1F;

/// All demodulator oracle bytes: [`DEMOD_DEFAULTS`] plus the PID table.
pub fn demod_oracle() -> Vec<(String, u8, u8, u8, u8, &'static str)> {
    let mut v: Vec<_> = DEMOD_DEFAULTS
        .iter()
        .map(|x| {
            (
                x.name.to_string(),
                x.page,
                x.offset,
                x.mask,
                x.value,
                x.cite,
            )
        })
        .collect();
    for (n, pid) in PID_RESET.iter().enumerate() {
        let off = PID_FIRST_OFF + 2 * n as u8;
        v.push((
            format!("PID{n}[12:8]"),
            PID_PAGE,
            off,
            PID_MSB_MASK,
            (pid >> 8) as u8,
            "Table 13 pp.25-26",
        ));
        v.push((
            format!("PID{n}[7:0]"),
            PID_PAGE,
            off + 1,
            0xFF,
            *pid as u8,
            "Table 13 pp.25-26",
        ));
    }
    v
}

/// The configuration EEPROM: an M24C02 with E2..E0 grounded ([RTL2832U]
/// Fig. 9, p. 44), so 0xA0 write / 0xA1 read by the M24C02's own
/// addressing. Its contents carry the device's USB strings, serial
/// included, so the records never store bytes read from these addresses.
pub const EEPROM_I2C_WRITE: u8 = 0xA0;
pub const EEPROM_I2C_READ: u8 = 0xA1;

/// True when a wValue's low byte addresses the EEPROM.
pub fn eeprom_addressed(wvalue: u16) -> bool {
    let lo = wvalue as u8;
    lo == EEPROM_I2C_WRITE || lo == EEPROM_I2C_READ
}

// ---- R820T / R820T2 tuner ----

/// 7-bit 0x1A: write 0x34, read 0x35 ([R820T] Table 3-1, p. 15).
pub const TUNER_I2C_WRITE: u8 = 0x34;
pub const TUNER_I2C_READ: u8 = 0x35;
/// R0 = 0x96, "fixed as reference check point for read mode" ([R820T] p. 17);
/// read data is LSB first, so it arrives as 0x69 unreversed.
pub const TUNER_R0_CHECK: u8 = 0x96;
/// R19: all eight cells fixed 0 ([R820T2] Table 1-2, p. 5).
pub const TUNER_R19: u8 = 19;
pub const TUNER_R19_FIXED: u8 = 0x00;
