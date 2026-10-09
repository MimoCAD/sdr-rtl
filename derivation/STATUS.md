# Status of the derivation

One page: what is derived, what is open, and where the evidence is.
Dates are when the result landed; the records are under `data/`, the
reasoning in [`findings.md`](findings.md), the method in
[`METHODS.md`](METHODS.md).

## Derived (ready to build on)

| What | How | Evidence |
|---|---|---|
| USB vendor-request encodings: system/USB blocks (base+offset, little-endian), demodulator pages (offset<<8 \| 0x20 once the PLL runs), tuner through the I²C repeater | documented reset values as the oracle; 512 forms scored | findings 1A–1C, `data/step1-*.json` |
| Tuner access: I²C 0x1A via page 1 0x01 bit 3; reads LSB-first; R0 = 0x96 | sweep of all 128 addresses × 2 placements | findings 2, `data/step2-tuner-access.json` |
| The demodulator runs on the tuner's clock output: R15 CLK_OUT_ENB must stay 0 | test + null, power-cycle recovery | findings 4a |
| A power meter without raw samples: `if_agc_val` (MSB-first), −207 counts/dB, ±0.5 dB | tone ladder, every documented readback compared | findings 4b, `data/step4-meter.json` |
| PLL: f_LO = f_ref·2·(Nint+Nfra)/div with f_ref = 14.4 MHz; VCO 1.67–3.69 GHz; /2 /4 /8 confirmed; R2 is a lock indicator (70–127 inside) | 416-setting sweep with the tone at each predicted RF | findings 4c, `data/step4-pll-sweep.jsonl` |
| Undescribed divider codes: 100 = /32, 101 = /64; 110, 111 unusable | PLL held, tone swept 2–1300 MHz | findings 4d, `data/step4-lo-find-*.jsonl` |
| Fractional tuning works (300 MHz: /8, Nint 84, SDM 39504) | tone received at the predicted IF | findings 4e |
| Band table: best RF path per 10 MHz from 42–102 and 212–1002 MHz; RFFILT 11 and RFMUX 11 are dead codes | 45,936 readings; path decided from the 256-reading stage | findings 4f, `generated/band.rs` |
| IF filter: FILT_CODE corner 10.6 → 7.8 MHz (0.19 MHz/code); FILT_BW 10.6 / 8.8 / 7.3 MHz; HPF has no measurable effect | tone stepped across the passband | findings 4g, `generated/if_filter.rs` |
| High-side mixing: the wanted sideband is below the LO | tone above vs below the LO | findings 4h |
| Gain per code at 1 dB: LNA 0…36 dB, mixer 0…18 dB, VGA 3.57 dB/code | comparator bisection of the source level | findings 4i–4j, `generated/gain.rs` |
| Generated tables and the generator (`examples/tables`) | deterministic, cited per record | `generated/` |
| The driver's non-streaming half (`derived/`) | from the above only; 33 unit tests | `derived/src/` |

## Open

| What | State | Next |
|---|---|---|
| **Raw-sample output mode** (the thing that makes it an SDR) | Not in any document. Every single-write space covered once from the documented baseline with 0 streams: demodulator pages 0–2 (196,608, plain), pages 3–4 (65,536 each), the system block (65,536), the USB block (130,407; 155 values of the test-mode register deferred). The restart variant of pages 0–2: 196,608, complete, 0. About 790,000 single-write candidates in all, 0 streams. The only bytes ever seen are 188-byte transport-stream blocks. **The single-write hypothesis is exhausted.** 2026-10-09: the bit scan repeated from two other starting states — **low-if** (§9.1 IF mode, 4.57 MHz DDC, Spec_inv 1, tuner image tuned to 300 MHz, 2.4 Msps; live by the tone: `if_agc_val` 8191 → −902) and **power-on** (DEMOD_CTL PLL only) — 10,240 each, **0 streams**; neither baseline streams alone. New bytes: p0 0x07 bit 7 with the tuner programmed replays the endpoint FIFO's 20 × 188 B at 45 MB/s (rate independent of rsamp, tone absent, FIFO drop counter 255) — not samples | No further search started; the multi-write space is the project owner's decision. Evidence: findings "the bit scan from two other starting states", `data/step3-scan-lowif.jsonl`, `data/step3-scan-poweron.jsonl`, `data/step3-confirm-lowif-p0-07-bc.json` |
| Wire sample format | Follows from the above | — |
| RF 110–208 MHz | No divider reaches LO 114–214 MHz under the documented formula | Search for a lower VCO under other register settings |
| Image rejection | 10–20 dB at IMR_G = IMR_P = 32 | Calibrate IMR_G × IMR_P with the tone above the LO |
| Tracking-filter corner codes | Within the meter's noise with one tone | Second-tone rejection measurement |
| Bias tee | GPIO4 documented "antenna power, default off", but the reset value has its bit set | Measure the pin |
| PLL lock | Inferred from R2 | Accept, or find a documented lock bit |
| Bench | USB register 0x1C is a hazard: a wedge there plus the power cycle drops the whole hub branch (twice); only a hub restart recovers it. Its remaining 155 values are deferred; the block resumes at 0x1D | Restart the hub; `harness recovery`; `blockscan --block usb --from 29 0` |

## Resume commands (from the repository root of the workspace)

```
cargo run --release -p sdr-rtl --example harness -- recovery
cargo run --release -p sdr-rtl --example harness -- blockscan --block usb --from 29 0 --limit 12000
cargo run --release -p sdr-rtl --example harness -- bytescan --restart --limit 15000
cargo run --release -p sdr-rtl --example harness -- scan --baseline low-if --limit 3000 [--source-cmd "cargo run --release --example tone_source -- --cabled"]
cargo run --release -p sdr-rtl --example harness -- scan --baseline power-on --limit 3500
cargo run --release -p sdr-rtl --example harness -- confirm-write 0 0x07 0xBC --baseline low-if [--source-cmd "…"]
cargo build --manifest-path crates/rtl/derived/Cargo.toml && cargo test --manifest-path crates/rtl/derived/Cargo.toml
```
