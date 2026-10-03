# Bench findings

Replay: `cargo run --release -p sdr-rtl --example harness -- <step>`

Status: recovery proven; step 1 (encodings) and step 2 (tuner access) done; step 3 baseline + exhaustive single-bit scan done: **no single bit streams**; confirm of p1 0x92 bit 6 reads 0 B alone; the plain byte scan over pages 0–2 is complete (**no single byte streams**); the byte scan with restart is paused at 28976 of 196608 tested (through page 0 0x71 value 0xA1), 0 streams, wedges/errors only at page 0 0x10 and 0x20 (resume: `harness bytescan --restart --limit 15000`, repeated). The complete plain byte-scan record is stored gzip-compressed (`data/step3-bytescan.jsonl.gz`, 52 MB raw); `gunzip -k` it before re-running the plain scan. The bulk detector was checked on the wire (below): it polled endpoint 0x81 without a gap; a halted endpoint would have blinded it silently, which it now detects, clears and records.

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

## Step 3 — byte scan with restart (in progress) — `data/step3-bytescan-restart.jsonl` — `harness bytescan --restart [--limit N]`

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

The windows themselves (50 ms + 200 ms follow-up; 200 ms first-byte +
200 ms judge) were polled throughout: the old detector and the new one
give identical wire behaviour without a halt. Closing the conditional
part needs the scans re-run with the new detector (any STALL now shows
as `halts` > 0 and is cleared); none has been started.
