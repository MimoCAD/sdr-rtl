# What the vendor documents say

Read from the documents' page images (never from extracted text). Page
numbers are each document's printed numbers. Where a document
contradicts itself, both readings are given and the contradiction is
named; the bench decides.

The three documents:

- **[R820T]** Rafael Micro, "R820T High Performance Low Power Advanced
  Digital TV Silicon Tuner Datasheet", Preliminary Rev 1.2, 2011-11-30.
  26 pages. R820T only.
- **[R820T2]** Rafael Micro, "R820T2 Register Description", v1.3,
  2014-10-14 (cover © 2011, footers © 2012). 11 pages.
- **[RTL2832U]** Realtek, "RTL2832U DVB-T COFDM Demodulator + USB 2.0",
  Rev 1.4, 2010-11-01. 53 pages; printed page = PDF page − 6.

## What the documents do NOT give

- No tuner initial values for R5–R31, no power-on defaults ([R820T]
  has no register map at all; [R820T2] prints only fixed cells).
- No per-frequency front-end table (tracking filter, notch, low-pass,
  RF filter band, RF mux, open-drain).
- No dB per LNA or mixer gain code; no Hz per IF filter code.
- No VCO frequency range, no divider-selection rule, no VCO current or
  charge-pump value, no PLL lock procedure; R2 `VCO_INDICATOR` and R3
  `RF_INDICATOR` are named and never described.
- No procedures of any kind (calibration, AGC, standby).
- No RTL2832U SDR / raw-IQ mode, no channel-filter (FIR) registers, no
  initialization sequence.
- Nothing on the R828D.

Each of these is derived on the bench (see [`README.md`](README.md)).

## R820T / R820T2 tuner

### I²C ([R820T] §3, pp. 15–17; [R820T2] pp. 2–4)

- 7-bit address 0x1A: write 0x34, read 0x35 ([R820T] Table 3-1).
- 100 kHz standard, up to 400 kHz.
- Write: S, 0x34, A, register address, A, data…, P. The address byte is
  `000` + a 5-bit register address; data auto-increments (page write).
  Write data is labelled D7…D0: MSB first.
- Read: S, 0x35, A, then data starting **from register 0**, continuing
  until P. No address phase.
- **Read data is LSB first**: "The data is transmitted from LSB to MSB"
  ([R820T] p. 17, Fig. 3-4 labels D0…D7). Each byte read must be
  bit-reversed; written bytes are not.
- "The data of register 0 (0x96) is fixed as reference check point for
  read mode." Read raw (MSB-first), the check value arrives as 0x69.
- "Register 0 to Register 4 are reserved for internal use only and can
  be written by I2C write command" — but [R820T2] Table 1-2 marks R0–R4
  read-only. Treated as read-only.

### PLL ([R820T2] pp. 9–10)

- `Nint = 4·Ni2c + Si2c + 13` (R20: `SI2C[7:6]`, `NI2C[5:0]`; the matrix
  prints `N_I2C[4:0]` across six cells — six bits taken).
- `Nfra = SDM_IN[16]·2⁻¹ + … + SDM_IN[1]·2⁻¹⁶` (R21 = `SDM_IN[8:1]`,
  R22 = `SDM_IN[16:9]`).
- `Ndiv = (Nint + Nfra)·2`. The VCO relation `f_VCO = f_ref·Ndiv` is
  implied, not stated.
- `REF_DIV2` (R16[4]): 0 → f_ref = f_xtal; 1 → f_ref = f_xtal/2 "(for
  Xtal > 24 MHz)".
- `SEL_DIV` (R16[7:5], "PLL to Mixer divider"): 000 /2, 001 /4, 010 /8,
  011 /1. 100–111 undescribed.
- `PLL_AUTO_CLK` (R26[3:2]): 00 128 kHz, 01 32 kHz, 10 8 kHz; 11
  undescribed.
