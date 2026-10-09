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

### Signal path ([R820T] Figure D, p. 7; §4.3, p. 20)

- Figure D: RF_IN (pin 24) → LNA → RF_Filter → mixer → IF_Filter → VGA →
  VOP / VON (pins 13 / 12): one differential IF output, not I/Q. `pdet1`
  taps after the LNA and feeds back to it (also pin 4 Det1); `pdet2` taps
  after the mixer (pin 5 Det2); VAGC (pin 14) drives the VGA; a buffer
  from the LNA to pin 3 LT (loop-through). PLL: PFD → CP (pin 15) → VCO
  → Div → mixer, with a second Div and an Sdm in the feedback; crystal on
  pins 8 / 9, clk_out pin 10.
- §4.3: "The mixer down-converts the RF signal to a low-IF frequency,
  which depends on channel bandwidth"; "the standard IF filters are
  implemented for 6/7/8 MHz channel bandwidths". So the demodulator's IF
  mode (§9.1) is the documented pairing, not zero-IF.

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
- Fig. 5 (§8.3, p. 15): the demodulator is drawn as its own I²C slave
  ("Demod I2C") on the bus that carries "I2C commands from driver via
  USB interface"; with the repeater on, a command "will be heard by the
  tuner and the RTL2832U at the same time", and the two "distinguish
  the origin of the command by the I²C address". The address is not
  printed.
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
- Readbacks, all read-only (the only documented values that could
  reflect signal level or a signal's presence):
  - `if_agc_val` page 3 {0x59, 0x5A} [13:0] s(14,13f) "IF AAGC Value";
    `rf_agc_val` page 3 {0x5B, 0x5C} [13:0] s(14,13f); `aagc_lock`
    page 3 0x50 [0] (Table 2, p. 13; this table's offset column is
    headed {LSB, MSB}).
  - `dagc_val` page 3 0x05 [7:0] "Gain of DAGC"; "linear voltage gain
    of DAGC = dagc_val/16"; `en_dagc` page 1 0x11[0] (Table 10, p. 22).
  - `inc_det_cnt` page 3 {0x28, 0x29} [8:0] impulse-noise events;
    `inc_det_cnt_rst` page 1 0x5E[6] (Table 9, p. 21).
  - `Est_kq` page 1 {0x66, 0x67} [11:0] u(12,11f) "Estimated Gain for
    IQ Gain Mismatch"; `Est_sin` page 1 {0x68, 0x69} [11:0] s(12,10f)
    (Table 5, p. 18; zero-IF, with `en_iq_est`).
  - `sfreq_off` page 3 {0x18, 0x19} [13:0] s(14,24f) (ppm =
    sfreq_off/2²⁴·10⁶); `cfreq_off` page 3 {0x5F, 0x61} [17:0] s(18,7f),
    in carrier spacings /2⁷ (Table 12, p. 24; §9.9, p. 23).
  - `mode_gi_idx` page 3 0x51 [2:0] (Table 11, p. 22).
- AGC (§8.1, pp. 12–13): `loop_gain2` 0–26, "recommended 20", "depends
  on the tuner"; `if_agc_min/max`, `rf_agc_min/max` 8-bit two's
  complement, "127 means maximum output voltage" (example: 10 %/90 % of
  the pull-high voltage → −103 / 101); `vtop` u(6,6f) "take-over point of
  RF VGA", 0 = RF gain always max, 63 = RF gain does not delay;
  `polar_rf_agc` / `polar_if_agc` page 0 0x0E [1] / [0] "Inverse the
  AGC_RF / AGC_IF Sigma-Delta Pin" (Table 2, p. 12). So the two analog
  AGC loops end in two sigma-delta output pins; where they go on a board
  is not the demodulator document's to say.
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

## RTL2832U block diagram and data path (Figure 1, §7)

All from [RTL2832U] page images (Figure 1 re-rendered at 3600 px, and
Figure 9 at 7200 px, then cut into tiles). "Drawn" means only what the
figure shows; nothing here is inferred from other sources.

### Figure 1, Block Diagram (§4, p. 3)

Blocks, every label as printed:

- **ADC**, with "Sampling Clock 28.8 MHz" printed above it. One input
  arrow comes in from the left; it has **no label** (no pin name, no
  I/Q). **One** ADC is drawn.
- **AGC**, with two arrows pointing out to the left, labelled
  "RF AGC" and "IF AGC".
- "IF to Baseband" (a label over the mixer stage, not a box): two
  multiplier circles (⊗), a box "90", and an oscillator circle (~).
- Two boxes **LPF** (one after each multiplier).
- **Resampler**.
- **Synchroniza-tions** (printed broken over two lines like this).
- **FFT**.
- **Channel Estimation/ Equalizer** (one box).
- **De-Mapper**.
- **Channel Decoding & De-Interleaver** (one box).
- **PID Filter**.
- **USB**.

Lines and arrows, as drawn:

1. The ADC output splits at a junction. One branch goes down and enters
   **AGC** (arrowhead into AGC). The other branch splits again, into the
   upper and the lower multiplier.
2. The oscillator (~) feeds the **lower** multiplier directly (arrow up
   into it). A thin grey line leaves the oscillator and goes up to the
   right side of the "90" box (no arrowhead). The "90" box feeds the
   **upper** multiplier (arrow up into it).
3. Upper multiplier → LPF (arrow); lower multiplier → LPF (arrow).
4. The upper LPF output is labelled **I**, the lower LPF output **Q**.
   Both lines run into the left side of **Resampler**, with no
   arrowheads.
5. Resampler → Synchronizations → FFT (arrows, left to right).
6. "Timing Error": a line from the top of Synchronizations, over and
   down into the top of Resampler (arrowhead into Resampler).
7. "Frequency Error": a line from the bottom of Synchronizations, down
   and left into the oscillator (arrowhead into the oscillator).
8. FFT → down to Channel Estimation/Equalizer (arrow). The bottom row
   then runs right to left: Channel Estimation/Equalizer → De-Mapper →
   Channel Decoding & De-Interleaver → (labelled "MPEG 2") → PID Filter →
   (labelled "MPEG 2") → USB. No line leaves USB.

The data path as drawn, in order: ADC → multipliers driven by the
oscillator and its 90 copy ("IF to Baseband") → LPF ×2 → **I / Q** →
Resampler → Synchronizations → FFT → Channel Estimation/Equalizer →
De-Mapper → Channel Decoding & De-Interleaver → MPEG 2 → PID Filter →
MPEG 2 → USB.

- **One** path, **one** output. Figure 1 draws no branch, mux, bypass,
  tap or second output on the data path. The only side branches are
  the ADC → AGC branch (out as RF AGC / IF AGC) and the two feedback
  loops (Timing Error → Resampler, Frequency Error → oscillator).
- Figure 1 draws **no** 8051, I²C (master, repeater or pins), IR, GPIO,
  EEPROM, TS pin or M_ERR. USB appears only as the data path's last
  block.
- Figure 1 draws none of these, though the text describes them: DC
  cancellation / IQ compensation (§9.2), adjacent-channel rejection
  (§9.3 `En_aci`), co-channel interference rejection (§9.5), impulse
  noise cancellation (§9.6), DAGC (§9.7), guard interval removal (§7.5),
  the TPS decoder (§7.9), the descrambler (§7.11). The document never
  says where these sit relative to the drawn blocks.
- The text's section order (§7.5 guard interval removal, §7.6 FFT,
  §7.7 synchronization) is not the figure's (Synchronizations before
  FFT); §7.5 says guard interval removal works "according to the
  results of synchronization".
