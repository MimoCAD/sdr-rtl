# Bench findings

Replay: `cargo run --release -p sdr-rtl --example harness -- <step>`

Status: recovery proven; step 1 (encodings) and step 2 (tuner access) done; step 3 baseline + exhaustive single-bit scan done: **no single bit streams**. Stopped there (no multi-bit search).

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

Not run: `confirm` (no hit meets the streaming criterion).