- `PW_SDM` (R18[3]) exists; undescribed.
- PLL locking time 5 ms max; phase noise −91/−98/−109 dBc/Hz at
  1/10/100 kHz ([R820T] Table 1-2).
- Loop filter on the reference board: 1.8 kΩ + 6.8 nF, 330 pF
  ([R820T] Fig. 4-1).

### Register matrix ([R820T2] Table 1-2, p. 5)

Fixed cells as printed (mask / value over B7..B0); `-` cells are
unspecified. The document never says whether fixed cells are defaults
or required values.

| Reg | Fields (B7 → B0) | fixed mask / value |
|---|---|---|
| R0 | 1 0 0 1 0 1 1 0 (read) | 0xFF / 0x96 |
| R1 | - (read) | |
| R2 | 0, `VCO_INDICATOR[6:0]` (read) | |
| R3 | `RF_INDICATOR[7:0]` (read) | |
| R4 | - (read) | |
| R5 | `PWD_LT`, 0, `PWD_LNA1`, `LNA_GAIN_MODE`, `LNA_GAIN[3:0]` | 0x40 / 0x00 |
| R6 | `PWD_PDET1`, `PWD_PDET3`, `FILT_3DB`, 1, 0, `PW_LNA[2:0]` | 0x18 / 0x10 |
| R7 | 0, `PWD_MIX`, `PW0_MIX`, `MIXGAIN_MODE`, `MIX_GAIN[3:0]` | 0x80 / 0x00 |
| R8 | `PWD_AMP`, `PW0_AMP`, `IMR_G[5:0]` | |
| R9 | `PWD_IFFILT`, `PW1_IFFILT`, `IMR_P[5:0]` | |
| R10 | `PWD_FILT`, `PW_FILT[1:0]`, 1, `FILT_CODE[3:0]` | 0x10 / 0x10 |
| R11 | 0, `FILT_BW[1:0]`, 0, `HPF[3:0]` | 0x90 / 0x00 |
| R12 | 1, `PWD_VGA`, 1, `VGA_MODE`, `VGA_CODE[3:0]` | 0xA0 / 0xA0 |
| R13 | `LNA_VTH_H[3:0]`, `LNA_VTH_L[3:0]` | |
| R14 | `MIX_VTH_H[3:0]`, `MIX_VTH_L[3:0]` | |
| R15 | 0, 0, 1, `CLK_OUT_ENB`, 1, 0, `CLK_AGC_ENB`, 0 | 0xED / 0x28 |
| R16 | `SEL_DIV[2:0]`, `REF_DIV2`, 0, 1, `CAPX[1:0]` | 0x0C / 0x04 |
| R17 | `PW_LDO_A[1:0]`, 0, 0, 0, 0, 1, 1 | 0x3F / 0x03 |
| R18 | 1, 0, 0, 0, `PW_SDM`, 0, 0, 0 | 0xF7 / 0x80 |
| R19 | 0 0 0 0 0 0 0 0 | 0xFF / 0x00 |
| R20 | `SI2C[1:0]`, `NI2C[5:0]` | |
| R21 | `SDM_IN[8:1]` | |
| R22 | `SDM_IN[16:9]` | |
| R23 | `PW_LDO_D[1:0]`, 1, 1, `OPEN_D`, 1, 0, 0 | 0x37 / 0x34 |
| R24 | 0, 1, - - - - - - | 0xC0 / 0x40 |
| R25 | `PWD_RFFILT`, 0, 0, `SW_AGC`, 1, 1, -, - | 0x6C / 0x0C |
| R26 | `RFMUX[1:0]`, 1, 0, `PLL_AUTO_CLK[1:0]`, `RFFILT[1:0]` | 0x30 / 0x20 |
| R27 | `TF_NCH[3:0]`, `TF_LP[3:0]` | |
| R28 | `PDET3_GAIN[3:0]`, 0, 1, -, 0 | 0x0D / 0x04 |
| R29 | 1, 1, `PDET1_GAIN[2:0]`, `PDET2_GAIN[2:0]` | 0xC0 / 0xC0 |
| R30 | 0, `FILTER_EXT`, `PDET_CLK[4:0]`… | see below |
| R31 | 1, 1, 0, 0, 0, 0, -, - | 0xFC / 0xC0 |