- **Contradiction:** Figure 1 draws one ADC with one input, but §7.1
  speaks of "Zero IF (I & Q channel) input". Table 1 (p. 5) also lists
  two differential input pairs (VIP/VIN "I Path", VQP/VQN "Q Path"), and
  Tables 4 / 15 (pp. 17, 29) give separate ADC_I / ADC_Q enables.
  Figure 1 draws only the IF case.

### §7 Functional Description (pp. 7–9), quoted exactly

Quoted with the document's own typos and punctuation (marked *sic*).

- §7 (p. 7): "The block diagram of the RTL2832U DVB-T demodulator is
  shown in Figure 1, page 3. The RTL2832U accepts IF or Zero IF input
  signals with the analog signal sampled by the internal ADC. The sampled
  data stream is then processed by OFDM demodulation. After decoding by
  an on-chip FEC (Viterbi and Reed-Solomon decoder) the USB 2.0 interface
  outputs packets with transport stream data."
- §7.1 ADC (p. 7): "The internal A/D converter can accept tuner output
  with various bandwidths (6, 7, 8MHz), different IF frequencies (4.57M
  or 36.167M,), Zero IF (I & Q channel) input and can perform spectrum
  reversion." (*sic*) "Using a sampling clock generated by the internal
  PLL with a 28.8MHz clock source, the RTL2832U demodulates the received
  TV signal."
