# Bench findings

Replay: `cargo run --release -p sdr-rtl --example harness -- <step>`

Status (2026-10-09): **the single-write hypothesis is exhausted.** From the documented baseline, no single write anywhere the register map has room for makes the chip stream raw samples: the bit scan (10,240), the plain byte scan of demodulator pages 0–2 (196,608), the restart byte scan of pages 0–2 (196,608, complete), pages 3 and 4 (65,536 each), the system block (65,536) and the USB block (130,407, with 155 test-mode values deferred) — about 790,000 candidates, **0 streams**. The only bytes ever seen are 188-byte transport-stream blocks (page 0 0x07, both scans; page 1 0x92 in the plain one). The raw-sample mode therefore needs more than one write (or a state the documented baseline excludes); how to search that larger space is a decision for the project owner. **Update, same day:** the bit scan was re-run from two other starting states — the documented low-IF state with the tuner tuned (step 4's, proven live by the tone) and the bare power-on state — 2 × 10,240 candidates, **0 streams** (section "the bit scan from two other starting states" below). The only new bytes: page 0 0x07 bit 7 with the tuner programmed replays the endpoint FIFO's 20 × 188 bytes at 45 MB/s, not samples. With that path held open, a third bit scan (10,240, content judged per window) found **no bit that changes the content**: every window that could be scored was the same 3,760-byte replay; 28 bits only throttle or stop it. Records compressed where large (`data/step3-bytescan.jsonl.gz`, `data/step3-bytescan-restart.jsonl.gz`); `gunzip -k` before re-running either scan. The bulk detector is proven on the wire (below).

Step 9 (the vendor's Windows driver observed on the wire), 2026-10-09: **an interoperability observation, not a bench result.** A USB analyzer capture of Realtek's own driver under a DAB player shows the chip streaming raw samples: 954,067 bulk packets of 512 bytes, 8-bit unsigned interleaved pairs centred on ≈ 126.7, ≈ 4.1 MB/s (2 × 2.048 Msps within the analyzer's timebase error), never the 3,760-byte replay. The stream begins **0.55 ms after the driver writes page 0 0x06 = 0x80**, the last of 22 demodulator writes made after a demodulator reset (t = 60.377 s); the driver **never writes page 0 0x07, page 0 0x08 (AD_EN) or page 1 0xB1 (en_bbin)**, and the stream runs before the tuner is programmed. A multi-register state none of our baselines held. Section "Step 9" below; the bench test that would confirm it is at its end (not run).

Step 5 (the chip's firmware), 2026-10-09: the 8 KiB ROM read through `GetROMCode`, mapped with our own 8051 disassembler. **The firmware never touches the data path**: it configures bulk endpoint A at SET_CONFIGURATION / SET_INTERFACE and acknowledges its interrupt, and nothing else; it has no mode variable, no mode table and no source selector. The demodulator is an I²C slave the firmware reaches only on the host's behalf. What feeds the FIFO is hardware, steered (on the bench evidence) from the demodulator's register file. Two spaces the scans never covered follow from the code: **demodulator pages 5–15** (the page nibble is masked to 4 bits, not 0–4) and **external-data addresses outside 0x2000–0x21FF / 0x3000–0x30FF** (blocks 1 and 2 are a raw window on the whole 16-bit space). Section "Step 5 — the firmware" below.

Step 7 (the datasheet's DAB example), 2026-10-09: the front half at §9.3/§9.4's DAB example (pset_iffreq 0x2FB814, rsamp_ratio 0xE10000 = 8.192 MHz), every documented DVB-T back-end enable off (ACI, CCI, impulse-noise cancellation, DAGC, auto mode detection; PID filter at its documented pass-all), the output held open: alone it is the same replay; the bit scan (10,240) with the content detector and the DAB rate targets found **no candidate that carries the tone or changes the content** (period 1.000 in all 10,225 scorable windows; 5 rate-flagged candidates confirmed as the replay). Section "Step 7" below.

Step 8 (zero-IF input with the tuner tuned and the path held open), 2026-10-09: the documented zero-IF input (§9.1 AD_EN_reg = AD_EN_reg1 = 1, en_bbin = 1; §9.3 pset_iffreq = 0) on top of the low-IF state's tuner image (LO 304.57 MHz) and I²C repeater, page 0 0x07 = 0xBC last — a combination no earlier scan held. Alone it is the same 3,760-byte replay (44.4–44.6 MB/s tone off, ~37 MB/s with the transmitter keyed); the tone moves `if_agc_val` 8191 → −449. The bit scan (10,240, the tone keyed throughout, content detector): **1 interesting** (p0 0x06 b4, by rate alone), confirmed as the replay — no rsamp tracking (42.40 / 42.57 / 42.46 MB/s at 2.4 / 1.2 / 2.048 Msps), period 3,760 in every state, no spectral line that rises with the tone or moves with it. Period score ≥ 0.9999 in all 10,224 scorable windows. **No candidate carries the tone.** Section "Step 8" below.

Step 6 (the spaces step 5 opened), 2026-10-09: a read-only survey of demodulator pages 5–15 and the 8051's external data outside the scanned blocks, and the single-bit scan of pages 5–15 (22,440, register 0x00 excluded) from the held-open data path with the content detector: **0 interesting**, period score 1.000 in all 22,423 scorable windows. Pages 8–15 alias pages 0–7 on read (8 = 0, 9 ≈ 1, 10 ≈ 2, 12 = 4), writes alias by page mod 4 (on the bench evidence); 5–7, 11, 13–15 read like the live page 3. External data outside the blocks is zero except X:2F00–X:2FFF (a few bytes, X:2F80 live), the data RAM X:4000–X:41FF and the IR range X:FC00–X:FD3F. Section "Step 6" below.

Step 4 (the tuner against a cabled tone), 2026-10-07/08: all steps run.
Results: the demodulator is clocked from the tuner's clock output (4a);
the meter is `if_agc_val` MSB-first with the IF loop closed through the
VAGC pin, polarity 0 (4b); VCO 1.67–3.69 GHz, dividers /2 /4 /8 (/1
untestable) confirmed, SEL_DIV 100 = /32, 101 = /64, 110/111 unusable,
**LO 114–212 MHz unreachable** (4c, 4d); fractional tuning works (4e);
band table over 87 frequencies (4f); IF filter corners per FILT_CODE
and FILT_BW, HPF without effect (4g); the meter is pre-DDC, the wanted
sideband is below the LO, image rejection 10–20 dB uncalibrated (4h);
gain per code for LNA, mixer and VGA at 1 dB resolution (4i, 4j).

## Step 9 — the vendor driver on the wire (interoperability observation) — capture outside the repository (`~/mimosdr-captures/rtl2832u-windows/`)

**Source of these facts.** Approved by the project owner on 2026-10-09
under the rule of step 5: the driver we build never contains or uses any
part of the vendor's code. What follows is what the vendor's Windows
driver (Realtek's proprietary driver, under a DAB player that decodes in
software) was **observed** to put on the USB wire: which requests, which
registers, which values, in which order, and what the chip did. Every
fact is cited by its capture time `t` (analyzer seconds). No line of the
capture is reproduced here; the capture, the decoded transfer list and
the throwaway parsing scripts stay in the capture folder. Nothing here
was tested on our bench.

**The capture.** 178.5 s, the dongle (VID 0BDA, PID 2832, high speed)
behind the USB analyzer, replugged at the start, device address 7. One
line per USB packet except start-of-frame (SOF) packets; data payloads
longer than 64 bytes kept to their first 16 bytes. Decoded (scripts in
the capture folder: `parse_capture.py`, `decode_control.py`,
`tuner_track.py`, `bulk_stats.py`): 4,401 control transfers (4,239
acknowledged, 162 stalled) and 954,067 acknowledged bulk IN packets on
endpoint 0x81, every one 512 bytes (488,482,304 bytes); 45,125,015 IN
tokens and 44,176,110 NAKs on that endpoint.

**Encodings seen** (all match steps 1, 2 and 5c): vendor requests with
bRequest 0; demodulator wValue = register << 8 | 0x20, wIndex = page
(Get) or 0x10 | page (Set); USB/system wValue = the address, data
little-endian; the tuner through **block 6** (standard I²C), wValue
0x0034, the register as the first data byte and the values after it,
reads (block 6, Get) returning R0 onward bit-reversed. After **every**
demodulator write the driver reads page 10 register 0x01 (wIndex 0x000A;
464 reads, always 0x09). Power-on values quoted below for comparison are
our own bench's (the `original` field of `data/step3-scan-poweron.jsonl`).

### 9a. Timeline

| t (s) | what the wire shows |
|---|---|
| 56.58–56.67 | enumeration: device descriptor at address 0, SET_ADDRESS 7, descriptors, SET_CONFIGURATION 1 |
| 56.675–56.707 | USB set-up: USB_SYSCTL ← 0x09 (DMA enable and bit 3, full-packet mode); USB_EPA_CFG read 0xD1; USB_EPA_MAXPKT ← 0x0200 and the next two bytes (0x215A) ← 0; USB_EPA_FIFO_CFG read 0x14, written 0x14, read 0x14; USB_EPA_CTL ← 0x0210 (FIFO reset and stall) then ← 0 |
| 56.711–56.779 | GP_CFG0, GPO, GPD, GPOE, USB 0x2010 each read and written back unchanged; DEMOD_CTL_1 0x02 → 0x22; DEMOD_CTL 0x20 → 0xA0 → 0x80 → 0xA0 (PLL on, then a reset pulse on bit 5) |
| 56.831–56.853 | page 0 0x01 read (0x04); DEMOD_CTL → 0xA8 → 0xE8 (ADC_Q, then ADC_I) |
| 56.91–58.07 | **probing**: tuner reads through block 3 at I²C 0xC0, 0xAC, 0xC8, 0xC2 stall (three tries each); at 0x34 R0 = 0x96 (t = 57.353); block 0 reads at I²C 0x44, 0x3E, 0x24 and a block 6 access at 0x26 stall; each try bracketed by page 1 0x01 = 0x18 / 0x10 (the I²C repeater, step 2); GPO bits 0, 6, 7 and GPOE bits 6, 7 toggled between tries |
| 58.10–58.17 | GPIO end state GPO 0x98, GPD 0x03, GPOE 0xDC |
| 58.171–58.191 | DEMOD_CTL 0xE8 → 0xA8 → 0xA0 → 0x20 (ADCs off, PLL off). **No demodulator register was written in the whole driver-load phase** |
| 58.19–59.88 | nothing on the wire |
| 59.876–59.912 | GPIO read back unchanged; USB_EPA_CTL ← 0x0210 then ← 0; CLEAR_FEATURE(ENDPOINT_HALT) on 0x81 |
| 59.919 | **the host starts polling endpoint 0x81** (first IN token); 186,007 IN tokens are NAKed until the first data |
| 59.919–60.375 | **the stream-start sequence, 9b** |
| **60.377394** | **first bulk data**, 0.552 ms after the data stage of the page 0 0x06 = 0x80 write was accepted (t = 60.376842) |
| 60.383–60.801 | 9c's writes (the stream already running) |
| 60.788–60.797 | page 1 0x01 = 0x14 then 0x10: the stream pauses 9.0 ms (9d) |
| 65.584–70.83 | the tuner programmed and calibrated (9e) — 5.2 s **after** the stream began |
| 70.73–173.77 | 38 tunings, one per ~2.78 s (9e), each followed by page 1 0x16–0x18 ← 0, 0x3E–0x3F ← 0 and the 0x01 bit 2 pause |
| 173.77 | last control transfer |
| 178.456 | last bulk packet; the capture ends with the stream still running. **No stop sequence is on the wire.** |

### 9b. The stream-start sequence (t = 59.919–60.375), in order

Every write below, nothing omitted (the read of page 10 0x01 after each
demodulator write and the GPIO read-and-write-back pairs left out). "PO"
= our bench's power-on value.

| # | t (s) | write | PO |
|---|---|---|---|
| 1 | 59.923 | DEMOD_CTL_1 ← 0x02 (read 0x02) | |
| 2 | 59.931–59.947 | DEMOD_CTL 0x20 → 0xA0 → 0x80 → 0xA0 (PLL on; demodulator reset held, then released) | 0x20 |
| — | | 65 ms without a request | |
| 3 | 60.012, 60.017 | reads: page 0 0x01 = 0x04, page 10 0x01 = 0x09 | |
| 4 | 60.026, 60.034 | DEMOD_CTL → 0xA8 → 0xE8 | |
| 5 | 60.041 | GPO 0x98 → 0x88 (GPIO4 low) | |
| 6 | 60.061 | page 1 0x3E–0x3F ← 00 00 (one 2-byte write) | 40 00 |
| 7 | 60.070 | page 1 0x15 ← 0x00 | 0x06 |
| 8 | 60.079 | page 1 0x16–0x17 ← 00 00 | 3F CE |
| 9 | 60.083 | page 1 0x18 ← 0x00 | 0xCC |
| 10 | 60.092, 60.096 | page 1 0x19–0x1A ← 38 11, 0x1B ← 12: **pset_iffreq = 0x381112 = −519,918 → IF 3.570 MHz** (§9.3) | 35 D7 5D |
| 11 | 60.105 | page 1 0x15 ← 0x01 (Spec_inv = 1, En_aci = 0) | 0x06 |
| 12 | 60.113, 60.117 | page 1 0x9F–0xA0 ← 03 84, 0xA1–0xA2 ← 00 00: **rsamp_ratio = 0xE10000 (8.192 MHz)**, the high nibble of 0x9F (cfreq_off_ratio's) 0 | F3 26 66 64 |
| 13 | 60.126–60.295 | page 1 0x1C … 0x2F, one byte each, ascending: CA DC D7 D8 E0 F2 0E 35 06 50 9C 0D 71 11 14 71 74 19 41 A5 (20 registers no document names) | 09 F6 D2 A7 9A C9 27 77 06 BF EC F4 4F 0B FC 01 63 35 54 A7 |
| 14 | 60.304 | page 0 0x17 ← 0x11 | 0x13 |
| 15 | 60.313 | page 0 0x18 ← 0x10 | 0x10 |
| 16 | 60.322 | page 0 0x19 ← 0x25 | 0x20 |
| 17 | 60.330 | page 1 0x92 ← 0x00 | 0x00 |
| 18 | 60.339 | page 1 0x93 ← 0xF0 | 0x0F |
| 19 | 60.348 | page 1 0x94 ← 0x0F | 0xFF |
| 20 | 60.357 | page 0 0x61 ← 0x60 (Table 13: **enable_PID = 0** "No Output", mode_PID = 1) | 0xE0 |
| 21 | 60.366 | page 0 0x20 ← 0x40 | 0x40 |
| 22 | 60.375 | **page 0 0x06 ← 0x80** (bit 7; opt_adc_iq [5:4] = 0) | 0x00 |
| | **60.377394** | **first bulk data** | |

Of these, items 6–14, 16, 18–20 and 22 change a register from its
power-on value; 15, 17 and 21 rewrite it. Between the endpoint polling
start (59.919) and item 22 every IN token was NAKed; after item 21
(60.366) the endpoint still NAKed for 11 ms; the first data came 0.55 ms
after item 22's data stage.

### 9c. Written after the stream began (t = 60.383–60.801)

Read then written, in order: page 1 0x12 0x39 (unchanged), 0x02 0x40
(unchanged), 0x03 0x2D → 0x40, 0xC7 0x18 → 0x2C, 0x04 0xC0 → 0xD0, 0x05
0xBE (unchanged), 0xC8 0x1A → 0x18, 0x06 0x10 → 0x35, 0xC9 0x30 → 0x21,
0xCA 0x28 → 0x21, 0xCB 0x02 → 0x00, 0x07 0x0F → 0x40, 0xCD 0x20 → 0x10,
0xCE 0x30 → 0x10, 0x08–0x0B unchanged (0x80 0x7F 0x80 0x7F); page 0 0x0E
0xFC twice (unchanged); page 0 0x11 0xC9 → 0xF4; page 1 0x01 0x10 → 0x14
→ 0x10. None of these is needed for the first bytes (they come after
them).

**Never written by the driver in the whole capture:** page 0 0x07, page 0
0x08 (AD_EN_reg / AD_EN_reg1), page 1 0xB1 (en_bbin and the DC/IQ
enables), page 1 0x11 (en_dagc), page 1 0x9D–0x9E, any page 2, 3 or 4
register, USB_EPA_CFG. **Never read:** any page 3 readback (no AGC or
lock polling at all).

### 9d. Stream pause: page 1 0x01 bit 2

39 times (t = 60.788, then once per tuning from 71.130 to 173.730): page 1
0x01 0x10 → 0x14, then ~9 ms later → 0x10. The bulk stream stops
2.3 ms after the 0x14 write begins and resumes 2.4 ms after the 0x10
write begins: 39 gaps of 8.94–9.02 ms, the only gaps over 0.4 ms in the
stream but one (0.71 ms at t = 65.882, during the tuner's R16 write).
Each such second carries ≈ 72 fewer packets (8,032 instead of 8,104).
Bit 2 of page 1 0x01 is one of the 28 throttle/stop bits our held-open
scan found (step 3).

### 9e. The tuner (facts that bear on our open items)

- Programmed through block 6 only, 5.2 s after the stream began: R5–R31
  written in full at t = 65.603, 65.709 and 70.380; between them (t =
  65.82–70.37) **a loop of 65 R8 and 65 R9 writes, R1 read back after
  each** (IMR_G / IMR_P, [R820T2] Table 1-2), with PLL changes (R16,
  R20–R22) and writes of R12, R26 and R31 — the vendor calibrates image
  rejection, which our step 4h left open. 393 tuner reads in t =
  65.60–70.83.
- 38 tunings: channel 5A at t = 70.73, then 37 retune bursts of 42 writes,
  one per ~2.78 s (t = 73.43–173.23).
  With f_ref = 14.4 MHz (R16 bit 4 = 1) and f_LO = f_ref · 2 · (Nint +
  SDM/2¹⁶) / div, **LO − 3.570 MHz lands on the 38 DAB Band III channel
  centres 5A–13F (174.928–239.200 MHz) within 1 kHz** — if, and only if,
  SEL_DIV 011 divides by 16: the driver uses **SEL_DIV 011 with VCO
  2.856–3.528 GHz for LO 178.50–220.49 MHz** (5A–11A) and SEL_DIV 010
  (/8, step 4c) with VCO 1.778–1.942 GHz for LO 222.21–242.77 MHz
  (11B–13F). [R820T2] prints 011 as /1; our step 4c could not test it.
  So the observation implies **SEL_DIV 011 = /16**, which reaches LO
  106–228 MHz with our measured VCO range — the band step 4d found
  unreachable. Bench confirmation pending.
- ~249 ms after each retune the driver writes R29 = 0xDD and the chip
  stalls it three times (accepted once, t = 148.726).
- Final tuner state (t = 173.23, retune to 13F): R5 03, R6 12, R7 75,
  R8 E1, R9 40, R10 D6, R11 6B, R12 F9, R13 53, R14 75, R15 68, R16 54,
  R17 BB, R18 80, R19 31, R20 8D, R21 A6, R22 6F, R23 30, R24 48, R25 EC,
  R26 3A, R27 13, R28 20, R29 C5, R30 6A, R31 40.

### 9f. The bulk data (from the kept 16-byte packet prefixes)

| | t 60.377–65.60 (tuner not programmed) | t 71.17–178.46 (scanning DAB) |
|---|---|---|
| packets / prefix bytes | 42,244 / 675,904 | 866,756 / 13,868,096 |
| distinct 16-byte prefixes | 38,726 | 862,702 |
| byte values | 124–128 only (126: 47 %, 127: 45 %) | all 256; entropy 6.94 bits; 0x47 at 0.24 % (uniform 0.39 %) |
| even bytes: mean / variance | 126.54 / 0.39 | 126.73 / 922.5 |
| odd bytes: mean / variance | 126.54 / 0.39 | 126.74 / 923.0 |
| even/odd correlation | +0.03 | −0.0002 |
| clipped (0 or 255) | 0 | 0.015 % |
| lag-1 complex autocorrelation (8 pairs per prefix) | 0.73 | 0.125 at −15.7° |

- **Not the replay.** A 3,760-byte replay read in 512-byte packets has at
  most 235 distinct prefixes; this stream's prefixes are 92–99.5 %
  distinct from the first packet on.
- **8-bit unsigned interleaved pairs, both channels alive:** the two byte
  positions have the same mean and variance and are uncorrelated; with
  the tuner idle the bytes sit at 126–127 (a converter with no input),
  and the per-second variance jumps to 365–1100 from the second the tuner
  image is written (t = 65.6), changing with every retune. Which of the pair is I
  cannot be read off the capture.
- **Packets** all 512 bytes (USB_SYSCTL bit 3, full-packet mode, was set
  at t = 56.675); median spacing 134.9 µs.
- **Rate.** In analyzer time: 4,137,181 B/s over the whole stream, 8,104
  packets = 4,149,248 B/s in every second without a 9d pause. The
  analyzer's clock is not calibrated against the bus here (SOF packets
  are not logged), and one fact says it reads short under the bulk load:
  the host's control transfers, spaced 3.999 ms (median of 214) before
  the polling began, are spaced 3.949 ms (median of 3,158) during it.
  Taking the host's spacing as unchanged, the uninterrupted rate is
  4,097,369 B/s = **2 × 2.0487 MB/s: 2.048 Msps of 8-bit pairs, which is
  rsamp_ratio's 8.192 MHz / 4**, the DAB rate. Uncorrected it is
  2 × 2.0746.

### 9g. Against our bench

- **Refuted as the switch:** page 0 0x07 bit 7 (the "floodgate"). The
  driver never writes 0x07 (power-on 0x3C); its stream is real samples,
  the 0x07 path only the FIFO replay.
- **Not needed by the driver:** the documented zero-IF / low-IF input
  bits — AD_EN (page 0 0x08, power-on 0x0D) and en_bbin (page 1 0xB1,
  power-on 0x1F) are never written; the tuner image (the replay needed
  it, step 3) comes 5.2 s after the stream; enable_PID is set to **0**
  ("No Output"), not 1.
- **Matches:** rsamp_ratio 0xE10000 (step 7's DAB example, with
  cfreq_off_ratio's nibble of 0x9F at 0 where step 7 kept F); Spec_inv =
  1 with En_aci = 0 (page 1 0x15 = 0x01); pset_iffreq for a low IF
  (3.570 MHz, not step 7's 7.326 or low-if's 4.57); page 1 0x01 bit 2 as
  a stream stop (one of the 28 throttle bits, 9d); the demodulator reset
  pulse before configuration (our restart probe's DEMOD_CTL bit 5).
- **Registers the driver writes that our single-write scans also wrote**
  (each alone, one at a time, from the zero-IF baseline in the byte scan
  and from five states in the bit scans): every one of 9b and 9c —
  including page 0 0x06 = 0x80 itself (byte scan, zero-IF: 0 streams; in
  the held-open bit scans bit 7 throttled or stopped the replay).
- **Written by the driver, in no baseline of ours:** page 1 0x16–0x18
  = 0, page 1 0x3E = 0, the 20 registers page 1 0x1C–0x2F, page 0 0x17
  and 0x19, page 1 0x93–0x94, page 0 0x61 = 0x60 together with page 0
  0x06 = 0x80, page 1 0xC7–0xCE and page 0 0x11 (after the start). The
  stream needs, on this evidence, a **combination** — the single-write
  hypothesis's failure (step 3) is consistent with it.
- **Written by our baselines, never by the driver:** page 0 0x07, page 0
  0x08, page 1 0xB1, page 1 0x11, the AGC enables of page 1 0x04 (the
  driver writes 0x04 only after the start).

### 9h. The bench test that would confirm it (NOT run)

From a power cycle, every write read back, the bulk endpoint polled from
before step 1 with the step-3 detector (period score, entropy, I/Q
moments) and 10 × 100 ms windows after each numbered step:

1. USB: USB_SYSCTL ← 0x09; USB_EPA_MAXPKT ← 0x0200; USB_EPA_CTL ← 0x0210
   then ← 0; CLEAR_FEATURE(ENDPOINT_HALT) on 0x81.
2. DEMOD_CTL ← 0xA0, ← 0x80, ← 0xA0; wait 65 ms; ← 0xA8; ← 0xE8.
3. 9b items 6–21 in 9b's order with 9b's values (page 1 0x3E–0x3F, 0x15
   = 0, 0x16–0x17, 0x18, 0x19–0x1B = 38 11 12, 0x15 = 1, 0x9F–0xA2 =
   03 84 00 00, 0x1C–0x2F, page 0 0x17, 0x18, 0x19, page 1 0x92, 0x93,
   0x94, page 0 0x61 = 0x60, 0x20 = 0x40). Expected: 0 bytes.
4. Page 0 0x06 ← 0x80. Expected, by the capture: bytes within ~1 ms;
   ≈ 4.1 MB/s; byte values 124–128 around 126.5 (tuner not programmed);
   period score far below 1 (not the replay); 512-byte packets.
5. Page 1 0x01 ← 0x14, then ← 0x10: the stream stops for the interval
   and resumes.
6. Rate tracking: rsamp_ratio at the 2.4 Msps value (×4: 9.6 MHz); the
   rate should move to 2 × rate.
7. Signal: the tuner image of step 4 tuned to the tone, IF 3.570 MHz
   (or pset_iffreq re-set for step 4's IF): the variance rises and a
   spectral line moves with the tone.
8. Minimisation: from a power cycle, steps 1–4 with one item of 9b
   removed at a time (and page 0 0x06 = 0x80 directly after step 2) —
   which writes the stream needs.
9. Separately: SEL_DIV 011 against the tone at 176.64 MHz (LO 180.21 MHz
   by Nint 100, SDM 7646, R16 bit 4 = 1) to confirm /16.

### Evidence kept outside the repository

`~/mimosdr-captures/rtl2832u-windows/`: `capture-1.log` (the capture),
`capture-1.summary.txt`, `control.tsv` (4,401 transfers, raw setup and
data), `control-decoded.tsv` (each decoded), `tuner-writes.txt` (tuner
bursts with the PLL words), `bulk.tsv` (954,067 packets: time, length,
prefix), `ep1-per-10ms.tsv`, `bulk-stats.txt`, `bulk-gaps.txt`,
`pass1-stats.txt` and the four scripts.

## Step 8 — zero-IF input, tuner tuned, path held open — `data/step8-scan-zero-if-open.jsonl` — `harness scan --baseline zero-if-open` — COMPLETE, 0 candidates carry the tone

Hypothesis (in the record's header, 2026-10-09): every zero-IF scan so
far (step 3) ran with en_bbin = 1 but without the tuner configured and
without the output path open; the 45 MB/s first-in-first-out (FIFO)
memory replay (page 0 0x07 bit 7) only appears with the tuner image
written and the I²C repeater open. The combination — zero-IF baseband
input (§9.1, §9.3) + tuner configured and tuned + output path held open —
had never been scanned. A single control may route the baseband into the
FIFO in that state: the 3,760-byte period would break, or the rate would
move toward 2 × 2.4 MB/s.

### The baseline `zero-if-open` (`examples/harness/baselines.rs`), itemised

A power cycle, then in order; every demodulator write a read-modify-write
read back, every USB/system write read back (all verified in the header's
`baseline_setup`):

| item | write | document | read back |
|---|---|---|---|
| DemodCtl | DEMOD_CTL \|= PLL \| ADC_I \| ADC_Q \| reset release | Table 15 | 0x20 → 0xE8 |
| TunerPower | GPIO3 output, enabled, low; 50 ms | Tables 16–19 | GPO 0x18 → 0x10 |
| IicRepeat | page 1 0x01[3] = 1 | Table 3; findings 2 | 0x10 → 0x18 |
| AdEn | page 0 0x08[7:6] = 11 (AD_EN_reg, AD_EN_reg1) | Table 4, §9.1 zero-IF input | 0x0D → 0xCD |
| EnBbin | page 1 0xB1[0] = 1 en_bbin | Table 4, §9.1 (§9.2: DC cancellation and IQ compensation on with it) | 0x1F (already set) |
| IfFreqZero | pset_iffreq = 0 (page 1 0x19–0x1B) | Table 6, §9.3 zero-IF example | `35 D7 5D` → `00 00 00` |
| AgcLoops, Dagc, AgcPolarity | as low-if | Tables 2, 10; findings 4b | unchanged (0xC0, 0x07, 0xFC) |
| Rsamp | rsamp_ratio = 0x3000000 (2.4 Msps) | §9.4, Table 7 | `F3 26 66 64` → `FC 00 00 00` |
| TunerImage | low-if's image: vagc configuration, LO 304.57 MHz (300 MHz RF at the tuner's 4.57 MHz IF, high side) | findings 4a–4e | R5–R15 read back, R2 in 70–127 |
| EpaMaxPkt, EpaCfg, EpaFifoReset, Dma | the bulk endpoint as the zero-IF baseline sets it | Tables 30–33 | as written |
| OpenPath | page 0 0x07 \|= 0x80 | step 3 | 0x3C → 0xBC |

Not written: Spec_inv / En_aci (page 1 0x15, the zero-IF baseline does
not write them), the Table 5 DC/IQ enables (default on with en_bbin),
opt_adc_iq. The tuner stays a low-IF part, so with the down-converter
(DDC) at 0 Hz the tone reaches the analogue-to-digital converter at the
tuner's IF, off the DDC's centre; the tone test therefore steps it across
RF = LO − 4.57 MHz + {0, ±0.2, ±0.5, ±1.0} MHz and RF = LO, and looks for
**any** spectral line that rises over the tone-off reference and moves
between tone frequencies (`tone_lines`), not one expected bin.

### Solo (header): the replay, unchanged

| state | bytes/s (10 × 100 ms) | period score |
|---|---|---|
| zero-if-open, tone off | 44.44–44.61 MB/s | 1.000 every window |
| tone at each of the 8 keyed states | ~37.0 MB/s (the transmitter shares the host's USB); one run of 34.6–36.1 MB/s at the LO | 1.000; 0.9991–0.9999 in 7 windows |
| tone off again | 44.44–44.64 MB/s | 1.000 |

Positive control: `if_agc_val` (MSB-first) 8191 → −449 with the tone at
300 MHz, the analogue AGC locking (aagc_lock 0 → 1). Tone lines in the
solo sweep: 0 tone states beat the null (complex and real), no line moves
with the tone. No solo window was non-replay, so the scan ran.

**Why the scan keeps the tone keyed** (header `scan_tone`, from
`data/step8-fifo-watch.json` and `data/step8-fifo-watch-lowif-open.json`,
`harness fifo-watch`): in the held-open states the only new bytes that
ever enter the FIFO memory are 4 bytes at one fixed position of each
188-byte block, rewritten in bursts ~2.5–3 s apart, and only while the
tone is on. A bit that changed what this signal-driven writer writes
would show only with a signal present; so the tone stays keyed at 300 MHz
(txvga 10, backoff 0) for every candidate, and the thresholds come from
the tone-on solo windows (rate band 29.6–44.5 MB/s, entropy 7.892–7.992
bits, period minimum 0.99, stream band 2.4–9.6 MB/s).

### The scan: 10,240 candidates, 0 carry the tone

Detector and flow check as in steps 3, 6 and 7 (with the recheck rule of
step 7 from the start: a rate-only flag is measured once more with the
flip in place). Eleven chunks; each keyed the tone after its setup
(`scan_tone` line) and unkeyed it at the end.

| | count |
|---|---|
| interesting (confirmed) | **1**: p0 0x06 b4 (0x00 → 0x10), 5.59 MB/s, recheck 6.80 MB/s — in the 2.4–9.6 MB/s stream band, period intact |
| period score < 0.99 | **0 of 10,224** scorable windows (16 windows too short to score: the replay stopped) |
| period score 0.9999 (rounded) | 269 windows, the rest 1.000; consistent with the tone-driven 4-byte rewrite landing inside a window (not tested further) |
| entropy | 7.9368–7.9489 bits |
| zero-byte windows | 0 |
| rate changed (throttle/stop), not interesting | 20: p0 0x06 b5 b7, p0 0x07 b6 b7, p0 0x0A b3 b7, p0 0xB5 b3 (0.03–0.44 MB/s); p4 0x06 b5 b7, p4 0x07 b0–b6, p4 0x0A b3 b7 (0.03–0.41 MB/s); p2 0x9F b7 (28.8 MB/s) and p4 0x0E b0 (19.2 MB/s), single windows |
| signature changed | 0 |
| held flow lost before a candidate | 0 |
| wedged, recovered by a power cycle | 6 (p0/p4 0x20 b4, 0xA7 b5 b6 — the step-7 set) |
| restore not verified, not wedged | 341: 340 on page 3 (the live readbacks), 1 on p4 0x10 b1 |
| detector check | 0 transfer errors, 0 halts |

The page-4 mirror of the interesting bit, p4 0x06 b4, read 32.96 MB/s
(period 1.000) in its window — not flagged.

### The confirmation — `data/step8-confirm-zeroifopen-p0-06-10.json` — `harness confirm-write 0 0x06 0x10 --baseline zero-if-open`

| test | result |
|---|---|
| rate tracking (rsamp 2.4 / 1.2 / 2.048 Msps) | 42.40 / 42.57 / 42.46 MB/s = 8.83 / 17.74 / 10.37 × 2 × rate — **no tracking**; period 3,760 (20 × 188 B), match 1.000 at every rate |
| tone states (10 × 100 ms each) | tone off 42.75 MB/s; five of the eight keyed states, and "tone off again" (2.3 → 42.3 MB/s), hold runs of windows at 1.9–13.8 MB/s amid 35.6–42.3 MB/s: an intermittent throttle present with the tone off as well as on (the scan's 5.59 MB/s window caught one) |
| content in every state | period 3,760 (match 1.000); whole-capture match 0.9955–0.9993; I/Q means 125.28 / 127.31, variances 5484 / 5669 (unsigned) — identical across all ten states to 3 decimals |
| tone lines | complex: null rise 0.22 dB, top rise in any tone state 1.89 dB, **0 tone states beat the null + 6 dB**, no line moves with the tone; real: null 0.51 dB, 0 beat it |
| minimisation (16 items) | needed for any bytes: **DemodCtl and OpenPath only** (as in step 7); without Rsamp the replay runs at 21.4 MB/s; every other item removed leaves 42.1–43.0 MB/s, period 3,760 |

**No candidate carries the tone; none tracks rsamp_ratio.** p0 0x06 bit 4
is the step-7 observation again: it runs the replay without the tuner
image or the I²C repeater, and its rate varies; the content is the replay.

### Replay

```
SRC='--source-cmd "cargo run --release --example tone_source -- --cabled"'
cargo run --release -p sdr-rtl --example harness -- fifo-watch --baseline zero-if-open $SRC                              # data/step8-fifo-watch.json
cargo run --release -p sdr-rtl --example harness -- scan --baseline zero-if-open --limit 1200 $SRC   # repeat to 10240 (an interesting candidate ends its chunk with the confirmation)
cargo run --release -p sdr-rtl --example harness -- confirm-write 0 0x06 0x10 --baseline zero-if-open $SRC
```

## Step 7 — the datasheet's DAB example with the back end off and the path held open — `data/step7-scan-dab-open.jsonl` — `harness scan --baseline dab-open` — COMPLETE, 0 candidates carry the tone

Hypothesis (the project owner's, 2026-10-09; in the record's header):
the document configures a "DAB mode" only in its pset_iffreq and
rsamp_ratio examples (§9.3 p. 19: −1066988 = 0x2FB814; §9.4 p. 20:
14745600 = 0xE10000, f_symbol 8.192 MHz = 4 × 2.048 MHz). With the front
half at that example, the DVB-T back end disabled through its documented
enables and the output held open (page 0 0x07 bit 7), a single control
may route the baseband samples (Figure 1: between the LPFs and the FFT)
into the packetiser/FIFO.

### The baseline `dab-open` (`examples/harness/baselines.rs`), itemised

The low-if-open items in order, three with dab-open values, then the
back-end items, then OpenPath; every write a read-modify-write read back
(all 21 verified in the header):

| item | write | document | read back |
|---|---|---|---|
| IfFreq | pset_iffreq = −1066988 = 0x2FB814 (f_IF_D = 1066988 / 2²² × 28.8 MHz = 7.32643 MHz) | §9.3 DAB example | `35 D7 5D` → `2F B8 14` |
| Rsamp | rsamp_ratio = 0xE10000 (f_symbol 8.192 MHz) | §9.4 DAB example, Table 7 | `F3 26 66 64` → `F3 84 00 00` (0xE10000 << 2; the high nibble of 0x9F is cfreq_off_ratio's) |
| Dagc | en_dagc page 1 0x11[0] = 0 | Table 10 "0: Disable DAGC" | 0x07 → 0x06 |
| AciOff | En_aci page 1 0x15[1] = 0 | Table 6 "0: Disable adjacent channel rejection" — **ambiguous placement**: §9.3 puts the canceller in the DDC (the front half); turned off as an itemised choice | 0x07 → 0x05 |
| CciOff | En_cci page 1 0x40[1] = 0 | Table 8 "0: Disable CCI cancellation" (§9.5) | 0x4F → 0x4D |
| IncOff | en_inc page 1 0x5D[0] = 0 | Table 9 "0: Disable impulse noise cancellation" (§9.6) | 0x33 → 0x32 |
| AutoScanOff | dis_auto_scan page 1 0x5F[0] = 1 | Table 11 "1: Disable auto mode detection" (§9.8; inverted polarity, as printed) | 0x40 → 0x41 |
| PidPassAll | mode_PID page 0 0x61[6] = 1, en_PID[31:0] 0x62–0x65 = 0 | Table 13 / Fig. 7: no individual filter enabled, XOR with mode_PID passes all (§9.11 "the default register setting will let all PID transport streams pass through"); the document has no other "filter off" | already so (0xE0, 0, 0, 0, 0) |
| OpenPath | page 0 0x07 \|= 0x80 | this derivation's finding (step 3) | 0x3C → 0xBC |

Not changed, with the reason (in the header): **enable_PID** page 0
0x61[7] stays 1 — Table 13: "Set 1 to Enable Output of PID Filter / Set
0 No Output", the output gate the held path needs (measured, below);
err_pass stays 1 (not an enable); pset_mode_gi left as read (a preset for
auto-detection off; Table 11 prints default 3, the chip reads 0, 1C);
cfreq_off_ratio stays 0xAEBAF (§9.9 says it "must be set according to
the sampling frequency" but the DAB example gives no value); the AGC
loops stay on (the tuner's VGA is closed through them, 4b); the Table 5
DC/IQ blocks are zero-IF blocks already bypassed by en_bbin = 0 (§9.2).
The tuner keeps low-if's LO (304.57 MHz, high side), so the DDC centre
is RF 304.570 − 7.326 = **297.2436 MHz**; the controls put the tone
there (+200 kHz, −300 kHz).

### Solo (header): the replay, unchanged

| state | bytes/s (10 × 100 ms) | period score | entropy | I/Q unsigned mean / var | two's complement mean / var |
|---|---|---|---|---|---|
| dab-open, tone off | 44.41–44.58 MB/s | 1.000 every window | 7.9460 | 124.98 / 127.61; 5537 / 5649 | 1.20 / −0.26; 5401 / 5213 |
| tone on at 297.2436 MHz | 36.96–37.18 MB/s (the transmitter shares the host's USB) | 1.000 | 7.9460 | the same to 3 decimals | the same |
| enable_PID = 0 ("No Output") | 44.97 MB in 10 windows: **no effect on the replay** | 1.000 | 7.946 | the same | the same |

Positive control: `if_agc_val` 8191 → −1008 with the tone at the DDC
centre RF. No solo window was non-replay, so the scan ran.

### The scan: 10,240 candidates, 0 carry the tone

Detector and flow check as in steps 3 and 6, thresholds from this solo;
interesting additionally when bytes/s is within 20 % of 4.096, 8.192,
16.384 or 32.768 MB/s (2 × 2.048 / 2 × 8.192 MHz, 8- or 16-bit), plus
the stream band 8.19–32.77 MB/s (½–2 × 2 × 8.192 MHz). Each interesting
candidate ended its chunk with a full `confirm-write` (rates 8.192 /
4.096 / 16.384 Msps = the DAB ratio, ×2, ×½; tone off, DDC centre RF
+200 kHz, −300 kHz, off; statistics; minimisation of the 22 items).
12 chunks.

**Detector revision (2026-10-09, after line 301, a `detector_revision`
line in the record):** the 32.768 MB/s band reaches 39.3 MB/s, and one
slow window of the unchanged replay fell into it (p0 0x25 b4: 36.6 MB/s;
its confirmation read 45.0 MB/s at every rate). From then on a window
flagged by rate alone (period intact) is measured once more with the flip
in place, and is interesting only if that second window is flagged too;
a period break would count at once. Eight flags were cleared this way,
each first window being the replay (period 1.000) running down to 0 B:
p1 0x04 b7 (12.9 MB/s), p1 0x94 b0 (8.9), p4 0x01 b2–b7 (16.3–31.3).

| | count |
|---|---|
| interesting (all confirmed) | 5: p0 0x01 b1, p0 0x06 b4, p0 0x25 b4 (before the revision), p1 0xC7 b5, p4 0x06 b4 |
| period score < 1.000 | **0 of 10,225** scorable windows |
| entropy | 7.9368–7.9483 bits |
| first windows within 20 % of 2 × 8.192 MB/s | 4 (p4 0x01 b2, b4, b5, b6) — the replay (period 1.000) on its way to 0 B, cleared by the recheck |
| zero-byte windows | 0 |
| rate changed (throttle/stop) | 38 (the step-3 set, plus p1 0x04 b5 b7, p1 0x94 b0, p1 0xC7 b5, p4 0x01 b2–b7 mirroring page 0 0x01) |
| held flow lost before a candidate | 10, all restored (9 by a power cycle) |
| wedged, recovered | 6 (p0/p4 0x20 b4, 0xA7 b5 b6) |
| detector check | 0 halts, 1 transfer error |

The five confirmations (`data/step3-confirm-dabopen-p{page}-{offset}-{value}.json`):

| candidate | bytes/s at 8.192 / 4.096 / 16.384 Msps (× 2 × rate) | period, every state | tone off / +200 kHz / −300 kHz / off | needed (minimisation) |
|---|---|---|---|---|
| p0 0x01 = 0x06 (b1) | 1.63 / 7.71 / 3.39 MB/s (0.10 / 0.94 / 0.10) | 3,760 (match 1.000) | 0.03 / **37.58** / **37.56** / 0.05 MB/s | DemodCtl, IicRepeat, TunerImage, OpenPath |
| p0 0x06 = 0x10 (b4) | 23.86 / 24.90 / 22.91 (1.46 / 3.04 / 0.70) | 3,760 | 25.03 / 34.68 / 33.40 / 24.71 | **DemodCtl, OpenPath only** |
| p0 0x25 = 0x10 (b4) | 44.99 / 45.01 / 45.03 — the held replay | 3,760 | 45.13 / 37.70 / 37.77 / 44.97 | DemodCtl, IicRepeat, TunerImage, OpenPath |
| p1 0xC7 = 0x38 (b5) | 19.37 / 31.27 / 19.82 (1.18 / 3.82 / 0.61) | 3,760 | 19.40 / 33.77 / 32.93 / 19.39 | DemodCtl, IicRepeat, TunerImage, OpenPath |
| p4 0x06 = 0x10 (b4) | 26.27 / 27.05 / 24.96 (1.60 / 3.30 / 0.76) | 3,760 | 24.66 / 34.01 / 35.16 / 26.41 | **DemodCtl, OpenPath only** |

In every state of every confirmation the complex spectrum's top peaks
are the replay's own fixed bins (−0.2517, 0.3723, 0.1733, −0.1155
cycles/sample, 10–12 dB), identical with the tone off and on; nothing
at the expected +0.0244 / −0.0366 (offset / 8.192 Msps, either sign).
**No candidate carries the tone.** None tracks rsamp_ratio.

Two side observations, both about the flow of the replay, not its
content:

- **p0 0x01 bit 1 gates the replay by signal presence**: 30–50 kB/s with
  the tone off, the full 37.6 MB/s with the tone on at either offset
  (repeated off → on → on → off). The content stays the 3,760-byte
  replay. The register's other bits only throttle (b0) or nothing.
- **p0 0x06 bit 4** (and its page-4 alias) runs the replay without the
  tuner image or the I²C repeater (needed: DemodCtl and OpenPath only),
  where page 0 0x07 bit 7 alone needed the tuner programmed (step 3).
  With the tone on, its rate rises (25 → 34 MB/s).

### Replay

```
SRC='--source-cmd "cargo run --release --example tone_source -- --cabled"'
cargo run --release -p sdr-rtl --example harness -- scan --baseline dab-open --limit 0 $SRC      # header: definition, solo, enable_PID = 0, thresholds
cargo run --release -p sdr-rtl --example harness -- scan --baseline dab-open --limit 1200 $SRC   # repeat to 10240 (an interesting candidate ends its chunk with the confirmation)
cargo run --release -p sdr-rtl --example harness -- confirm-write 0 0x01 0x06 --baseline dab-open $SRC
```

## Step 6 — the spaces the firmware opens: pages 5–15 and the rest of external data — `data/step6-survey.json`, `data/step6-scan-p5-15.jsonl` — `harness survey`, `harness scan --baseline low-if-open --pages 5-15 --offset-from 1 --record step6-scan-p5-15.jsonl`

Both steps start from the low-if-open held state (low-if + page 0 0x07
bit 7, the 3,760-byte replay running) and use only the facts of step 5c:
block 0 reaches pages 0–15 (page written to register 0x00 on a change);
blocks 1/2 read any X: address, page nibble 1 straight; data stages ≤ 64
bytes.

### 6a. Read-only survey — `data/step6-survey.json` (run twice; the classes agree)

No writes. Pages 0–15 read byte by byte (register 0x00 first), twice,
≥ 100 ms apart; X: 0x2000–0x21FF and 0x3000–0x30FF (references) plus the
four ranges in 64-byte reads, twice. No read failed or wedged; one power
session. **The 8051's data RAM (X:4000–X:41FF) is recorded by class
only**: it holds the USB string descriptors built from the EEPROM, the
serial number among them (a first run stored those bytes; that record
was deleted and the step changed before the run that is kept).

Demodulator pages 5–15 (of 255 offsets, register 0x00 excluded from the
comparisons):

| page | reg 0x00 | non-trivial (not 0x00/0xFF) | differ from page 0 | live (changed between passes; 2nd run) | closest page (equal offsets) |
|---|---|---|---|---|---|
| 5 | 5 | 32 | 165 | 50 | 3 (226) |
| 6 | 6 | 37 | 167 | 46 | 13 (212) |
| 7 | 7 | 35 | 166 | 46 | 3 (227) |
| 8 | 8 | 148 | **1** (register 0x00 only) | 0 | **0 (255 of 255)** |
| 9 | 9 | 190 | 227 | 0 | **1 (233)** |
| 10 | 10 | 215 | 239 | 0 | **2 (242)** |
| 11 | 11 | 32 | 168 | 39 | 3 (213) |
| 12 | 12 | 6 | 150 | 0 | **4 (255 of 255)** |
| 13 | 13 | 34 | 171 | 39 | 5 (212) |
| 14 | 14 | 36 | 164 | 41 | 3 (227) |
| 15 | 15 | 37 | 171 | 39 | 3 (212) |

So reads of pages 8, 9, 10, 12 return pages 0, 1, 2, 4 (9 and 10 differ
from 1 and 2 at their live offsets); pages 5–7, 11, 13–15 read like page
3, the live readback page, each with 39–50 live offsets in 0x05–0x79
(the same block every time). Register 0x00 reads the page number on
every page, both passes.

External data (page nibble 1, 64-byte chunks):

| range | class |
|---|---|
| X:2200–X:2EFF | all zero |
| X:2F00–X:2F3F | static, a few non-zero bytes |
| X:2F40–X:2F7F | all zero |
| X:2F80–X:2FBF | **live** (two bytes at +0x16/+0x17 change between passes) |
| X:2FC0–X:2FFF | static, one non-zero byte |
| X:3100–X:3FFF | all zero |
| X:4000–X:41FF | static non-zero: the 8051's data RAM (bytes not recorded, above) |
| X:4200–X:FBFF | all zero (744 chunks) |
| X:FC00–X:FC7F | static non-zero (the IR receive buffer's range, step 5) |
| X:FC80–X:FCFF | all zero |
| X:FD00–X:FD3F | **live** (bytes +0x01, +0x0A, +0x0B; the IR registers' range) |
| X:FD40–X:FDFF | all zero |

No chunk is a byte-for-byte mirror of another or of the USB/system
blocks. The only live hardware outside the scanned blocks is 64 bytes at
X:2F80 and the IR block at X:FD00.

### 6b. Single-bit scan of pages 5–15 with the path held open — `data/step6-scan-p5-15.jsonl` — COMPLETE, 0 interesting

Candidate space: pages 5–15 × offsets 0x01–0xFF × bits 0–7 = 22,440,
**register 0x00 excluded on every page** (the page-select register: the
firmware writes the page there and caches it; writing it would
desynchronise the cache; stated in the header). Detector, thresholds
and the held-flow check as in the step-3 open scan, re-measured for
this record: held state alone 44.5 MB/s, period score 1.000, entropy
7.9446 bits; with the tone on 37.0 MB/s, the same signature, `if_agc_val`
8191 → −889. Interesting = period score < 0.99 (≥ 15,040 B) or 2.4–9.6
MB/s; each would have ended its chunk with a full `confirm-write`.
19 chunks of ≤ 1,200, ~4.6 min each.

| | count |
|---|---|
| interesting | **0** — no confirmation was triggered |
| period score < 1.000 | 0 of 22,423 scorable windows |
| bytes/s 2.4–9.6 MB/s | 0 |
| entropy | 7.9357–7.9510 bits (no "signature changed") |
| rate changed (throttle/stop, periodic where scored) | 46: p5 0x01 b0 b1 b3–b7, 0xD7 b0; p8 0x01 b0, 0x06 b5 b7, 0x07 b6 b7, 0x0A b3 b7, 0x0E b0, 0xB5 b3; p9 0x01 b2, 0x09 b7, 0x0C b6, 0xD7 b0; p12 0x01 b0 b3, 0x06 b5 b7, 0x07 b0–b6, 0x0A b3 b7, 0x0E b0; p13 0x01 b0 b1 b3–b7, 0xD7 b0; p14 0x20 b7; p15 0xF2 b3, 0xF5 b2 |
| zero bytes in the window | 2: p8 0xB5 b3 (0x0A → 0x02, one transfer ended 0xE00002ED — page 0 0xB5 b3's behaviour exactly) and p12 0x01 b3 |
| held flow lost before a candidate | 21 (after p5/p13 0x01, p8 0xB5, p12 0x01/0x02, p14 0x0B, p15 0xF1): 2 restored by the reapply, 19 by a power cycle; every candidate ran with the flow verified |
| held state disturbed, reapplied | p5 and p13: 0x01 (IIC_repeat), 0x15 (Spec_inv), 0x19–0x1B (pset_iffreq), 0x9F (rsamp_ratio) — page 1's registers; p8 and p12: 0x10 (soft reset), 0x07, 0x08 (AD_EN) — page 0's |
| wedged, recovered | 6: p8 and p12 at 0x20 b4, 0xA7 b5, 0xA7 b6 — page 0's three |
| restore not verified (not wedged) | 1,361, on pages 5–7, 11, 13–15 (the live readback-like pages), 1 on page 12 |
| detector check | 0 halts; 1 transfer error (p8 0xB5 b3) |

**Writes alias too:** a write to page 8 or 12 acts on page 0 (its soft
reset, its two wedges, its stoppers 0x07, 0x0A, 0x0E, 0xB5 recur bit for
bit), pages 5, 9 and 13 act on page 1 (IIC_repeat, Spec_inv,
pset_iffreq, rsamp_ratio, and page 1's stoppers 0x01 b2, 0x09 b7, 0x0C
b6, 0xD7 b0); on pages 5, 12, 13 the restore writes back the aliased
read value (page 3-like, page 4-like), which is why those disturb the
held state. Pages 6, 7, 10, 11, 14, 15 showed three single-bit throttles
(p14 0x20 b7, p15 0xF2 b3, 0xF5 b2) and nothing else.

**No candidate carries the tone, or changes the content at all.** Pages
5–15 add no data-path control beyond the registers pages 0–4 already
showed; every window that could be scored was the FIFO's 3,760-byte
replay. The scan ran with the source unkeyed; no candidate was
interesting, so none reached the tone test.

### Replay

```
cargo run --release -p sdr-rtl --example harness -- survey
SRC='--source-cmd "cargo run --release --example tone_source -- --cabled"'
cargo run --release -p sdr-rtl --example harness -- scan --baseline low-if-open --pages 5-15 --offset-from 1 --record step6-scan-p5-15.jsonl --limit 0 $SRC     # header: held state alone, thresholds
cargo run --release -p sdr-rtl --example harness -- scan --baseline low-if-open --pages 5-15 --offset-from 1 --record step6-scan-p5-15.jsonl --limit 1200 $SRC  # repeat to 22440
```

## Step 5 — the firmware — `data/step5-rom-dump.json` — `harness rom-dump`

Approved by the project owner on 2026-10-09, after ~810,000 single-write
candidates found no raw-sample mode, under one rule: the driver never
contains or uses any part of the firmware. The image and every listing
of it live outside every repository (`~/mimosdr-captures/rtl2832u-rom/`).
This section carries only facts, in words, each with the ROM address it
was read at and its evidence: **code** (read from the disassembly),
**bench** (measured), or both. No byte of the ROM is reproduced here.
The disassembler is ours: `examples/rom51` (a plain MCS-51 opcode table,
recursive descent from the vectors, jump tables of LJMP/AJMP entries
followed, a constant DPTR tracked through straight runs, `--inline
ADDR:N` for routines that read their argument bytes from after the
call). Addresses below are ROM addresses unless written `X:` (the
8051's external-data space, as the code addresses it).

### 5a. The dump

| | |
|---|---|
| request | `GetROMCode` (Table 27): bmRequestType 0xC0, wIndex 0x0400, wValue = address, read-only |
| chunk | 64 bytes. `rom-dump --probe`, fresh power-on: wLength 1, 8, 64 answer in ~1.7 ms; 65 → Stall after ~84 ms, chip still answering; 256 → Stall after ~98 ms, then every request (system block included) times out until a power cycle. In the record: 1, 8, 32, 63, 64 agree byte for byte with one-byte reads at 0x0000, 0x2000, 0x8000; 65 does not |
| passes | the whole 16-bit space three times: A and B in one power session, C after a power cycle |
| **size** | **8,192 bytes (0x0000–0x1FFF)** |
| **SHA-256** | **`f554992f78a44dd5c5a8aed77595a23272f8114b4bbf9766437d5ea4cdea6dd1`** (the harness's own SHA-256, checked against `shasum -a 256`) |
| consistency | the image identical in A, B and C; the whole space A = B; A ≠ C only in 0x2000–0x2FFF |

Why 8 KiB (bench, and code independently):

- 0x0000–0x1FFF is the same in two power sessions; 0x2000–0x2FFF reads
  one byte value repeated 4,096 times, different in each power session,
  and it is not
  the USB block (block 1 at the same wValue reads the documented
  registers in the same session).
- Above it, `GetROMCode` reaches the system block's registers at
  0x3000–0x300B (equal to block 2's reads with the 4 bytes of each 32-bit
  word reversed), reads 0 from 0x4000 to 0xEFFF, and finds 13 non-zero
  bytes at 0xFD03–0xFD17 (the IR registers' documented 0xFD00 range).
  The wIndex page nibble 1–15 changes nothing (all 30 probes equal page 0).
- Code: the firmware checks its own ROM with a CRC-16 (polynomial 0x8005,
  initial value all ones, MSB first) over exactly 0x0000–0x1FFF (range set
  at 0x0B24–0x0B2B, routine 0x1B5A), compared against a constant at
  0x0B1E or a value from the EEPROM (0x0AEF–0x0B1C). The CRC of our image
  equals that constant. The last two bytes of the ROM are its balance
  bytes; 0x1D0C–0x1FFD is an initialisation table and zero fill.
- Coverage: the walk from the vectors reaches 6,737 of the 8,192 bytes as
  instructions; what it does not reach is the inline argument bytes
  after calls to 0x120B, the descriptor/string/test-packet tables
  0x0DD0–0x0F9E, and the tail above.

The dump answers one question about the block itself: `GetROMCode` reads
the 8051's **code** space (the handler at 0x08A1 hands the copy routine
0x19D3 a code-space pointer, 0x08F1), so above 0x1FFF it shows what the
code space decodes there, not external data.

### 5b. Structure (code)

- Vectors: reset → 0x1A64 (clears internal RAM, stack, runs the
  initialisation table at 0x1D0C, then the main routine 0x1C13); external
  interrupt 0 → 0x14D8 (the USB interrupt); timer 0 → 0x1CCE (a tick that
  decrements two software timers). The other vectors are unused.
- Main 0x1C13: timers, serial port and interrupts (0x1C98; the serial
  port is set up but SBUF is never used), the I²C master (0x1C59), the
  EEPROM-driven configuration (0x0A7D), the USB initialisation (0x1895),
  the GPIO set-up (0x1AF0); then the loop at 0x1C22 waits for the
  setup-packet flag and dispatches each control request (0x000E).
- The setup packet is read from X:20F8–X:20FF, each 32-bit word stored
  most-significant byte first: X:20FB bmRequestType, X:20FA bRequest,
  X:20F8/9 wValue high/low, X:20FE/F wIndex high/low, X:20FC/D wLength.
- 0x001B: request type standard → bRequest table (0x003D, 12 entries:
  GET_STATUS, CLEAR_FEATURE, SET_FEATURE, SET_ADDRESS, GET_DESCRIPTOR,
  GET/SET_CONFIGURATION, GET/SET_INTERFACE; SET_DESCRIPTOR and the two
  reserved codes do nothing); vendor → 0x06F7; class → ignored.
- The only fixed I²C address in the firmware is the EEPROM's (0x0FB8,
  0x16F6, 0x1756). The demodulator's address appears nowhere: the host
  supplies it.

### 5c. The vendor dispatcher (code; bench where marked)

0x06F7 takes **wIndex's high byte as the block** (Table 28's nibble [2]),
bounds it below 8 (0x0703; 8 and up → STALL at 0x0A72) and jumps through
an 8-entry table (0x0711). Every handler tests **wIndex bit 4** (Table
28's access nibble) for Set; the low nibble is the page.

| block | handler | what it does |
|---|---|---|
| 0 demodulator | 0x072A, then 0x0788 | I²C access: **slave address = wValue low byte, register = wValue high byte**. Before it, the page (wIndex & 0x0F, so **0–15**) is written to **register 0x00** of that slave, by a one-byte I²C write, whenever it differs from a cached copy (0x0736–0x0769); the cache is invalidated at USB initialisation (0x1917) and after a failed page write (0x075F). Bench: wValue = offset<<8 \| 0x20 (1B/1C); page 3 and page 4 register 0x00 read 3 and 4 and take any value (block scan) — the page register itself |
| 1 USB, 2 system | **one handler**, 0x0925 | a raw external-data window: **wValue is the X: address, any of 0x0000–0xFFFF**; Get copies wLength bytes out (0x13C4), Set copies in (0x15DB). With page nibble 0 the bytes of each aligned 32-bit word are reversed (0x13EE–0x141D), so the host sees little-endian what the 8051 stores most-significant-first; any other page nibble copies straight. **Bench (`rom-dump --xdata-check`, 2026-10-09): block 2 at 0x2144/0x2158/0x2000 reads EPA_CFG/EPA_MAXPKT/USB_SYSCTL exactly as block 1 does, block 1 at 0x3000 reads the system block, and page nibble 1 returns every word's bytes reversed.** So the documented bases 0x2000 and 0x3000 are the 8051's own X: addresses, and the "block" only names a convention |
| 3 tuner | 0x0788 | the same I²C access as block 0 without the page write (so block 3 at address 0x20 reaches the demodulator's current page: bench 2, address 0x10 read through the tuner block) |
| 4 ROM | 0x08A1 | code-space read at wValue; Set → STALL (0x0919) |
| 5 IR | 0x091F | always STALL: this firmware serves no IR request (the IR registers are reachable through blocks 1/2 at X:FC00/FD00) |
| 6 standard I²C | 0x076B | wLength > 16 → STALL; otherwise block 0/3's access with a flag (X:4000 = 1) that makes the I²C write omit the register-address byte (0x12D8) |
| 7 *(undocumented)* | 0x0A3C | Set only (Get → STALL): clears bit 0 of USB register 0x2010 (the bit the USB initialisation sets last, 0x191D, after clearing it at start-up, 0x1CB7), waits wValue timer ticks (0x0A64), then re-runs the whole USB initialisation (0x1895: EEPROM patch list, SIE reset, endpoint 0). A host-commanded disconnect and re-enumeration — **a bench hazard, not a mode** |

Data stages are sent in packets of at most 64 bytes; between packets the
handlers wait on endpoint-0 status (X:210F bit 1) with a 100-tick bound
and STALL at its end (0x07A4–0x0811, 0x08B8–0x0916, 0x0949–0x09C5) —
bench: GetROMCode wLength 65 stalls after ~84 ms. I²C transactions are
split into at most 16 bytes (0x17D4, 0x1985).

### 5d. The registers the firmware writes (code)

USB block, as the host addresses it (32-bit little-endian):

- 0x2000 USB_SYSCTL: SIE reset set, cleared, then DMA enable set
  (0x1898–0x18CF, and again on bus reset 0x1BD7–0x1BE7).
- 0x2008 interrupt status (write-1-to-clear) and 0x200C interrupt enable,
  set to bits 2, 4, 5, 6, 7 (0x1907, 0x1C02). In the handler (0x14F7):
  bit 5 = setup packet; bit 4 → the USB registers re-initialised (0x1BC0;
  a bus reset, by its use); bit 2 → 0x2010 bits 3–4 set, then bit 4
  cleared (0x1524);
  **bit 7 = bulk endpoint A: only acknowledged — 0x2150 is written all
  ones (0x156F–0x157E) and nothing else happens**; bit 6 does the same
  for endpoint 0's 0x2110.
- 0x2010: bit 0 cleared at start-up, set at the end of USB initialisation
  (a connect bit, by its use); bit 2 remote wake-up (SET/CLEAR_FEATURE);
  bit 5 set or cleared by the GPIO set-up from an EEPROM option (0x1B0A,
  0x1B37). Bench: the USB block scan wedged at offset 0x10.
- 0x2014 bit 0: read as the high-speed indicator (0x03D8).
- 0x2018: the device address (SET_ADDRESS writes wValue there, 0x0352).
  Bench: the block scan wedged at offset 0x18.
- **0x201C: the test mode.** SET_FEATURE(TEST_MODE) (0x05C1–0x064E)
  writes 2 for Test_J, 3 for Test_K, 1 for Test_SE0_NAK, and for
  Test_Packet first loads the standard 53-byte test packet (table at
  0x0F6A) into the endpoint-0 buffer, then writes 4; then the firmware
  loops forever (0x064E). This confirms `HAZARD-usb-0x1C.md`'s reading
  (values 1–4 are test modes) and explains its capture: a bare write of 4
  sends whatever the endpoint-0 buffer holds, not the test packet.
- 0x2104–0x2123: endpoint 0's register set, laid out like endpoint A's
  0x2144–0x2163 (0x2104 bit 7 enable; 0x2108 control: handshake, stall;
  0x210C status — bit 1 polled as "IN buffer free"; 0x210C and 0x2110
  written all ones at initialisation; **0x2118 = 64, endpoint 0's max
  packet**; 0x2120 its OUT byte count). Bench: the block scan wedged at
  0x104.
- **Endpoint A**, only in these places:
  - SET_CONFIGURATION(1) (0x03B4) and SET_INTERFACE alternate 0 (0x0445):
    USB_EPA_CTL ← 0x20 (flush) then bit 5 cleared; USB_EPA_MAXPKT ← 512
    at high speed, 64 at full speed; USB_EPA_CFG ← 0xD1 (enabled, bulk,
    IN, endpoint 1); **0x214C and 0x2150 ← all ones** (two registers
    Table 29 calls reserved; endpoint 0's counterparts are 0x210C/0x2110).
  - SET_INTERFACE alternate 1 (0x049C): USB_EPA_MAXPKT ← 940 (5 × 188) and
    USB_EPA_CFG ← 0xB1 (isochronous) — the "ISO mode" of §11.1; the ROM's
    descriptors (0x0DD0) declare no alternate setting 1.
  - SET_FEATURE / CLEAR_FEATURE(ENDPOINT_HALT) on 0x81: USB_EPA_CTL bit 4
    set / cleared (0x0656, 0x06BB) — bench: ClearFeature clears bit 4
    (detector check).
  - **Never referenced: USB_EPA_FIFO_CFG (0x2160), USB_EPA_CTL bit 9
    (FIFO reset) and bit 0 (FIFO valid).** Bench: FIFO_CFG reads 20
    blocks at power-on (1A), the period of the page 0 0x07 replay.

System block (host view; the 8051 sees each word's bytes reversed):

- **DEMOD_CTL (0x3000) is never written** (its byte is not referenced).
- GPIO: GP_CFG0 bit 7 set (0x1AF0); GPIO3 made an output and driven, or
  made an input, by an EEPROM option (0x1AF7–0x1B58); GPIO4 and GPIO0
  pulsed by an EEPROM record (0x0AD4–0x0BA4); DEMOD_CTL_1 (0x300B) from an
  EEPROM option (0x1AFD).
- I²C master: I2CCR (0x3040) ← 0x17 (0x1C59; the "FD10" divisor the
  document says software must write — bench: it reads 0x17 at power-on,
  1A), then I2CMCR bit 31 set and polled until it clears (0x1C63–0x1C95);
  every transaction (0x127C write, 0x0FBA read) loads I2CMFR (0x3050)
  and starts with I2CMCR bit 31.

Others: X:4000–X:40FF is the 8051's data RAM (variables; the request
buffer at X:4090). X:A5A5–X:A5A6 are written on every byte of the ROM CRC
loop (0x1BB1): something outside the documented map (unknown).

### 5e. What feeds the bulk endpoint — the facts

1. **No code moves, inspects or selects stream data.** Endpoint A appears
   only in 5d's list: configuration at SET_CONFIGURATION/SET_INTERFACE,
   the halt bit, and an acknowledged interrupt. There is no mode
   variable, no table of modes and no branch that writes a source or
   format register (code, whole ROM: every MOVX with a constant address
   is in the cross-reference; the only writes to addresses not fixed in
   the code are the host's own requests and the EEPROM patch list
   below). The path demodulator → 188-byte packetiser → FIFO → endpoint A
   is hardware.
2. **The firmware never writes the demodulator on its own** (code): every
   I²C transfer is a host request (blocks 0/3/6) or one EEPROM record's
   single I²C write (0x0B68–0x0B8D, address and data from the EEPROM);
   the page write of block 0 is register 0x00.
3. **Consistent with the bench:** page 0 0x07 bit 7 (the 45 MB/s replay
   of the FIFO's 20 × 188 bytes) and the 28 bits that throttle it are
   demodulator registers no firmware path writes; the replay period is
   USB_EPA_FIFO_CFG's power-on 20 blocks, which no firmware path changes;
   the demodulator's I²C address 0x10 (8-bit 0x20) is the host's choice
   in wValue.
4. **An EEPROM patch list** (0x16D6–0x17BB, run inside the USB
   initialisation at boot and by block 7): at EEPROM (I²C 0xA0) offset
   0x4E, if the byte is 0x5C, the next byte is a length (capped at 178)
   and then come (address high, address low, value) triples, each written
   to that X: address. It is the vendor's way of setting hardware
   registers before enumeration; whether this dongle's EEPROM carries a
   list is not known (its contents were never recorded).

So the raw-sample switch, if the chip has one, is a hardware register,
and the code points at two spaces no scan has covered:

- **Demodulator pages 5–15.** Block 0 masks the page to 4 bits (0x0734)
  and writes it to register 0x00, so wIndex 0x0005–0x000F (Get) and
  0x0015–0x001F (Set) reach pages 5–15 exactly as 0–4 are reached. Pages
  16–255 would need register 0x00 written directly, which desynchronises
  the firmware's page cache until a different page is requested.
- **External-data addresses beyond the scanned ranges.** Blocks 1 and 2
  read and write any X: address; the scans covered 0x2000–0x21FF and
  0x3000–0x30FF only. The rest of 0x2000–0x3FFF, 0x4100–0xFBFF and the IR
  range 0xFC00–0xFDFF are unscanned hardware space.

### 5f. Open

- Why `GetROMCode` above the ROM shows the system registers but one
  repeated, session-dependent value at 0x2000–0x2FFF and 0x3040–0x305F,
  and zeros where the data RAM sits (0x4000).
- Why wLength 256 wedges the chip where 65 only stalls (both end in the
  same 100-tick wait).
- What X:A5A5–X:A5A6, USB 0x214C/0x2150 (beyond "all ones at
  configuration, all ones on endpoint A's interrupt") and the
  non-standard special function register 0x8E written at start-up are.

### Replay

```
cargo run --release -p sdr-rtl --example harness -- rom-dump                  # the dump, data/step5-rom-dump.json; image to ~/mimosdr-captures/rtl2832u-rom/
cargo run --release -p sdr-rtl --example harness -- rom-dump --probe 0:64 0:65 # request lengths (prints only)
cargo run --release -p sdr-rtl --example harness -- rom-dump --xdata-check     # blocks 1/2 as one X: window (prints only)
cargo run --release -p sdr-rtl --example rom51 -- ~/mimosdr-captures/rtl2832u-rom/rtl2832u-rom.bin --inline 0x120B:4 --xref > ~/mimosdr-captures/rtl2832u-rom/listing.txt
```

## Step 4 — the tuner from the documents, measured with a cabled tone: CODE BUILT, BENCH PENDING

The bench gains a signal source: a transmitter's output through a 20 dB
fixed attenuator into the antenna input, nothing radiating, driven by the
harness over the source's stdin (`--source-cmd "<command line>"`; the
protocol is in `examples/harness/source.rs`). Levels are relative only
(txvga nominally 1 dB per step, backoff exact digital scaling down to
about −40 dB); the source's `ready` line names the transmitter and is
never recorded. The source runs in its own process group: Ctrl-C stops
the harness, which unkeys and quits the source on its way out; a dead
harness closes the pipe and the source unkeys on EOF.

### What is written, and why (`examples/harness/tuner.rs`)

- R5–R31: every fixed cell of [R820T2] Table 1-2 as printed (a unit test
  checks every image against the 19 fixed masks); every other field a
  choice from Table 1-3 with its reason in `IMAGE_NOTES`, copied into
  every record. The bring-up front end: all three gain stages manual at
  code 8, detectors off, FILT_CODE 0000 / FILT_BW 00 / HPF 1111 (widest),
  RFMUX 01 (bypass) and RFFILT 00, TF corners 0000, R30 bit 6 at the
  matrix's 1 (Table 1-3 reads it as FILTER_EXT; `--r30b6` tries 0).
- PLL: Nint = 4·Ni2c + Si2c + 13, Nfra = SDM_IN[16:1]/2¹⁶, REF_DIV2 = 1
  (28.8 MHz > 24 MHz, the document's rule; `--ref-div2 0` tests the
  other), and the hypothesis the document implies but never states:
  f_LO = f_ref · 2 · (Nint + Nfra) / div. Nint's range is 13..=268. The
  VCO range (`tuner::VCO_RANGE_HZ`, None until measured) and the divider
  rule are what `pll-sweep` measures; until then every step needs
  `--div CODE`.
- The demodulator in IF mode ([RTL2832U] §9.1: AD_EN_reg1 = 1,
  AD_EN_reg = 0, en_bbin = 0), pset_iffreq for the IF (§9.3), Spec_inv = 1
  for a high-side LO (§9.3), rsamp_ratio 0xC99999 (8 MHz, Table 7),
  en_if_agc = en_rf_agc = 1, aagc_hold = 0, en_dagc = 1, the AGC pins'
  polarity 0 — every write read back. IF 4.57 MHz by default ([R820T]
  Table 4-1 row 5, the 8 MHz channel; `--if` for the others).
- The tuner's I²C paths are step 2's; a 32-byte read after the image is
  written records whether the register file reads back.

### The meter (`examples/harness/meter.rs`)

Every documented readback that could reflect level is read as one
snapshot: `dagc_val` (p3 0x05, Table 10), `if_agc_val` / `rf_agc_val`
(p3 0x59–0x5C, Table 2, decoded in both byte orders since Table 2's
column is headed {LSB, MSB}), `aagc_lock` (p3 0x50), `sfreq_off`,
`cfreq_off` (Table 12), `inc_det_cnt` (Table 9), `Est_kq` / `Est_sin`
(p1 0x66–0x69, Table 5), the tuner's R1–R4 (R2 VCO_INDICATOR, R3
RF_INDICATOR; named, never described), and with `--page3` the whole of
page 3 so an undocumented byte that moves is seen (declared as
`p3:0xNN` if ever used). A reading is 3 snapshots 60 ms apart (1 in the
long sweeps); "moved" = |Δmedian| > 2 × (sum of spreads) + 1. The
documented settings each needs: the two analog loops need their enables
and aagc_hold = 0, dagc_val needs en_dagc; for the IF loop to close
through the tuner the VGA must take its gain from the VAGC pin
(VGA_MODE = 1) and the board must wire the pin — hence the
configurations below.

### The steps, their candidate spaces and records

| step | candidate space (plain order) | record |
|---|---|---|
| `meter` | divider probe: SEL_DIV 0..=7 at `--freq` (documented codes under their divider; 100–111 under each of 1, 2, 4, 8, 16, 32), configs manual and vagc, tone off then on; then the ladder on the responding codes (all four documented ones if none): configs (manual), (vagc, polar 0), (vagc, polar 1), (auto) × source off, txvga {0, 5, 10, 15, 20} × backoff {−20, −12, −6, 0} ascending, off | `data/step4-meter.json`: every snapshot, medians, and per readback: per-level medians, span, up/down steps, reversals, off→top moved |
| `pll-sweep` | SEL_DIV 0..=7 × Nint 13..=268 step 5 (268 included), Nfra 0: R0–R4 at 10 and 60 ms, tone off reading, then per divider hypothesis the tone at the RF that LO predicts | `data/step4-pll-sweep.jsonl`: one line per (code, Nint) with the status bytes and the readbacks off / per hypothesis |
| `tune HZ` | one frequency | `data/step4-tune-<MHz>.json`: PLL words, status, readbacks off / on |
| `gain-ladder` | LNA_GAIN, MIX_GAIN, VGA_CODE 0..=15, one stage at a time, the others at `--lna/--mix/--vga` (8); the meter (`--meter NAME LO HI up\|down`) kept in its window by 6 dB source moves (backoff first), every move logged | `data/step4-gain-ladder.json`: per code the reading, the source, the moves |
| `band-table` | 42..=1002 MHz in `--grid-mhz` (10) steps × RFFILT 0..=3 × RFMUX 0..=3 × TF_NCH 0..=15 × TF_LP 0..=15; staged (16 paths at TF 0000/0000, then the best two × 256) or `--full` (4096); the source re-levelled once per frequency at the bypass path, then fixed | `data/step4-band-table.jsonl`, resumable by (f, rf, mx, n, l) |
| `filter` | ref (0, 00, 1111), FILT_CODE 0..=15, FILT_BW 0..=3, HPF 0..=15, the tone's IF 0.1..=12 MHz in 100 kHz steps; LO fixed; source fixed after one re-level; `--ddc track` moves pset_iffreq 1 MHz below the tone so the demodulator's own filter does not shape the curve (`--ddc fixed` keeps it) | `data/step4-filter.jsonl` |
| `if-choice` | tone below / above the LO × pset_iffreq IF in {3.57, 4.063, 4.07, 4.57, 32/7 (the chip's reset), 0} MHz × Spec_inv 0/1 × the tone's IF 0.25..=12 MHz in 250 kHz steps | `data/step4-if-choice.jsonl` |

Why the staged band table loses nothing it cannot name: the band and
mux fields select a path and the corner codes tune within it; stage A
ranks the 16 paths with both corners at 0000 (highest, least attenuating
for the low-pass), stage B covers every corner code on the best two. A
path that wins only with a corner lowered would be missed; `--full` at
sample frequencies is the check, and the record says which mode ran.

How `if-choice` finds the tone: a readback taken after the demodulator's
channel filter responds only while the tone's IF is within ~4 MHz of the
DDC's IF (and on one side only if Spec_inv / the LO side matter); one
taken before it responds wherever the tuner's IF filter passes the tone.
The readback whose response against the tone's IF has those edges locates
the tone and says where in the chain it is read.

### Commands (from the repository root; the source command is the bench's)

```
SRC='--source-cmd "cargo run --release --example tone_source -- --cabled"'
cargo run --release -p sdr-rtl --example harness -- meter $SRC [--freq 300e6] [--page3]
cargo run --release -p sdr-rtl --example harness -- pll-sweep $SRC [--nint-step 5] [--ref-div2 0]
cargo run --release -p sdr-rtl --example harness -- tune 300e6 $SRC --div CODE
cargo run --release -p sdr-rtl --example harness -- gain-ladder $SRC --div CODE --meter NAME LO HI up|down
cargo run --release -p sdr-rtl --example harness -- band-table $SRC --meter NAME LO HI up|down [--grid-mhz 10] [--full]
cargo run --release -p sdr-rtl --example harness -- filter $SRC --div CODE --meter NAME LO HI up|down [--ddc fixed] [--r30b6 0]
cargo run --release -p sdr-rtl --example harness -- if-choice $SRC --div CODE
```
(`--source-cmd` takes one argument: quote the command line. Every step
accepts `--out PATH` for a scratch record and `--limit N`.)

### Step 4a — the demodulator is clocked from the tuner's clock output: PROVEN — `data/step4-clkout-check.json` — `harness clkout-check`

The first meter run (`data/step4-meter-run1-aborted-at-R15.json`) wrote
the image with R15 = 0x38 (CLK_OUT_ENB = 1, clock output off, chosen on
the reasoning that the demodulator had answered before the tuner was
powered): R5–R14 were accepted, the write of R15 and every USB request
after it failed with `Unknown(3758097133)` = 0xE00002ED
(kIOReturnNotResponding), and the probe's "nothing moved" lines were
read errors. Hypothesis: the demodulator's XI is driven by the tuner's
clock output ([R820T] §6.3 "Share Crystal"; [RTL2832U] §9.10 "oscillator
output should be connected to the XI pin"), and the step-2 observation
only shows that GPIO3 does not switch the tuner's supply on this board.

`clkout-check`, test and null: the whole image with CLK_OUT_ENB = 0 is
written without error and the dongle answers (DEMOD_CTL 0xE8, R0 0x96);
the single write R15 = 0x38 fails with kIOReturnNotResponding and so
does every request after it (the device stays enumerated: 1 listed); a
power cycle (gone in 58 ms, back in 540 ms) restores it. **R15 bit 4 =
0 on this board**; the image's default is now clock output on, with the
reason in `tuner::IMAGE_NOTES`.

Also from this record: a 32-byte tuner read returns R0–R15 and then
R0–R15 again (the read wraps after 16 registers), and R5–R15 read back
exactly as written (`98 90 68 A0 20 90 0F E8 A5 A5 28`); R16–R31 cannot
be verified by read-back. With the clock on and the PLL at the 300 MHz
words, R1 = 0x80, R2 = 0x80 (bit 7 set, VCO_INDICATOR[6:0] = 0 — the
matrix prints bit 7 as 0), R3 = 0xFF, R4 = 0xE0.

### Step 4b — the meter: `if_agc_val` with the IF loop closed through the VAGC pin — `data/step4-meter.json` — `harness meter --txvga 0 --backoff -30 --page3`

Setup: image with the clock output on, no write errors, R5–R15 read back
exactly; SEL_DIV 010 (/8) with Nint 84 for a 304.57 MHz LO. Ladder:
configs (manual), (vagc, polar 0), (vagc, polar 1), (auto) × off, txvga
{0, 5, 10, 15, 20} × backoff {−20, −12, −6, 0}, off; 3 snapshots per
state. The ladder is taken txvga outer, backoff inner, which is not
ascending in level = txvga + backoff, so the record's `analysis`
(computed in ladder order) overstates reversals; the numbers below are
sorted by level.

**Byte order (bench decision):** the AGC bytes read as {0x59, 0x5A} =
{0x1F, 0xFF} or {0xE0, 0x00}. Read MSB-first ({MSB, LSB} like every
other table) those are +8191 and −8192, the two rails of a 14-bit
two's-complement integrator; LSB-first they are −225 and 224. **0x59 is
the MSB; Table 2's {LSB, MSB} column header is wrong.** `if_agc_val` =
s(14,13f) over {0x59 MSB, 0x5A LSB}; the same for `rf_agc_val`.

| config | `if_agc_val` (MSB-first) vs level, sorted | verdict |
|---|---|---|
| manual (VGA_MODE 0) | +8191 at −20…−10 dB, −8192 at −7 dB and above (rf_agc_val identical) | a comparator: the loop's output pin drives nothing, the integrator rails; threshold ≈ −8.5 dB relative |
| vagc, polar 0 | 5000, 3762, 3055, 2921, 2467, 2289, 2380, 2010, 1609, 1289/1511, 909, 600, 439, −205, −539, −749, −2150, −2384, −4031 at −20, −15, −12, −10, −7, −6, −5, −2, −1, 0, 3, 4, 5, 8, 9, 10, 14, 15, 20 dB | **monotonic** (one 91-count inversion at −6/−5 dB, inside the ~100-count reading spread); linear fit −207 counts/dB, zero at +5.4 dB, residuals up to 1000 counts (txvga steps are nominal); `rf_agc_val` 7792 → 60, −187 counts/dB |
| vagc, polar 1 | +8191 everywhere but −8192 at four of the top levels | positive feedback: the wrong polarity |
| auto (LNA/mixer AGC, detectors on) | −8192 throughout; `dagc_val` 52 → 5; R3 255 → 239 → 191 → 143 → 95 … 165, not monotonic in level | the demodulator's loops rail; R3 RF_INDICATOR follows the tuner's own gain state with hysteresis |

So the board wires the demodulator's AGC_IF sigma-delta output to the
tuner's VAGC pin with polar_if_agc = 0, and **the meter is
`if_agc_val` (MSB-first) in the vagc configuration**: ≈ −207 counts per
dB (it falls as the signal rises), rails at ±8191 (≈ 79 dB if the slope
holds), reading spread ≈ 100 counts (≈ 0.5 dB), with spreads of
1200–1500 counts in three readings taken 150 ms after a level change
(the loop still settling: gain sweeps settle longer, `--settle`).
`dagc_val` flips between 0 and 255 from snapshot to snapshot in every
configuration and is not a meter. R1 went 0x80 → 0xB4/0xB5 after the
first retune and stayed. R2 VCO_INDICATOR: 0 at Nint 21 and 42, 92–93
at Nint 84, 63 at Nint 169 — it changes with the PLL words, a candidate
lock indicator for the sweep.

The divider probe at −30 dB (below the manual comparator's threshold)
saw a readback move only at SEL_DIV 010 /8; that is not evidence against
the other codes, which `pll-sweep` covers at +10 dB in the vagc
configuration.

Gain sweeps use `--meter if_agc_val_msb_first -3000 3000 down --config
vagc` (the window is ±14 dB about the zero).

### Step 4c — PLL: VCO 1.67–3.69 GHz, the documented dividers confirmed, R2 is a lock indicator — `data/step4-pll-sweep.jsonl` — `harness pll-sweep --config vagc --txvga 10 --backoff 0 --snapshots 1 --settle 300`

416 settings: SEL_DIV 0..=7 × Nint 13..=268 step 5, Nfra 0, REF_DIV2 = 1
(f_ref 14.4 MHz); per setting R0–R4 at 10 and 60 ms (identical in all
416), a tone-off reading, then the tone at the RF each divider hypothesis
predicts (RF = LO − 4.57 MHz), one snapshot each, source at +10 dB
relative where the meter reads ≈ −1000 in band. "Received" =
`if_agc_val` (MSB-first) well off the 8191 rail; full strength is
−200…−1350, values of 2000–6000 are partial.

| code | hypothesis | received at Nint (LO MHz) | reading |
|---|---|---|---|
| 000 | /2 | 63–93 (907–1339), fading at 98, 103 (1411, 1483: RF outside the tuner's range) | −412…−3, then 270, 633 |
| 001 | /4 | 63–128 (454–922); 58 (418) marginal 2345 | −688…−99 |
| 010 | /8 | 58–128 (209–461) | −768…−1241 |
| 011 | /1 | nothing in the lockable range (the tone would be at 1.8–3.7 GHz; 12 settings above 6 GHz skipped) | untestable by the tone |
| 100 | /32 | 53–133 (48–120) at full strength; nothing under /1, /2, /4, /8, /16 in that range | −373…−1345 |
| 101, 110, 111 | any of the six | isolated hits only (several near a tone of 655–665 MHz regardless of Nint) | unresolved |

**VCO range:** received from Nint 58 (f_VCO = 28.8 MHz × 58 =
1.670 GHz; marginal with /4, full with /8) to 128 (3.686 GHz); not at
53 (1.526) or 133 (3.830). `tuner::VCO_RANGE_HZ` = (1.70, 3.65) GHz,
30 MHz inside the received bounds. With Nint + Nfra fractional the
documented dividers give LO 209 MHz–3.65 GHz; **42–209 MHz needs an
undescribed code**, and code 100 behaves as /32 (LO 52–115 MHz). The
hypothesis f_LO = f_ref · 2 · (Nint + Nfra) / div holds for /2, /4, /8
(the tone is received exactly where it predicts, across 15 settings per
code).

**R2 VCO_INDICATOR[6:0]** is the same function of Nint for all eight
codes: 63 at Nint 13–18, 0 at 23–58, 70, 77, 83, 87, 91, 95, 103, 108,
112, 116, 120, 122, 125, 127 at 63, 68, …, 128, and 63 from 133 on (/8
alone reads 127 at 133). So 0 and 63 are "out of range" on either side
and 70–127 tracks the VCO's position inside it; bit 7 reads 1 always.
R1 0xB5, R3 0xFF, R4 0xE0 throughout (R1 was 0x80 before the first
retune of a session).

The undescribed codes get a hypothesis-free search next (`lo-find`: the
tone stepped across 2–1300 MHz with the PLL held at Nint 88).

### Step 4d — the undescribed SEL_DIV codes: 100 = /32, 101 = /64, 110 and 111 unusable — `data/step4-lo-find-sel1xx.jsonl` — `harness lo-find --div 4|5|6|7 --config vagc --txvga 10 --backoff 0 --snapshots 1`

Hypothesis-free: the PLL held at Nint 88 (f_VCO 2.534 GHz), the tone
stepped 2–1300 MHz in 3 MHz steps (433 points per code), `if_agc_val`
(MSB-first) read at each; the receiver answers where the tone is an IF
from the LO.

| code | full-strength response (MHz, reading) | LO | f_VCO / LO | weaker responses |
|---|---|---|---|---|
| 100 | 71–77 (−987, −1107, −1154) | ≈ 79 | **32** | 239–248 (3·LO), 383–395, 704–710, 1019–1028 |
| 101 | 29–38 (−1067, −1245, −1053, −605) | ≈ 40 | **64** | 122–131, 188–197, 347–353 |
| 110 | 623–632 (−149…−508) | ≈ 634 (= f_VCO/4) | 4? | a broad shoulder 596–683 |
| 111 | 626–632 (−897, −869, −806) | ≈ 634 | 4? | 617–641 |

100 and 101 are clean dividers (bit 2 of SEL_DIV acting as ×16 on the
documented /2 and /4). 110 and 111 answer only around f_VCO/4 at this
Nint, and in the sweep the /4 prediction held for them at Nint 93 alone:
not dividers the driver can use. `tuner::MEASURED_DIVIDERS` carries
100 → 32, 101 → 64.

**Consequence:** with the VCO at 1.70–3.65 GHz the reachable LOs are
26.6–57 MHz (/64), 53–114 (/32), 212–456 (/8), 425–912 (/4), 850–1825
(/2), 1.7–3.65 GHz (/1): **no code reaches 114–212 MHz** (RF ≈
110–208 MHz with a 4.57 MHz IF). Either the VCO runs lower than
1.67 GHz under settings this search did not vary (the unspecified cells
of R24, R25, R28, R31, the LDO voltages, PW_SDM, CAPX), or the band is
not covered; that is an open search, not assumed either way.

### Step 4e — `tune 300e6` — `data/step4-tune-300.000MHz.json`

With `VCO_RANGE_HZ` set, `choose_div` picked /8 (Nint 84, SDM 39504 →
LO 304.570 MHz exactly); the tone at 300 MHz read −801 (`if_agc_val`
MSB-first) against 8191 off, `rf_agc_val` 3088 against 8191; R2 = 93.
Fractional programming by the documented formula works.

### Step 4f — band table — `data/step4-band-table.jsonl` — `harness band-table --config vagc --meter if_agc_val_msb_first -3000 3000 down --txvga 10 --backoff 0 --settle 300 --snapshots 1 --grid-mhz 10`

87 frequencies (42–102 and 212–1002 MHz in 10 MHz steps; 112–202 MHz
skipped: no divider, step 4d), 45 936 readings, staged: 16 paths at
TF_NCH = TF_LP = 0000, then the best two paths × 256 corner codes. The
source was re-levelled once per frequency at the bypass path and held
within it; the meter is `if_agc_val` (MSB-first), lower = more signal,
≈ 207 counts/dB; a path reading repeats to 0.2–0.5 dB (the reading
right after a retune can be an outlier: 692 MHz, −4399 then −923).

- Paths passing nothing (reading 8191, the rail), out of 87 frequencies:
  (RFFILT 11, RFMUX 11) 86; (11, 10) 82; (11, 01) 65; (11, 00) 52;
  (01, 11) 46; (10, 11) 43; (10, 10) 39; (00, 11) 37; (01, 00) 27;
  (00, 10) 26; (10, 00) 22; (01, 10) 19; (00, 00) 5; (10, 01) 2;
  (01, 01) 2; (00, 01) 1. The undescribed RFFILT 11 is dead; the
  undescribed RFMUX 11 nearly so.
- Best path at the corner codes 0000/0000: (RFFILT 10, RFMUX 01) at 39
  frequencies, (00, 01) at 22, (00, 00) 8, (01, 01) 8, (10, 10) 5,
  (01, 10) 3, (10, 00) 2. The spread between live paths at one frequency
  is large (median 21 dB, up to 48 dB); the band code matters even with
  the tracking filter bypassed.
- The corner codes (TF_NCH, TF_LP) raise the best path by at most 3.8 dB
  (median 0.6 dB) at this level: with a single tone they tune the
  passband's edge, not its gain. Their rejection of an out-of-band tone
  is not measured here (a second-tone step would be).
- The per-frequency table (best path, best corner codes, their readings)
  is read from the record by frequency; a generated table for the driver
  comes from this file.

### Step 4g — IF filter — `data/step4-filter.jsonl` — `harness filter --config vagc --meter if_agc_val_msb_first -3000 3000 down --txvga 10 --backoff 0 --settle 300 --snapshots 1`

LO fixed (300 MHz + 4.57 MHz, /8), the tone 0.1–12 MHz below the LO in
100 kHz steps, `--ddc track` (pset_iffreq 1 MHz below the tone). 37
curves × 120 points. Readings relative to the reference curve (FILT_CODE
0000, FILT_BW 00, HPF 1111), which is flat (≈ −750 ± 100) from 0.6 to
10.4 MHz, falls below 0.5 MHz and is −3 dB at ≈ 10.8 MHz; some points
alternate between two loop states ≈ 1.7 dB apart. The repeat of code 0
in the FILT_CODE stage matched the reference to 0.0 dB.

| field | result |
|---|---|
| FILT_CODE (BW 00, HPF 1111) | upper −3 dB corner 10.6, 10.6, (10.6), 10.5, 10.2, 9.9, 9.7, 9.4, 9.2, 9.0, 8.8, 8.6, 8.4, 8.1, 8.1, 7.8 MHz for codes 0–15: monotonic, ≈ 0.19 MHz per code; ≥ 16 dB down 2 MHz past the corner |
| FILT_BW (CODE 0, HPF 1111) | 00 = the reference; 01 and 10 identical, corner 8.8 MHz; 11 corner 7.3 MHz — as Table 1-3 says (widest / middle / narrowest) |
| HPF (CODE 0, BW 00) | all 16 codes within ±1 dB of the reference from 0.1 to 12 MHz: no measurable high-pass corner above 0.1 MHz in this configuration (the roll-off below 0.5 MHz is common to every code, so it is not the tuner's HPF) |

Passband flatness in-band: ±1 dB. R30 bit 6 was at the matrix's 1
throughout (`--r30b6 0` not run).

### Step 4h — if-choice: the meter is pre-DDC; the wanted sideband is below the LO; image rejection 10–20 dB uncalibrated — `data/step4-if-choice.jsonl` — `harness if-choice --config vagc --txvga 10 --backoff 0 --snapshots 1`

LO 304.57 MHz (/8), tone 0.25–12 MHz below and above the LO in 250 kHz
steps × pset_iffreq IF {0, 3.57, 4.063, 4.07, 4.57, 4.5714} MHz ×
Spec_inv 0/1: 1152 readings.

- Tone below the LO: `if_agc_val` ≈ −400…−830 (full strength) from
  0.5 to 10 MHz for every DDC IF and both Spec_inv values, +300 at
  0.5 MHz, +600…+900 at 11 MHz — the tuner's IF passband of step 4g,
  unchanged by anything the demodulator's DDC is set to. **`if_agc_val`
  is taken before the DDC and the channel filter**; it cannot locate the
  tone against pset_iffreq, and no documented readback can (`dagc_val`
  flips 0/255 at random here too). The DDC IF for the driver is the
  tuner's IF by construction (4.57 MHz, Spec_inv = 1 for the high-side
  LO, §9.3), verifiable only once a baseband path exists (step 3's open
  question).
- Tone above the LO: 2000–3300 at 0.5–2 MHz (10–15 dB below the lower
  sideband), 3300–5000 at 4.5 MHz, the rail (nothing) from ≈ 7 MHz. So
  the wanted sideband is **below the LO** (high-side mixing, as §9.3
  describes) and the upper sideband is the image, rejected by only
  10–20 dB with IMR_G = IMR_P = 32 (the datasheet's 65 dBc needs the
  undocumented image calibration: a search over IMR_G × IMR_P with the
  tone above the LO is the next step for that).

### Step 4i — gain per code — `data/step4-gain-ladder.json` (loop meter) — `harness gain-ladder --config vagc --meter if_agc_val_msb_first -3000 3000 down --txvga 10 --backoff 0 --settle 400`

300 MHz, the other stages at code 8. A first run of this command lost
its record to a harness panic (a JSON array given a field in the
comparator's stop path); fixed, and every JSON step now writes its
record on a panic too.

- **VGA_CODE** (comparator: VGA_MODE = 0, the source level bisected in
  1 dB steps to the flip of `if_agc_val` between +8191 and −8192): flip
  at +19, +16, +13, +10, +7, +3, −1, −4, −7, −11, −14, −18, −21, −25,
  −31 dB for codes 0–14; code 15 below the source's −36 dB floor. Gain
  rises with the code by 3–4 dB per step, **3.57 dB/code** over 0–14
  (the document's 3.5 dB/step with its sign misprint confirmed; the
  documented 52.5 dB span for 0–15 is consistent with code 15 being
  > 50 dB above code 0).
- **LNA_GAIN** (loop meter, source re-levelled on the exact backoff):
  relative to code 0: 0, 1.9, 5.4, 7.3, 7.5, 13.0, 16.4, 14.9, 24.3,
  25.3, 27.8, 29.8, 31.9, 30.6, 30.7, 36.4 dB — 36 dB in all, in
  uneven steps (0.2 to 9.4 dB), two steps slightly negative within the
  loop's ≈ 1.7 dB two-state quantisation.
- **MIX_GAIN** (loop meter, source fixed): 0, 0.7, 1.4, 1.1, 3.0, 3.9,
  4.0, 7.7, 7.8, 10.7, 10.7, 11.9, 12.1, 13.4, 15.1, 14.6 dB — ≈ 15 dB,
  flat from code 12.

The loop meter's dB scale is the −207 counts/dB fit of step 4b (from
nominal txvga steps). The comparator method has 1 dB resolution with
exact source steps, so it was then applied to all three stages.

### Step 4j — gain per code by the comparator, all stages — `data/step4-gain-ladder-comparator.json` — `harness gain-ladder --config manual --meter if_agc_val_msb_first -3000 3000 down --txvga 10 --backoff 0 --settle 300 --out …`

300 MHz, the other stages at code 8, every code's flip level found by
bisection in 1 dB steps (exact backoff for levels ≤ 0, txvga above).
Gain relative to code 0 = −(flip − flip₀):

| code | 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 | 11 | 12 | 13 | 14 | 15 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| LNA_GAIN flip dB | 16 | 13 | 9 | 6 | 3 | 1 | −2 | −5 | −7 | −11 | −12 | −14 | −15 | −18 | −19 | −20 |
| LNA gain rel. dB | 0 | 3 | 7 | 10 | 13 | 15 | 18 | 21 | 23 | 27 | 28 | 30 | 31 | 34 | 35 | 36 |
| MIX_GAIN flip dB | 5 | 3 | 1 | 0 | −2 | −3 | −5 | −6 | −7 | −9 | −10 | −12 | −12 | −13 | −13 | −12 |
| MIX gain rel. dB | 0 | 2 | 4 | 5 | 7 | 8 | 10 | 11 | 12 | 14 | 15 | 17 | 17 | 18 | 18 | 17 |
| VGA_CODE flip dB | 19 | 16 | 13 | 10 | 7 | 3 | −1 | −4 | −7 | −11 | −14 | −18 | −21 | −25 | −30 | < −36 |
| VGA gain rel. dB | 0 | 3 | 6 | 9 | 12 | 16 | 20 | 23 | 26 | 30 | 33 | 37 | 40 | 44 | 49 | > 55 |

Monotonic in every stage (the mixer's 12–15 within ±1 dB of flat); the
VGA repeats step 4i's run to ±1 dB. Resolution ±1 dB (the bisection's
step; levels above 0 dB rest on the source's nominal 1 dB txvga steps).
These are the driver's gain tables; a generated table cites this file.

## Step 0 — recovery: PROVEN — `data/step0-recovery.json` — `harness recovery`

3/3 power cycles: the dongle is gone within ~60 ms of power-off and
enumerates again within ~530 ms of power-on (now as 0bda:2832).

## Step 1A — system/USB: wValue = base+offset, little-endian — `data/step1-readonly-sys-usb-encoding.json` — `harness protocol-readonly`

Candidates: base+offset, offset, offset<<8, offset<<8|base>>8,
swap(base+offset). base+offset matches DEMOD_CTL 0x20, GPO 0x18, GPOE
0x19, GP_CFG0 0x96, GP_CFG1 0x55 (5/6); the other forms score 0.
I2CMSTR reads `10 09 93 00` = 0x00930910 little-endian; byte reads
inside the 32-bit registers work.

Mismatches: GPD 0x06 (doc 0x0E), I2CCR 0x17 (0x13), I2CMCR 0x28
(0x013A0000), USB_SYSCTL 0x09 (0x01), EPA_CFG 0xD1 (0x40), EPA_CTL 0x0210
(0), EPA_MAXPKT 0x0200 (0x40), EPA_FIFO_CFG 0x14 (0xA). The dongle had
been opened and streamed by another driver earlier in the same power
session, so these are that driver's leftovers, not reset values; a power
cycle is needed before reading reset values.

Descriptor: high speed; interface 0 bulk IN 0x81, max packet 512;
interface 1 has no endpoints.

## Step 1B — demodulator: no answer as found (DEMOD_CTL 0x20) — `data/step1-readonly-demod-encoding.json`

512 forms (offset|k<<8, offset<<8|k for k in 0..=255) × 101 documented
reset bytes (59 non-zero): every read stalls except wValue low byte
0xA0/0xA1, which reads the configuration EEPROM (M24C02, Fig. 9;
contents never recorded, marked RR). Best score 3/59.

Block 0 with wValue (offset<<8)|i2c_address is a byte-addressed I²C read.

## Step 1A again, after a power cycle — `data/step1-sys-usb-encoding.json` — `harness protocol`

base+offset scores 8/14 (5/6 8-bit; 32-bit: 3 little-endian, 1
big-endian); every other form ≤ 1. **Little-endian** (I2CMSTR
`10 09 93 00`, USB_SYSCTL `01 00 00 00`, USB_EPA_CTL 0 match).

Power-on values that differ from the document, now with no other driver
involved, so they are the chip's own: GPD 0x06 (doc 0x0E), I2CCR 0x17
(0x13), I2CMCR 0x02000028 (0x013A0000), USB_EPA_CFG 0xD1 (0x40 — reads
as enabled, bulk, IN, endpoint 1, matching the descriptor's 0x81),
USB_EPA_MAXPKT 0x200 (0x40 — 512, the descriptor's), USB_EPA_FIFO_CFG
0x14 (size 4, doc 0xA). The two from 1A that were another driver's:
USB_SYSCTL now 0x01 and USB_EPA_CTL 0, as documented.

## Step 1B/1C — demodulator: wValue = offset<<8 | 0x20 — `data/step1-demod-encoding.json`

| condition | DEMOD_CTL | best form | non-zero oracles | all oracles |
|---|---|---|---|---|
| as booted | 0x20 | offset<<8 \| 0xA0 (EEPROM) | 3/59 | 17/101 |
| PLL enabled (bit 7), 50 ms settle | 0xA0 (read back) | offset<<8 \| 0x20 and \| 0x21, tied | 58/59 | 100/101 |

All other 508 forms ≤ 3/59. ADC enables were not needed (stage 2 not
run). The tie is the low address bit: 0x20/0x21 = 7-bit I²C address
0x10, the demodulator's own slave of Fig. 5 (p. 15). 0x20 taken (the
write placement Table 27 uses for IICAdd). 336 s per 512-form search.

The one miss: page 1 0x5F reads 0x40; Table 11 gives
dis_auto_scan/pset_mode_gi [3:0] = 0x6. Document vs chip: the chip
reads 0x0 in [3:0].

## Step 1D — writes verified — `data/step1-writes.json`

| register | original | written | read back | restored |
|---|---|---|---|---|
| demod p1 0x0B rf_agc_max | 0x7F | 0x7E | 0x7E | 0x7F |
| demod p1 0x09 if_agc_max | 0x7F | 0x7E | 0x7E | 0x7F |
| demod p0 0xA5 PID31[7:0] | 0x2D | 0x2C | 0x2C | 0x2D |
| USB_EPA_MAXPKT (32-bit LE) | 0x200 | 0x000 | 0x000 | 0x200 |
| DEMOD_CTL (PLL bit) | 0xA0 | 0x20 | 0x20 | 0xA0 |

Demodulator multi-byte reads auto-increment: page 1 0x9D len 6 = the
six single reads (`AE BA F3 26 66 64` — cfreq_off_ratio 0xAEBAF and
rsamp_ratio 0xC99999 exactly as Tables 7/12 print).

## Step 2 — tuner: wValue = offset<<8 | 0x34, after PLL + IIC_repeat bit 3 — `data/step2-tuner-access.json` — `harness tuner`

Cumulative from power-on (each step then the probe):

| applied | tuner read 0x0034 / 0x0035 |
|---|---|
| nothing, PLL, reset release, ADCs, FD10, tuner power | Stall |
| + IIC_repeat page 1 0x01 bit 3 (0x10 → 0x18) | `69` = 0x96 bit-reversed (5 bytes: `69 01 D3 FF 07`) |

wValue 0x3400 / 0x3500 / 0x0000 stall in every state.

Ablation (full set minus one, from a fresh power-on each): **necessary
= DemodPll, RepeatBit3**; not necessary = reset release (already 1),
ADC enables, FD10 write, GPIO3 tuner power.

Repeater bit (Table 3 bit 3 vs Note2's "7th bit"): bit 7 alone (0x90)
no access; bit 6 alone (0x50, "7th" counted from 1) no access; bit 3 +
bit 7 access. **Table 3 is right; Note2 is wrong.**

Write path: wValue (19<<8)|0x34, data `00` → accepted; wValue 0, data
`13 00` (Table 28's "within the DATA stage" note) → Stall; the Table 27
write with the repeater cleared → Stall (no acknowledge). **Table 27's
wValue form is right; Table 28's note is wrong.**

Address sweep, every 7-bit address × {a<<1, a<<1|1}, offset 0, 1 byte:

| repeater | answered (7-bit) |
|---|---|
| open | 0x10 (demodulator), **0x1A (tuner: R0 0x96)**, 0x50 (EEPROM, RR) |
| cleared | 0x10, 0x50 |

The tuner is the documented 0x1A (write 0x34 / read 0x35, [R820T]
Table 3-1); the low wValue bit is ignored (the direction comes from
bmRequestType). Through the tuner block, address 0x10 reads `01 18 40
2D C0` with the repeater set and `01 10 40 2D C0` cleared: its second
byte tracks page 1 0x01 (0x18 / 0x10).

Recovery note: one power cycle (500 ms off) did not bring the dongle
back; 3 s off did. `power_cycle` now retries once with 3 s off and
records it.

## Step 3 — baseline: 0 bytes — `data/step3-baseline.json` — `harness baseline`

Documented-only baseline at 2.4 Msps (rsamp_ratio 0x3000000; every
write verified by read-back): DEMOD_CTL 0x20 → 0xE8, GPO 0x18 → 0x10,
AD_EN p0 0x08 0x0D → 0xCD, en_bbin p1 0xB1 already 0x1F, pset_iffreq
0x35D75D → 0, rsamp bytes `F3 26 66 64` → `FC 00 00 00`, EPA_MAXPKT 0x200
/ EPA_CFG 0xD1 / DMA already as wanted, EPA_CTL FIFO reset 0x200 → 0.
Ten 100 ms windows: **0 bytes** each.

## Step 3 — exhaustive single-bit scan: 0 / 10240 stream — `data/step3-scan.jsonl` — `harness scan`

Candidates: pages 0–4 × offsets 0x00–0xFF × bits 0–7 = 10240, all
logged once. Streaming = within 20 % of 2 × 2.4e6 B/s. After each
candidate the baseline's DEMOD_CTL, AD_EN, en_bbin and rsamp bytes are
re-read; if disturbed the baseline is applied again (power cycle if it
does not hold).

| outcome | count |
|---|---|
| streams | **0** |
| any bulk bytes | 2: p1 0x92 bit 6 — 8460 B / 100 ms (84 kB/s, = 45 × 188); p1 0x92 bit 7 — 1316 B (= 7 × 188), with 752 B (= 4 × 188) stale from bit 6 discarded first |
| read errors | 0 |
| wedged (write Cancelled, chip gone → power cycle, re-baseline) | 6: p0 0x20 b4, p0 0xA7 b5, p0 0xA7 b6, p4 0x20 b4, p4 0xA7 b5, p4 0xA7 b6 |
| baseline disturbed, reapplied, held | 10: p0 0x10 b1 and p4 0x10 b1 (AD_EN and rsamp back to their power-on values `0D`, `F3 26 66 64`; the restore write stalled — a demodulator soft reset), p4 0x08 b0–b7 (AD_EN cleared) |
| restore not verified (not wedged) | 247: 246 on page 3 (60 offsets, the readback page — live values), 1 on p4 0x10 b1 |

The 188-byte multiples match Table 34's 188-byte blocks, the
transport-stream path, at ~1.8 % of the IQ rate: not raw IQ by the
detector. Page 4 writes at 0x08, 0x10, 0x20, 0xA7 act like page 0's
(same effects), while page 4 reads differ from page 0 (106/256 bytes
equal at bit 0; page 4 reads 0x00 at all but 6 offsets).

## Step 3 — confirm p1 0x92 bit 6: not reproducible alone — `data/step3-confirm-p1-92-b6.json` — `harness confirm 1 0x92 6`

From a fresh power-on + baseline at each rate, then the flip (0x00 →
0x40, read back 0x40 every time):

| rate | rsamp_ratio | control 100 ms before flip | 10 × 100 ms right after flip | bytes/s vs 2·rate |
|---|---|---|---|---|
| 2.4 Msps | 0x3000000 | 0 B | 0 B in every window | 0 / 4.8e6 |
| 1.2 Msps | 0x6000000 | 0 B | 0 B in every window | 0 / 2.4e6 |
| 2.048 Msps | 0x3840000 | 0 B | 0 B in every window | 0 / 4.096e6 |

No bytes, so no 188-byte structure or I/Q statistics to report.
Minimisation (each baseline item removed, flip kept, 500 ms right after
the flip): 0 B in all 10 (with DEMOD_CTL removed the flip itself stalls).
An earlier run of the same command (200 ms discarded after the flip,
then 1 s; record overwritten by the run above) also read 0 B everywhere.
So the scan's 45 blocks came from the chip's state at that point in the
scan (1170 candidates since the last power cycle), not from bit 6 alone.

## Step 3 — byte scan: NO single byte streams — `data/step3-bytescan.jsonl` — `harness bytescan [--limit N]`

Every value of every register on pages 0–2 (3 × 256 × 256 = 196608,
minus each register's current value, logged as skips), page / offset /
value ascending. Pages 3 and 4 excluded (live readbacks; page 4 writes
mirror page 0's). 50 ms window right after the write; any byte, counted
or stale, triggers a 200 ms follow-up, and the candidate streams if
either window is within 20 % of 4.8 MB/s. The window choice is justified
in the record header. The scan does not stop on a hit.

Smoke test, `--limit 2000`: page 0 0x00 value 0x01 → 0x07 value 0xD7,
2000 tested + 8 skipped in 166.1 s = **12.04 tested/s** (83 ms each);
0 streams, 0 wedges, 0 disturbed baselines, 0 failed restores.
Projected rest: 194600 candidates ≈ **4.5 h** plus power cycles
(~3.5 s each; the bit scan wedged on p0 0x20 and 0xA7).

Seen so far: page 0 0x07 (holds 0x3C) values 0xC0–0xCB each released a
one-shot burst of 5–14 whole 188-byte blocks (940–2632 B, every count
≡ 0 mod 188), mostly before the window opened (stale); the 200 ms
follow-up read 0 every time. 0xCC–0xD7: nothing.

Complete: 196608 candidates covered (195840 tested + 768 skipped as the
register's current value), 65280 tested per page, in four chunks
(9.35–12.15 tested/s). **0 streams.** Any bytes at all: page 0 0x07
(2 candidates, the one-shot 188-byte bursts above) and page 1 0x92
(4 candidates). 320 candidates logged a write error (the write stalled;
the scan recovered and continued). From the documented baseline, raw IQ
is not one register value away.


## Step 3 — restart probe — `data/step3-restart-probe.json` — `harness restart-probe`

From a fresh baseline, test register page 1 0x0B (rf_agc_max, reset
0x7F) written 0x7E:

| action | effect on the demodulator's registers |
|---|---|
| DEMOD_CTL bit 5 pulse (0xE8 → 0xC8 → 0xE8) | every watched register back to its power-on value: test 0x7E → 0x7F, AD_EN 0xCD → 0x0D, pset_iffreq 0 → `35 D7 5D`, rsamp `FC 00 00 00` → `F3 26 66 64`; IIC_repeat, opt_adc_iq, Spec_inv, en_bbin unchanged (already at reset) |
| demodulator access while held in reset | reads and writes stall |
| USB_EPA_CTL bit 9 pulse (0x200 then 0) | no watched register changes |

So the demodulator reset discards a candidate written before it, and
nothing can be written during it. Restart order used:
**demodulator reset pulse → the baseline's 9 demodulator bytes rewritten
→ candidate written → FIFO reset pulse → candidate read back → measure.**
Every candidate runs from the same state (power-on values + baseline +
the candidate alone), with no history from earlier candidates.

## Step 3 — byte scan with restart: COMPLETE, no single byte streams — `data/step3-bytescan-restart.jsonl.gz` — `harness bytescan --restart [--limit N]`

Complete 2026-10-09: 195,840 tested + 768 skipped = 196,608, 65,280 per
page, **0 streams**, 321 wedges (page 0 0x20 and its neighbours), all
recovered. Bytes at all: page 0 0x07 only (64 candidates, the one-shot
188-byte transport-stream bursts within 0.1 ms of the FIFO reset).

Same candidate space and order as the plain byte scan (pages 0–2 ×
offsets × values ascending, current value logged as a skip, pages 3/4
excluded). After the restart: first-byte deadline 200 ms; on any bytes a
further 200 ms window, streaming = within 20 % of 4.8 MB/s. Compact
lines (legend in the header): ~89 bytes per candidate, ~17.5 MB for the
whole space.

Smoke test, `--restart --limit 2000`: page 0 0x00 value 0x01 → 0x07 value
0xD7, 2000 tested + 8 skipped in 545.2 s = **3.67 tested/s** (272 ms
each: the 200 ms deadline + ~72 ms of control transfers). Restart
accepted and the candidate in place for 2000/2000; 0 streams, 0 wedges,
0 errors. Projected rest: 194600 candidates ≈ **14.7 h** (plus power
cycles).

Seen: page 0 0x07 (holds 0x3C) values 0xC0–0xD7 — all 24 tested so far
— each give a one-shot burst of 5–16 whole 188-byte blocks (940–3008 B)
within 0.1 ms of the FIFO reset, then 0 B/s in the 200 ms window. In the
plain scan only 0xC0–0xCB did; with the restart every value from 0xC0
does.


## Step 3 — block scan: the system block, the USB block, pages 3 and 4 — `data/step3-blockscan-{sys,usb,p3,p4}.jsonl` — `harness blockscan --block sys|usb|p3|p4 [--limit N] [--out PATH] [--from OFF [VAL]]`

Results so far: **p3 65,536 covered, 0 streams, 0 wedges; p4 65,536,
0 streams, 320 wedges (the page-0 mirror at 0x20) all recovered; sys
65,536, 0 streams, 0 wedges; usb 130,407 covered (every offset of
0x000–0x1FF touched), 0 streams, 723 wedges at six offsets (0x01, 0x0C,
0x10, 0x18, 0x1C, 0x104), all recovered — the only candidates not run
are the 155 deferred values of 0x1C.** So every single-write space the
map has room for — demodulator pages 0–4, the system block, the USB
block — has been covered once from the documented baseline without a
single stream. The usb block was stopped twice by a bench hazard:

**Hazard — USB register 0x1C (0x201C, in the map's reserved range).**
Writes there wedge the chip at every other value, and twice (values
0x54 and 0x64) the wedge plus the power cycle that followed took the
analyzer, the dongle and the second radio on the same hub branch off
the bus together; only a restart of the hub (and once the host)
brought them back. Not cycle wear: page 4 survived 320 consecutive
wedges. The remaining values of 0x1C (0x65–0xFF) are DEFERRED, to be
run last with someone present to restart the hub; the rest of the
block resumes at 0x1D (`--from 29 0`). This is a declared deferral
of 155 candidates, not a cut: they stay in the candidate space.

Every byte value at every address of the register spaces no single-write
scan had covered, plain method (no restart, as the plain byte scan), one
record per block, each resumable by (offset, value), offset then value
ascending, the address's current value logged as a skip. Per candidate:
read the original; write; read back (`in`, with `rb` when it differs — a
register that does not take the byte is still measured); 50 ms window;
200 ms follow-up on any byte; restore and verify; 50 ms after-restore
window on any byte; chip answering; the baseline re-checked (the
demodulator items plus USB_SYSCTL DMA, USB_EPA_CFG, USB_EPA_CTL,
USB_EPA_MAXPKT), reapplied if disturbed, power cycle if it does not
hold. Windows and the streams criterion are the plain byte scan's. Every
line carries the detector's self-check (`q`, `qf`, `qa`).

The spaces and their bounds, from the documented maps (each record's
header carries the full statement):

| block | space | bound, and what lies outside |
|---|---|---|
| `sys` | request block 2, wValue 0x3000 + offset, offsets 0x00–0xFF: 65,536 | Table 14 (pp. 27–28) maps 0x0000–0x000C and the I²C master 0x0040–0x0053; 0x00–0xFF covers both and the gaps 0x0D–0x3F, 0x54–0xFF. **Deferred**: the IrDA registers of the same table (IR_RX_BUFF 0xFC00–0xFC7F, a 128-byte buffer; IR_RX_IE … IR_RESUME_MASK_T_LEN 0xFD00–0xFD19) — 0x3000 + 0xFC00 does not fit a 16-bit wValue, Table 28 names a separate IR request block (5) with no base given, and it is an infrared receiver's buffer and control: a larger, separately addressed space to declare on its own if this one is empty |
| `usb` | request block 1, wValue 0x2000 + offset, offsets 0x000–0x1FF: 131,072 | Table 29 (p. 38): USB_SYSCTL 0x0000, USB_EPA_CFG 0x0144, USB_EPA_CTL 0x0148, USB_EPA_MAXPKT 0x0158, USB_EPA_FIFO_CFG 0x0160, reserved 0x0004–0x0140, 0x014C–0x0154, 0x015C, 0x0164–0x0FFF; the bound covers every named register, the gaps among them and 0x0164–0x01FF; outside lies only the reserved 0x0200–0x0FFF (3584 bytes) |
| `p3` | demodulator page 3, offsets 0x00–0xFF: 65,536 | the readback page (Tables 2, 9–12, all read-only); the bit scan's 60 live offsets listed in the header |
| `p4` | demodulator page 4, offsets 0x00–0xFF: 65,536 | undocumented; the bit scan's page-0-like offsets (0x08, 0x10, 0x20, 0xA7) and the read comparison (106/256 equal at bit 0; non-zero only at 6 offsets) in the header; covered whole regardless |

USB and system registers are 32-bit little-endian words (step 1A/1D);
each candidate is one byte with wLength 1 at one address, and whether the
chip takes a one-byte write into a 32-bit register is what `in` / `rb`
record. Declared hazards (headers): DEMOD_CTL bit 5 / bit 7, GPO bit 3
(tuner power) and bits 1/5 (clock latches), I2CMCR bit 30 "Command
Start" with I2CMFR as its FIFO byte (Tables 23, 26: a candidate at
0x44–0x53 can start an I²C transaction on the bus that carries the tuner,
the demodulator's slave and the configuration EEPROM); USB_SYSCTL bit 10
SIE reset, USB_EPA_CFG, USB_EPA_CTL bit 4 (a halt the detector clears).

Smoke tests, `--limit 300` each (offsets 0x00 whole and 0x01 to value
0x2D), 2026-10-08:

| block | tested/s | streams | wedges | what the first two offsets did |
|---|---|---|---|---|
| `sys` | 7.07 (42.4 s) | 0 | 0 | DEMOD_CTL (0x00, holds 0xE8): bit 2 never reads back (128 values `rb` = v with bit 2 clear), 128 values with bit 5 clear reset the demodulator (AD_EN and rsamp_ratio back to power-on; baseline reapplied, held every time); GPO (0x01, 0x10): all 45 values read back, nothing else |
| `usb` | 1.90 (158.0 s) | 0 | **22** | USB_SYSCTL byte 0 (0x00, holds 0x01): only 7 of 255 values read back (values with bit 1 set read 0; others keep bits 0 and 2), and the restore of 0x01 read 0 for 128 values until the baseline reapply set DMA again; byte 1 (0x01, holds 0): no value reads back, and every value with bit 2 set — bit 10, the SIE reset — ends the session: "read back: Cancelled, restore: Stall", chip gone, power cycle (22 cycles, 59–64 ms to disappear, 499–567 ms to return; one needed the 3 s retry) |
| `p3` | 10.91 (27.5 s) | 0 | 0 | 0x00 (holds 0x03) takes every value; 0x01 (holds 0x77) takes none |
| `p4` | 10.90 (27.5 s) | 0 | 0 | 0x00 (holds 0x04) takes every value; 0x01 (holds 0x02) takes none |

0 bytes on the bulk endpoint in all 1200 candidates; every `q` is
[submitted, completed, 0, 0, ≥ 1 in flight, 0] — no error, no halt, the
queue never empty. Lines are ~78 bytes (demodulator pages) to ~140 bytes
(system block, with its events); projected records ≈ 5 MB (p3, p4),
9 MB (sys), 16 MB (usb, the recovery lines included).

Projection per block, from the plain rate (~11 tested/s) with the wedge
cost (~6 s per power cycle) counted per register, not from the chunk's
average: `p3`, `p4` ≈ 1.7 h each; `sys` ≈ 1.7 h plus reapplies at
whatever else resets the demodulator; `usb` ≈ 3.3 h plus 106 more SIE
resets at offset 0x01 (~11 min) plus whatever USB_EPA_CFG / USB_EPA_CTL
cost. Resume with the same command; `--limit` bounds a chunk.

## Step 3 — the bit scan from two other starting states: low-if and power-on — `data/step3-scan-lowif.jsonl`, `data/step3-scan-poweron.jsonl` — `harness scan --baseline low-if|power-on [--limit N]`

Why (2026-10-09): every raw-sample scan above started from the
documented **zero-IF** baseline (§9.1), but the tuner is a low-IF part
([R820T] §4.3, Table 4-1) mixing high-side (4h), and step 4 saw the
demodulator respond to a tone only in the documented IF mode with the
tuner powered and tuned. And the zero-IF baseline's own writes might
block the mode: the bare power-on state had never been scanned.
`examples/harness/baselines.rs` defines both new starting states, item
by item with citations; each record's header carries the definition,
every write with its read-back, and the baseline measured alone.

**low-if** (16 items, applied after a power cycle, every demodulator
write a read-modify-write read back): DEMOD_CTL PLL | ADC_I | ADC_Q |
reset-release (Table 15); GPIO3 tuner power + 50 ms (Tables 16–19);
IIC_repeat (Table 3); AD_EN 0x08[7:6] = 01 and en_bbin = 0 (§9.1 IF
mode); pset_iffreq = −665554 for 4.57 MHz (§9.3, [R820T] Table 4-1
row 5); Spec_inv = 1 (§9.3, high-side per 4h); en_if_agc, en_rf_agc,
aagc_hold 0 (Table 2), en_dagc (Table 10), AGC pin polarity 0 (Table 2,
4b); rsamp_ratio 0x3000000 = 2.4 Msps (§9.4); the tuner image of
`tuner.rs` in the vagc configuration, CLK_OUT_ENB = 0 (4a), LO 304.57 MHz
= 300 MHz RF + 4.57 MHz, /8, Nint 84, SDM 39504 (4e); then the zero-IF
baseline's four endpoint items (Tables 30–33). Read-backs in the record:
DEMOD_CTL 0x20 → 0xE8, page 1 0x01 0x10 → 0x18, page 0 0x08 0x0D →
0x4D, page 1 0xB1 0x1F → 0x1E, pset_iffreq `35 D7 5D` → `35 D8 2E`,
page 1 0x15 0x06 → 0x07, rsamp `F3 26 66 64` → `FC 00 00 00`, every
one verified; tuner R5–R15 read back exactly, R0 0x96, R2
VCO_INDICATOR 92 (inside 70–127, 4c); endpoint 0x200 / 0xD1 / FIFO
pulse 0 → 0x200 → 0 / DMA 1.

**power-on** (after a power cycle, nothing but): DEMOD_CTL bit 7 (the
demodulator PLL; without it every demodulator access stalls, 1B/1C, and
every scan candidate is a demodulator write; the ADC enables are not
set); the endpoint registers (USB_SYSCTL DMA, USB_EPA_CFG, USB_EPA_MAXPKT,
USB_EPA_CTL stall) read and written only where they differ from what bulk
IN needs. Streams are judged against the power-on rsamp_ratio 0xC99999 =
64/7 MHz (9.142857 Msps, 18.29 MB/s).

Procedure per candidate (the original bit scan's): read, write the bit
flipped, one 100 ms window (bytes completed before it counted as stale),
restore, verify, chip alive, the baseline's settings re-read (low-if:
DEMOD_CTL, AD_EN = 01, en_bbin = 0, IIC_repeat, Spec_inv, pset_iffreq,
rsamp_ratio, tuner R0 = 0x96 with R2 in 70–127; power-on: the PLL bit),
reapplied if disturbed, power cycle and the baseline from scratch if
not held or the chip is gone.

### Results: COMPLETE, 0 streams from either baseline

| | low-if | power-on |
|---|---|---|
| baseline alone, 10 × 100 ms | 0 B (twice); with the tone on 0 B; positive control `if_agc_val` 8191 → −1194 / −902 (the demodulator sees the tone) | 0 B |
| writes | 16 items, every read-back as written (above) | DEMOD_CTL 0x20 → 0xA0; endpoint read 0x01 / 0xD1 / 0x200 / 0, all as needed, **0 writes** |
| candidates | 10,240 (pages 0–4 × 0x00–0xFF × bits 0–7), 4 chunks | 10,240, 3 chunks |
| streams (within 20 % of 2 × rate) | **0** (rate 2.4 Msps) | **0** (rate 64/7 Msps) |
| any bytes | 2 candidates: p0 0x07 b7 (0x3C → 0xBC) 4,461,616 B in the window; p0 0x08 b0 3,704 B + 60,968 B stale — the carry-over of the 0x07 flood, which stopped at its restore | **0** (p0 0x07 b7 too: the tuner is not programmed) |
| wedged, recovered | 6: p0 0x20 b4, p0 0xA7 b5, b6, p4 0x20 b4, p4 0xA7 b5, b6 (the zero-IF scan's six) | the same 6 |
| baseline disturbed, reapplied, held | 10: p0 0x10 b1 and p4 0x10 b1 (everything back to power-on: the demodulator soft reset, as zero-IF), p4 0x08 b0–b7 (AD_EN) | 0 (the check is the PLL bit alone; the soft reset returns the demodulator to the power-on state, which is this baseline) |
| restore not verified, not wedged | 255 (254 on page 3, the live readbacks; 1 on page 4) | 85 (84 on page 3; 1 on page 4) |
| detector self-check | 10,240 lines: 0 transfer errors, 0 halts, never an empty queue | the same |
| zero-IF's byte sources | p1 0x92 b6/b7 (TS blocks in the zero-IF bit scan): 0 B here; p0 0x07 0xC0+ not in a bit scan's space | 0 B |

So the starting state was not what hid a raw-sample mode from the
single-bit search: from the documented low-IF state with the tuner
tuned (a state proven live by the tone), and from the bare power-on
state, no single bit of demodulator pages 0–4 makes the chip stream at
the resampler's rate. The one new behaviour is the FIFO replay below.

### Page 0 0x07 bit 7 from low-if: 45 MB/s — the endpoint FIFO's 20 blocks replayed, not samples — `data/step3-confirm-lowif-p0-07-bc.json` — `harness confirm-write 0 0x07 0xBC --baseline low-if --dump /tmp/rtl-flood --source-cmd "…"`

In the first low-if chunk page 0 0x07 0x3C → 0xBC gave 4,465,620 B in
the 100 ms window (44.6 MB/s, 9.3 × 2 × 2.4 Msps: outside the
criterion, so not a "stream", but the only sustained data any scan has
produced); it stopped at the restore (the next candidate: 3,592 B +
65,536 B stale, then 0). Not seen from zero-IF (bit scan: 0 B; byte
scan: only the 0xC0+ one-shot 188-byte bursts). Confirmed and
characterised (fresh power-on + low-if for each part; 10 × 100 ms
windows; repeated twice, the numbers below are the second run):

| test | result |
|---|---|
| control window before the write | 0 B at every rate |
| rate tracking: rsamp 2.4 / 1.2 / 2.048 Msps | 45.04 / 45.03 / 44.61 MB/s = 9.38 / 18.76 / 10.89 × 2 × rate: **does not track rsamp** |
| USB_EPA_FIFO_CFG[31:24] dropped blocks (Table 34) | 0 before, 255 (saturated) after every window, every rate |
| after the restore | 29–46 kB (in flight), then 0 B |
| periodicity (`data[i] == data[i+P]`, P ≤ 16384) | **P = 3760 B = 20 × 188 B**, match 1.000 over the first 32 KiB, 0.9992 over each whole capture; the mismatches (≈ 33.7 kB ≈ 9 × 3760) are the nine window boundaries where stale completions are not kept |
| the 3760-byte pattern | identical through the tone states of one power session (> 4 s); different in every power session; all 256 byte values, no 0x47 at a fixed column (not transport-stream packets); byte histograms of each capture are exact multiples of the repeat count |
| tone off / RF + 200 kHz / RF − 300 kHz / off | the same bytes; the same spectral peaks (cycles/sample −0.2517, 0.3032, 0.1733 … at 10–13 dB, read as complex or real) in all four: the RF is not in the data |
| bytes/s with the tone on | 37.7 MB/s (the transmitter shares the host's USB) |
| minimisation (each low-if item removed, 5 windows) | needed: **DemodCtl, IicRepeat, TunerImage** (0 B without any one; without IicRepeat the image cannot be written); every other item removed: 44.2–45.1 MB/s |

USB_EPA_FIFO_CFG reads 0x14 = 20 at power-on (1A); the replay period is
20 blocks of 188 bytes. So bit 7 of page 0 0x07 lets the endpoint read
the FIFO's memory round and round at the bus's full rate (the chip's
drop counter saturating), with nothing new written into it: the bytes
are the FIFO RAM's power-on contents. It needs the tuner programmed (not
the IF-mode demodulator settings), which is why the zero-IF scans never
saw it. **Not a sample stream.** Recorded as an any-bytes register; the
scan continued.

### The bit scan with the data path held open: COMPLETE, the content never changes — `data/step3-scan-lowif-open.jsonl` — `harness scan --baseline low-if-open`

Hypothesis (the project owner's, 2026-10-09, in the record's header):
page 0 0x07 bit 7 opens the bulk endpoint's data path; a second register
may select what feeds the FIFO. With the path held open, a bit that
routed the sampled signal into it would change the **content** (the
3,760-byte period would break), not only the flow.

Held state: low-if + `OpenPath` (page 0 0x07 0x3C → 0xBC, read back);
`disturbed` also checks that bit. The detector (`examples/harness/content.rs`)
works on each 100 ms window's bytes on the fly (never stored): bytes/s;
the period score = fraction of i with data[i] == data[i + 3760]; the
byte histogram's entropy; mean and variance of even/odd bytes as 8-bit
I/Q, unsigned and two's complement.

**Held state alone** (10 × 100 ms, written into the header before the
scan): 44.42–44.60 MB/s; period score **1.000** in every window; entropy
7.9472 bits; I/Q unsigned means 125.07 / 127.22, variances 5519 / 5714;
two's complement means 0.743 / −0.511, variances 5429 / 5145. With the
tone on (tone control `if_agc_val` 8191 → −957): 36.9–37.1 MB/s (the
transmitter shares the host's USB), every other number the same to the
third decimal.

**Thresholds** (header `detector_thresholds`): interesting = period score
< 0.99 (solo minimum − 0.01) with ≥ 15,040 bytes in the window, OR
bytes/s in [2.4, 9.6] MB/s (½–2 × 2 × 2.4 Msps); logged "rate changed"
outside [35.5, 53.5] MB/s (solo range ∓ 20 %), "signature changed"
with the entropy outside [7.897, 7.997] bits. An interesting candidate
would end the chunk and run `confirm-write` on it at once (rate
tracking, tone off/±offsets, statistics, minimisation).

**A gap found and closed.** Two candidates stop the replay for good
although every register reads back as held: page 0 0xB5 0x0A → 0x02
(bit 3; the window got 96 kB and one bulk transfer ended with
0xE00002ED, kIOReturnNotResponding; nothing more until a power cycle)
and page 4 0x02 0x00 → 0x01 (bit 0; 30 MB/s in its window, then nothing
until the page 4 0x10 b1 soft reset and reapply). Of the 48 + 113
candidates after them, 159 began on a stopped path (p4 0x06 b5 and 0x0E
b1 began with the flow briefly restarted by the candidate before). The held state now includes
the flow itself: before every write a 50 ms window must reach rate_lo,
else the baseline is reapplied, else power cycle (`held_flow_before`
on every new line). The 155 candidates with no bytes at all were re-run
(`--redo-flowless`), and the 4 that had begun without flow but got a
few stale bytes (p4 0x06 b4, 0x0A b4, 0x0B b0, 0x0E b0) by
`--redo-list`; the re-test lines carry `"redo":true` and supersede the
originals. In the redo pass the flow was found lost once (before p4 0x0B
b1, 28.7 MB/s; the reapply restored 41.5 MB/s).

Results, the latest line per candidate (10,240 candidates, 10,399
lines):

| | count |
|---|---|
| interesting (period broken, or rate toward 2 × sample rate) | **0** — no confirmation was triggered |
| period score below 1.000 (10,223 windows with ≥ 15,040 B) | **0**: every window that carried enough bytes to score was the same 3,760-byte replay |
| bytes/s in 2.4–9.6 MB/s | 0 |
| signature changed (entropy) | 0; entropy 7.9409–7.9487 bits throughout |
| rate changed: the flow throttled or stopped, the content periodic where scored | 28: p0 0x01 b0, 0x06 b5 b7, 0x07 b6 b7, 0x0A b3 b7, 0x0E b0, 0xB5 b3; p1 0x01 b2, 0x09 b7, 0x0C b6, 0xD7 b0; p4 0x01 b0, 0x02 b0, 0x06 b5 b7, 0x07 b0–b6 (page 4 0x07 writes reach page 0 0x07: the held bit is cleared, reapplied), 0x0A b3 b7, 0x0E b0 b1 |
| zero bytes in the window | 0 |
| wedged, recovered | 6 (p0 0x20 b4, p0 0xA7 b5 b6, p4 0x20 b4, p4 0xA7 b5 b6, as in every scan) |
| detector check | 0 halts; 1 transfer error (p0 0xB5 b3 above) |

**No candidate carries the tone, or anything else from the air:** no
single bit, from the held-open path, changed what the endpoint delivers
from the FIFO memory's 3,760-byte replay; the bits that matter only
throttle or stop it. The scan ran with the source unkeyed (the detector
looks for any change of content, which sampled noise produces as surely
as a tone); since nothing was interesting, no candidate reached the
tone test.

### Replay (from the repository root of the workspace)

```
SRC='--source-cmd "cargo run --release --example tone_source -- --cabled"'
cargo run --release -p sdr-rtl --example harness -- scan --baseline low-if-open --limit 0 $SRC      # held state alone, thresholds into the header
cargo run --release -p sdr-rtl --example harness -- scan --baseline low-if-open --limit 1500 $SRC   # repeat to 10240 (an interesting candidate ends the chunk with its confirmation)
cargo run --release -p sdr-rtl --example harness -- scan --baseline low-if-open --redo-flowless $SRC
cargo run --release -p sdr-rtl --example harness -- scan --baseline low-if-open --redo-list 4:0x06:4,4:0x0A:4,4:0x0B:0,4:0x0E:0 $SRC
cargo run --release -p sdr-rtl --example harness -- scan --baseline low-if --limit 2500 $SRC   # first chunk: header, solo, tone control
cargo run --release -p sdr-rtl --example harness -- scan --baseline low-if --limit 3000        # repeat until 10240
cargo run --release -p sdr-rtl --example harness -- scan --baseline power-on --limit 3500      # repeat until 10240
cargo run --release -p sdr-rtl --example harness -- confirm-write 0 0x07 0xBC --baseline low-if --dump /tmp/rtl-flood $SRC
```
(`--limit 0` writes the header and the solo measurement only. A record
that exists is resumed; move it away to start over. The first 300
low-if candidates were run before the baseline was itemised and set
aside; the record was restarted, with the same final register state.)

## Step 3 — the bulk detector on the wire — `harness detector-check`, `cynthion-witness watch S --ep 1`

The alarm: `data/step3-witness-restart-scan.log` (30 s during the restart
scan) shows 107 gaps of 190–250 ms with no IN tokens. That capture
watched the **control endpoint**: the witness picked "the endpoint with
the most DATA bytes" and EP 0x81 carried none, so it printed `device 11
EP 0x80` (endpoint 0 | 0x80). Its gaps are the harness's first-byte waits,
when no control transfer runs; its NAK runs are control-read data stages
NAKed for 1–5 ms while the chip completes the demodulator access. The
witness now takes `--ep N` (endpoint N of the device that receives vendor
requests), closes NAK runs at gaps, counts SETUPs per gap and prints
histograms and per-endpoint totals.

Same 30 candidates each time (restart: page 0 0x07 values 0xBC–0xD9,
`--from 0 7 0xBC --limit 30`; the 0xC0+ values give bursts), host and
wire byte counts compared:

| capture | endpoint | IN tokens | DATA (bytes) | harness bytes | host-quiet gaps ≥ 1 ms | NAK runs |
|---|---|---|---|---|---|---|
| original log, 30 s restart scan | EP 0 | ~28 k/s | ~130/s | — | 1619: 1512 × 1–2 ms (2.3 s), **107 × 190–250 ms (21.7 s)** | 1944, all 1–5 ms (3.6 s) |
| restart, OLD detector (`…restart-old-detector-ep1.log`) | EP 0x81 | 2,244,240 (~237 k/s) | 121 (53,768 B) | **53,768** | **0** | 30: 27 × ≥ 250 ms, 3 × 5–10 ms (9.36 s) |
| restart, NEW detector (`…restart-new-detector-ep1.log`) | EP 0x81 | 2,236,931 | 115 (50,196 B) | **50,196** | **0** | 29: 27 × ≥ 250 ms, 2 × 10–50 ms (9.33 s) |
| same run, watched on EP 0 (`…restart-ep0.log`) | EP 0 | 242,293 | 1053 (885 B) | — | 481: 438 × 1–2 ms, 12 × 50–100 ms, **30 × 190–250 ms (6.08 s)** | 585, all 1–5 ms |
| plain byte scan, NEW detector (`…plain-new-detector-ep1.log`) | EP 0x81 | 1,913,675 | 96 (40,796 B) | **40,796** (window 11,280 + stale 28,576 + later windows 940) | **0** | 27 |

So the host polled EP 0x81 continuously, through the control phases and
every window (one NAK run spans a whole candidate, ended only by the next
burst), and the harness counted every byte the wire carried. No STALL on
EP 0x81 in any of these. New-detector self-check in the restart run
(written before the `halted` field was added, so 5 fields): 26 lines
`q = [1,1,0,0,3]` (1 submitted, 1 completed, 0 errors, 0 halts, ≥ 3 in
flight), 4 lines `[0,0,0,0,4]`; plain run: 30/30 with 0 errors, 0 halts,
≥ 3 in flight. Scratch records: `data/step3-wirecheck-*.jsonl` (the
restart old-detector record lacks q, as written then).

The real hazard — `data/step3-detector-check.json` +
`data/step3-witness-detector-check-ep1.log` (USB_EPA_CTL bit 4, Table 32
"stall endpoint", set on purpose):

| phase | wire | self-check |
|---|---|---|
| idle, 3 × 100 ms | NAKs | 4 in flight, clean |
| bit 4 set; halt cleared | 1 STALL, then 13.3 ms without IN tokens (2 SETUPs: the clear), then NAKs | Stall + Cancelled ×3, 1 ClearFeature(ENDPOINT_HALT) ok, refilled to 4; next window clean |
| — USB_EPA_CTL read after the clear | | **0x00000000: ClearFeature(ENDPOINT_HALT) clears the chip's bit 4** |
| bit 4 set; resubmit only (the OLD behaviour) | 1 STALL, then **430.6 ms with no IN tokens** (7 SETUPs, until the clear) | Stall once; then windows of 0 completions, 0 errors, 4 in flight — indistinguishable from a NAKing endpoint; now flagged `halted` |
| bit 4 written 0, still no clear | still no IN tokens (bit 4 reads 0x0; the host-side halt persists) | `halted` |
| ClearFeature(ENDPOINT_HALT) | NAKs again | clean |

The old counter (a) resubmitted after a Stall without clearing, (b)
discarded completions taken before a window (`measure`'s stale drain,
the restart scan's `drain()`) with their errors unrecorded, and (c) kept
at most 4–8 error strings per window. A halt beginning outside a window
therefore left no trace and every later window of that power session
read 0 B. The new counter (`examples/harness/usb.rs`): 4 transfers kept
in flight (every completion resubmitted, topped up before each wait); a
Stall → cancel the queue, ClearFeature(ENDPOINT_HALT) (USB 2.0 §9.4.1,
§9.4.5), result recorded, refill, retried before the next wait if it
failed; every completion counted, errors by kind, none dropped; a
persistent `halted` state. Per window: `q`/`qj` (restart) or
`bulk_check` (plain, bit scan, baseline, confirm) = [submitted,
completed, errored, halts, fewest in flight, halted 0/1]; each run's
setup line carries the legend (`detector`).

### Which records are evidence

No harness write sets USB_EPA_CTL bit 4 (the baseline clears it; the
restart's FIFO pulse keeps it as read), and every record holds **0 bulk
transfer errors**. What remains is an undocumented STALL caused by a
demodulator write, which the old detector could have missed. A
candidate is *proven live* when a later candidate in the same power
session received bytes (a halted pipe delivers none until re-enumeration):

| record | candidates | power sessions | proven live | conditional (no STALL assumed) | verdict |
|---|---|---|---|---|---|
| step3-baseline.json | 10 windows | 1 | — | no write after the endpoint opened | **valid** |
| step3-confirm-p1-92-b6.json | 3 rates + 10 ablations | 13 | — | one demodulator write each; the bit scan's p1 0x92 b7 bytes came after b6 in the same session, so that flip does not halt the endpoint | **valid** |
| step3-scan.jsonl (bit scan) | 10,240 | 7 | 1,881 (18.4 %) | 8,359 (81.6 %; longest 5,229) | valid where proven; the rest is valid only if no candidate stalls the endpoint |
| step3-bytescan.jsonl (plain byte scan) | 195,840 | 325 | 42,605 (21.8 %) | 153,235 (78.2 %; longest 45,000) | same |
| step3-bytescan-restart.jsonl (stopped) | 5,097 | 3 | 2,040 (40.0 %) | 3,057 (60.0 %; longest 1,597) | same; lines appended from now on carry `q` |
| step3-witness-restart-scan.log | — | — | — | — | watched EP 0; **not evidence about EP 0x81** |
| step3-blockscan-{sys,usb,p3,p4}.jsonl (smoke, 300 each) | 1,200 | 1 / 23 / 1 / 1 | — | none needed: written with the new detector, every line's `q` shows 0 halts and ≥ 1 transfer in flight | **valid** |
| step3-scan-lowif.jsonl, step3-scan-poweron.jsonl | 10,240 each | 10 / 10 | — | none needed: new detector, every `bulk_check` 0 errors, 0 halts, ≥ 1 in flight | **valid** |
| step3-scan-lowif-open.jsonl | 10,240 (10,399 lines) | — | the flow itself: lines with bytes or `held_flow_before` | 159 candidates ran on a stopped path; re-run with the flow check (`"redo":true` lines supersede) | **valid** with the redo lines |

The windows themselves (50 ms + 200 ms follow-up; 200 ms first-byte +
200 ms judge) were polled throughout: the old detector and the new one
give identical wire behaviour without a halt. Closing the conditional
part needs the scans re-run with the new detector (any STALL now shows
as `halts` > 0 and is cleared); none has been started.