R30: the matrix prints B7 = 0, B6 = 1 fixed and `PDET_CLK[4:0]` across
B5–B0; Table 1-3 gives `FILTER_EXT` at B6 and `PDET_CLK[5:0]`.
Contradiction — the bench decides which bits are live.

### Field meanings ([R820T2] Table 1-3, pp. 6–11)

Polarity is per field and NOT uniform — read each one.

- R5 `PWD_LT` 0 on / 1 off · `PWD_LNA1` 0 on / 1 off · `LNA_GAIN_MODE`
  0 auto / 1 manual · `LNA_GAIN` 15 max … 0 min (no dB).
- R6 `PWD_PDET1` 0 on / 1 off · `PWD_PDET3` **0 off / 1 on** ·
  `FILT_3DB` 0 = 0 dB / 1 = +3 dB · `PW_LNA` 000 max … 111 min current.
- R7 `PWD_MIX` 0 off / 1 on · `PW0_MIX` 0 max / 1 normal current ·
  `MIXGAIN_MODE` **0 manual / 1 auto** · `MIX_GAIN` 0000 min … 1111 max
  (no dB).
- R8 `PWD_AMP` (mixer buffer) 0 off / 1 on · `PW0_AMP` 0 high / 1 low
  current · `IMR_G` image gain 0 min … 63 max.
- R9 `PWD_IFFILT` 0 on / 1 off · `PW1_IFFILT` 0 high / 1 low current ·
  `IMR_P` image phase 0 min … 63 max.
- R10 `PWD_FILT` (channel filter) 0 off / 1 on · `PW_FILT` 00 highest
  … 11 lowest power · `FILT_CODE` "bandwidth manual fine tune" 0000
  widest … 1111 narrowest (no Hz).
- R11 `FILT_BW` "coarse tune" 00 widest, 01/10 middle, 11 narrowest ·
  `HPF` high-pass corner 0000 highest … 1111 lowest (no Hz).
- R12 `PWD_VGA` 0 off / 1 on · `VGA_MODE` 1 = gain from the VAGC pin,
  0 = from `VGA_CODE` · `VGA_CODE` 0000 = −12.0 dB … 1111 = +40.5 dB,
  printed "−3.5 dB/step" (the sign is a misprint: +3.5 dB/step).
- R13/R14 `LNA_VTH_H/L`, `MIX_VTH_H/L`: LNA / mixer AGC power-detector
  thresholds, 1111 = 1.94 V … 0000 = 0.34 V, ~0.1 V/step (printed
  `[4:0]` on 4-bit fields).
- R15 `CLK_OUT_ENB` 0 clock out on / 1 off · `CLK_AGC_ENB` 0 internal
  AGC clock on / 1 off.
- R16 `CAPX` internal crystal load: 00 none, 01 10 pF, 10 20 pF,
  11 30 pF.
- R17 `PW_LDO_A` PLL analog LDO: 00 off, 01 2.1 V, 10 2.0 V, 11 1.9 V.
- R23 `PW_LDO_D` PLL digital LDO: 00 1.8 V/8 mA, 01 1.8 V/4 mA,
  10 2.0 V/8 mA, 11 off · `OPEN_D` 0 High-Z / 1 Low-Z.
- R25 `PWD_RFFILT` 0 off / 1 on · `SW_AGC` 0 agc_in / 1 agc_in2.
- R26 `RFMUX` 00 tracking filter on, 01 bypass (10, 11 undescribed) ·
  `RFFILT` 00 highest, 01 middle, 10 low band (11 undescribed).
- R27 `TF_NCH` 0000 highest … 1111 lowest corner of the notch (LPNF) ·
  `TF_LP` 0000 highest … 1111 lowest corner of the low-pass.