- §7.2 AGC (p. 7): "The AGC circuit is used to adjust received signal
  strength to a moderate level for the ADC. This module supports two
  output paths, and both are sigma-delta modulated signals. The output
  signals need additional RC LPFs (Low-Pass Filters) before feeding to IF
  and RF VGA in the tuner The delayed AGC algorithm is described in
  section 8.1 …" (missing period *sic*).
- §7.3 Digital Down Conversion (p. 7): "The RTL2832U uses IF or Zero-IF
  sampling to process received signals. The Digital Down Conversion
  (DDC) circuit converts the sampled IF signal to a complex base-band
  signal for further processing. The down conversion frequency and
  low-pass filter can be programmed according to different IF frequency,
  sampling rates, and signal bandwidth." No register for the low-pass
  filter appears in Table 6 (p. 20), which lists only `Spec_inv`,
  `En_aci` and `pset_iffreq`.
- §7.4 Resampler (p. 8): "The Resampler circuit changes the received
  signal from a fixed ADC sampling rate to an Orthogonal Frequency
  Division Multiplexing (OFDM) sampling rate according to the signal
  bandwidth. The conversion ratio can be programmed via a register
  setting."
- §7.5 Guard Interval Removal (p. 8): "… It is necessary to remove the
  guard interval before Fast Fourier Transform (FFT) processing. This
  module is used to moderate the OFDM symbol boundary for FFT according
  to the results of synchronization."
- §7.6 FFT (p. 8): "The Fast Fourier Transform (FFT) circuit converts a
  received time domain signal to a frequency domain signal. Based on the
  ETSI 300-744 definition, FFT output contains continuous pilots,
  scattered pilots, Transmission Parameter Signal (TPS), and data
  signal. These signals can be used for synchronization, channel
  estimation, and data decision in further processing."
- §7.7 Synchronization (p. 8): "The RTL2832U can measure and compensate
  for a large range of sampling frequency offsets and carrier frequency
  offsets before making a data decision. A moderate symbol boundary is
  utilized to avoid Inter-Symbol Interference (ISI)."
- §7.8 Channel Estimation (p. 9): "… The estimated channel response can
  be used for equalization, and is updated symbol by symbol …"
- §7.9 TPS Decoder (p. 9): "… Parameters such as transmission mode,
  guard interval value, code rate etc, can be pre-set by registers to
  overwrite the result of TPS decoding."
- §7.10 Equalization (p. 9): "… The data bit is detected based on the
  equalization output."
- §7.11 De-Interleaver, FEC Decoder, and Descrambler (p. 9): "In
  accordance with ETSI 300-744, the RTL2832U uses a de-interleaver to
  re-order the decision data bit to the correct sequence. The Forward
  Error Correction (FEC) decoder circuit detects and corrects error bits
  in the received sequence. The descrambler recovers the output of the
  decoder to a standard Transport Stream (TS) sequence."
- §7 never mentions an output other than the USB transport stream. It
  says nothing on a parallel or serial TS interface, a TS clock, or
  valid / sync signals.

Related text outside §7:

- §8 (p. 10): "There are three interfaces (AGC, ADC input, two-wire
  serial interface) between the tuner and the demodulator."
- §8.2 (p. 14): "Tuner IF or Zero IF output is AC coupled to the
  RTL2832U differential ADC input. An external AC coupling capacitor is
  required."
- §9.3 (p. 19): "The Analog-to-Digital Converter (ADC) block sub-samples
  Intermediate Frequency (IF) signals, and a Digital Down Conversion
  (DDC) block converts the IF to baseband signals." The `pset_iffreq` and
  `rsamp_ratio` examples (pp. 19–20) include a "DAB Mode" (0x2FB814;
  0xE10000). The document says nothing else about DAB.
- §9.4 (p. 20): "As the ADC sampling clock is larger than the symbol
  ratio, there is a re-sampler to convert sampling data to symbol
  ratio."
- §9.2 (p. 18): "In Zero-IF mode, the output signal of the tuner often
  has DC bias and IQ mismatch issues. … DC cancellation and IQ
  compensation blocks are default enabled when *en_bbin* is set to 1."
- **Contradiction, IF frequency:** 36.125 MHz (§1, p. 1) against
  36.167 MHz (§2 p. 2, §7.1 p. 7, Fig. 3 p. 10). §9.3 gives
  `pset_iffreq` examples for both; its 36.125 line prints
  "f_IF_D = 36.167-28.8 = 7.367" (copy error *sic*).
- **Contradiction, crystal tolerance:** "±100ppm" (§2 p. 2, Fig. 3
  p. 10, §9.10 p. 24) against the ±30 ppm in the "Clock and analog"
  subsection above.

### Where baseband / I-Q exists in the chain

- Figure 1: complex baseband exists as the lines labelled **I** and
  **Q**, between the two LPFs and the Resampler. It continues, drawn as
  one line, through Resampler → Synchronizations → FFT. After the FFT it
  is frequency-domain (§7.6).
- §7.3 / §9.3: the DDC makes "a complex base-band signal" from the
  sampled IF.
- Zero-IF: I and Q arrive already split, on VIP/VIN and VQP/VQN (Table 1
  p. 5), each to its own ADC (ADC_I / ADC_Q, Tables 4 and 15). §9.2 DC
  cancellation and IQ compensation run in this mode. The document does
  not say whether the Figure 1 mixers are bypassed when
  `pset_iffreq` = 0 (§9.3 zero-IF example).
- No pin, register, endpoint or mode in the document outputs these
  samples. The only output drawn (Fig. 1) or described (§7, §11.1) is
  the MPEG-2 transport stream over USB.
- **Naming contradiction, I against Q:** §9.1 (p. 17) "IF signal mode"
  enables only `AD_EN_reg1`, which Table 4 calls "Enable ADC_Q". Yet
  Fig. 3's IF variant (p. 10) drives VIP/VIN, which Table 1 calls the
  "I Path". Fig. 9 (p. 44) labels the nets on VIP/VIN (pins 1/2)
  **Q_P / Q_N**, and those on VQP/VQN (pins 4/5) **I_N / I_P**, in that
  order. §9.1's lists also say "AD_EN_reg0" where Table 4 has
  `AD_EN_reg`. `opt_adc_iq` (Table 4) swaps the ADC_I / ADC_Q datapaths.

### TS interface / MPEG-TS port

- **No TS pins.** Fig. 2 (p. 4) and Table 1 (pp. 5–6) list no TS
  data, clock, valid, sync or start pins, parallel or serial. The only
  MPEG pin is **M_ERR**, pin 35, "MPEG Error Output", type O (Table 1,
  p. 6). Fig. 9 (p. 44) leaves pin 35 unconnected (×).
- The document has **nothing** on a parallel or serial TS mode, a TS
  clock, or valid / sync signals.
- The TS leaves only through USB: §7 (p. 7, quoted above); §11.1
  (p. 36): "The RTL2832U transfers transport stream data from the
  demodulation module to the host via an embedded high-speed USB 2.0
  interface (compatible with USB 1.1). Two endpoints are supplied. One
  is the control pipe and the other is the data pipe for TS transfer. …
  The data pipe can be configured to BULK mode or ISO mode."
- Endpoint A FIFO unit = one TS packet: `USB_EPA_CTL` bit 5 "FIFO Flush.
  Write 1 to flush the oldest TS packet (a 188 bytes block)"; bit 0
  "FIFO Valid. Write 1 to validate the TS packet (a 188 bytes block)"
  (Table 32, p. 40). `USB_EPA_FIFO_CFG`: `BLK_DROP_COUNTER` "Counts
  blocks dropped (each is 188 bytes) due to full FIFO"; "EPA FIFO Size …
  in 188-byte blocks. Valid data range is 1~10", reset Ah (Table 34,
  p. 40). `USB_EPA_CFG` bits 9:8 isochronous "Must be set to 00"; bit 7
  Endpoint Enable, reset 0 (Table 31, p. 39).