- R28 `PDET3_GAIN` detector 3 take-off point 0 highest … 15 lowest.
- R29 `PDET1_GAIN` "0 highest … 15 lowest" on a 3-bit field (only 0–7
  exist) · `PDET2_GAIN` **0 lowest … 7 highest**.
- R30 `PDET_CLK` detector timing 111111 max … 000000 min (no units) ·
  `FILTER_EXT` "filter extension under weak signal" 0 off / 1 on.

### Hardware and electrical ([R820T])

- Frequency range 42–1002 MHz; NF 3.5 dB at max gain; image rejection
  65 dBc; **max input +10 dBm**; voltage gain 85–95 dB; AGC range
  104 dB (Table 1-2).
- IIP3 −7.5 dBm at LNA max gain, +35 dBm at LNA min gain.
- IF VGA +1 to +48 dB from the VAGC pin, 0.6–2.5 V (§4.4, Fig. 4-2).
- IF table (Table 4-1): DVB-T 6 MHz at 3.57 MHz; 7 MHz at 4.07 or
  4.57 MHz; 8 MHz at 4.57 MHz; ATSC / ISDB-T 6 MHz at 4.063 MHz; DTMB
  8 MHz at 4.57 MHz.
- Crystal 16 MHz default; 12, 20, 20.48, 24, 27, 28.8, 32 MHz on
  request; CL 16 pF; ±30 ppm typical (§6.1, Table 6-1).
- Currents: sleep 9 mA (loop-through off), standby 64 mA (loop-through
  on), operating 178 mA typical (Table 1-1).
- Tracking-filter inductors on the reference board 150 nH (L2, L6),
  Q ≥ 10 at 100 MHz, SRF ≥ 550 MHz (Table 7-1).

## RTL2832U

### Vendor control protocol (Tables 27–28, pp. 36–37)

- Get: bmRequestType 0xC0; Set: 0x40; bRequest don't-care; wLength =
  bytes to access.
- `wIndex = block << 8 | access << 4 | page` — block 0 demodulator,
  1 USB, 2 system, 3 tuner, 4 ROM, 5 IR, 6 standard I²C; access 0 get,
  1 set; page 0–4 (demodulator only).
- Demodulator: wValue = "Reg's offset in page" (no shift or flag bits
  stated — the bench decides the encoding).
- USB / system: wValue = (base << 8) + offset; USB base 0x2000,
  system base 0x3000.
- Tuner: wValue = (offset << 8) + I²C address — yet the note under
  Table 28 says "the command is issued within the DATA stage, ignoring
  the 'wValue' field". Contradiction; the bench decides.
- Before any tuner access set `IIC_repeat` — page 1, 0x01, bit 3
  (Table 3, p. 15; the note under Table 27 says "the 7th bit of the
  first byte", contradicting Table 3). Not cleared by a STOP.

### USB registers (Tables 29–34, pp. 38–40)

- `USB_SYSCTL` 0x2000: bit 10 SIE reset; bit 3 full-packet mode (send
  only max-size packets; NAK while the FIFO holds less); bit 0 DMA
  enable (reset 1).
- `USB_EPA_CFG` 0x2144: bit 7 endpoint enable; 6:5 type (reset 10b
  bulk); bit 4 direction (read-only, 1 = IN); 3:0 endpoint number.
- `USB_EPA_CTL` 0x2148: bit 9 FIFO reset; bit 5 flush; bit 4 stall;
  bit 0 FIFO valid.
- `USB_EPA_MAXPKT` 0x2158: bits 10:0, reset 64. (512 for high-speed
  bulk is the USB 2.0 standard's, not this document's.)
- `USB_EPA_FIFO_CFG` 0x2160: 31:24 dropped-block counter (188-byte
  blocks dropped because the FIFO was full); 3:0 FIFO size in blocks
  (reset 10).

### System registers (Tables 14–21, pp. 27–32)

- `DEMOD_CTL` 0x3000: bit 7 demod PLL enable, bit 6 ADC_I enable,
  bit 5 demod reset (0 = held in reset, 1 = released; reset 1), bit 3
  ADC_Q enable. No power-up order given.
- `DEMOD_CTL_1` 0x300B: "Enable IR remote wakeup & low current XTL
  mode when suspended" — no bit table.
- GPIO: GPO 0x3001, GPI 0x3002, GPOE 0x3003, GPD 0x3004 (1 = input),
  GP_CFG0/1 0x3007/0x3008. GPIO3 doubles as tuner power ("default
  1 = off"), GPIO4 antenna power ("default 0 = off"), GPIO1/GPIO5 the
  clock-select latches.
- I²C master 0x3040–0x3053 (`I2CCR` FD10 divisor, `I2CMCR`, `I2CMSTR`
  timing, `I2CMSR`, `I2CMFR`); "when powered on, software must write
  FD10".

### Demodulator registers (Tables 2–13, pp. 12–25)

- Zero-IF input (§9.1, p. 17): `AD_EN_reg` (page 0, 0x08[7]) = 1,
  `AD_EN_reg1` (0x08[6]) = 1, `en_bbin` (page 1, 0xB1[0]) = 1. Low-IF:
  only `AD_EN_reg1` = 1, `en_bbin` = 0.
- `opt_adc_iq` page 0, 0x06[5:4]: 1 swaps I and Q.
- DDC (Table 6): `Spec_inv` page 1, 0x15[0]; `En_aci` 0x15[1];
  `pset_iffreq` {0x19..0x1B}[21:0], 22-bit two's complement:
  **`pset_iffreq = −floor(f_IF / f_xtal × 2²²)`** (§9.3, p. 19;
  examples: 4.57 MHz → −665554; zero IF → 0). "With a normal
  high-side-mixing tuner the demodulator requires an inverse spectrum".
- Resampler (Table 7, p. 20): `rsamp_ratio` {0x9F..0xA2}[27:2]:
  **`rsamp_ratio = floor(f_xtal / f_s × 2²²)`** (examples at 28.8 MHz:
  64/7 MHz → 0xC99999, 8 MHz → 0xE66666, 48/7 MHz → 0x10CCCCC). Shares
  byte 0x9F with `cfreq_off_ratio` {0x9D..0x9F}[23:4].
- AGC (Table 2): `en_rf_agc` page 1, 0x04[6], `en_if_agc` 0x04[7],
  `aagc_hold` 0x04[5], `loop_gain2` 0x04[4:1] + 0x05[7]; `en_dagc`
  page 1, 0x11[0] (all reset on).
- DC / IQ (Table 5): page 1, 0xB1 — `en_dc_est` [1], `en_iq_comp` [3],
  `en_iq_est` [4]; on by default with `en_bbin`.
- Readbacks (page 3): `sfreq_off` {0x18,0x19}[13:0] (ppm =
  sfreq_off/2²⁴·10⁶), `if_agc_val`, `rf_agc_val`, `dagc_val`.
- Byte order: Table 2 says {LSB, MSB}; every other table {MSB, LSB}.
- There is **no** writable ppm-correction register, no soft-reset
  register, no SDR / raw-IQ mode and no FIR coefficient registers in
  the document.

### Clock and analog (pp. 10, 43–44)

- Crystal 28.8 MHz fundamental, ±30 ppm; the ADC clock is 28.8 MHz from
  the internal PLL. ADC bit width not stated.
- IF input 1 Vpp differential; zero-IF I/Q on pins 1, 2, 4, 5.
- Tuner I²C (Table 37): SCL 50–300 kHz.
- The configuration EEPROM on the reference circuit is an M24C02 with
  E2..E0 grounded on the host I²C pins 18/19 (Fig. 9) — address 0xA0
  by the M24C02's own addressing; the document prints no address.