- PID filter (§9.11, p. 24; Fig. 7; Table 13, pp. 25–26), between the
  decoder and USB (Fig. 1). "There are 32 PIDs that can be selected
  within the received multiplex … the default register setting will let
  all PID transport streams pass through."
  - Fig. 7 top: "TS, PID=y" → [function | PID filter] → "After PID
    filtering TS".
  - Fig. 7 detail: for each n = 0…31, "PID=y" and "PIDn" go into
    "compare"; compare AND `en_PID[n]`; the 32 ANDs are ORed; the OR is
    XORed with `mode_PID`. Output "Bit 1: Pass the TS of PID = y /
    Bit 0 : Rejected the TS of PID = y".
  - Page 0, 0x61: `err_pass` [5] (0 reject / 1 pass all error packets,
    default 1), `mode_PID` [6] (0 pass matched / 1 reject matched,
    default 1), `enable_PID` [7] ("Set 1 to Enable Output of PID Filter
    / Set 0 No Output", default 1). `en_PID[31:0]` at 0x62–0x65,
    default 0. `PID0`–`PID31` [12:0] at {0x66,0x67} … {0xA4,0xA5}.
  - With the defaults (all `en_PID` 0, `mode_PID` 1) the XOR gives 1:
    pass all. This agrees with the text.
  - `enable_PID` = 0 is documented as "No Output" — a TS gate ahead of
    USB.

### I²C buses (Fig. 5, p. 15; Fig. 9, p. 44; Table 1, p. 6)

- Two pin pairs (Table 1, p. 6):
  - Tuner bus: I2C_SCLT pin 16 (OD, "Output SCLK Signal (5 Voltage
    Tolerance) for Tuner Control") and I2C_SDAT pin 17 (I/OD).
  - Under "Host Interface": I2C_SCL pin 18 (OD, "I²C Interface Clock
    Output Pin") and I2C_SDA pin 19 (I/OD).
- **Fig. 5, the repeater.** Labels as printed:
  - A "Tuner" box outside the "RTL2832U" box. Its bus is labelled
    "I2C_SCLT / I2C_SDAT" and enters the RTL2832U box on the left.
  - Inside the box, the bus passes through a switch, drawn open with a
    small arrow at its blade and labelled "I2C_repeat==1".
  - Right of the switch, a junction: a line goes up to an inner box
    "Demod I2C".
  - The bus continues right, out of the RTL2832U box, to the label
    "I2C commands from driver via USB interface".
  - So the repeater sits **between the tuner pins and an internal bus**
    that carries both the driver's USB-borne commands and the "Demod
    I2C" slave. With the switch closed, the tuner and "Demod I2C" share
    that bus.
  - Fig. 5 does not draw pins 18/19.
  - The figure's label is "I2C_repeat"; the register (Table 3) is
    `IIC_repeat`.
- **Fig. 9 (reference circuit).** Separate nets:
  - Pins 16 / 17 go to an off-board connector box "SCLT" / "SDAT", with
    R9 / R10 4.7K pull-ups to VDD3.
  - Pins 18 / 19 go to U4 M24C02-W SCL (pin 6) / SDA (pin 5), with R12 /
    R13 4.7K pull-ups to VDD3. E0/E1/E2/VSS (pins 1–4) are grounded,
    MODE (pin 7) is unconnected (×), VCC (pin 8) is on VDD3.
  - **Contradiction, pull-ups:** 4.7K here against "10k ohm" in §8.3
    (p. 15) and Fig. 3 (p. 10).
- Fig. 3 (p. 10) draws SCLT and SDAT from the demodulator to the tuner,
  both with arrowheads into the tuner, each pulled up by 10k ohm to
  VDD3.3. The demodulator's "DA" block drives "IF AGC" (solid) and
  "RF AGC" (dashed) through RC networks with pull-ups to VDD3.3. A thick
  "USB" arrow leaves the demodulator. Two variants:
  - IF: "IF differential input 1Vpk-pk 36.167MHz", 6pF on VIP and VIN.
  - Zero-IF: "Zero-IF differential input 1Vpk-pk", 0.1uF on each of VIP,
    VIN, VQP and VQN. Fig. 9 uses 1nF (C2–C5).
- GPIO[2] (§10.2, p. 29): "Input for external I²C mode, internal
  pull-down. This pin is used for LED control after 8051 firmware runs".

### 8051, USB, IR, GPIO relative to the data path

- None is drawn on Figure 1's data path except USB, its last block.
- 8051: appears only in §10's title "Register Descriptions (8051
  System)" (p. 27, base 0x3000) and in the GPIO[2] line above (p. 29).
  No figure draws it.
- IR: pin 38 IRRC "IR Signal Input" (Table 1); IrDA registers
  FC00–FD19 (Table 14, pp. 27–28); §11.1 "remote wakeup capability via
  IrDA".
- GPIO: 8 pins (Table 1), registers 0x3001–0x3008 (§10.2).
- Fig. 9 also ties pin 41 HSDM → "DM" and pin 40 HSDP → "DP" of the
  "USB Interface" connector, and pin 13 AGC_IF → R11 10K → connector
  "IF_AGC" (10nF C14 to ground, 1K R7 to VDD3). It leaves pin 15 AGC_RF
  and pin 6 VINR unconnected (×).
