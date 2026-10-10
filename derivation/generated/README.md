# Generated tables

Rust source tables derived from the step 4 bench records under
`derivation/data/`, written by `examples/tables` (the generator's source
is `examples/tables/`). Every file here but this README is
generated: its first line names the generator, the record(s) it was read
from and the harness commit that wrote them, and says "do not edit".
Every number in a table is a record value or a stated formula over
record values; the method, resolution and caveats are in each table's
doc comment, taken from `derivation/findings.md`.

## Regenerate

From the repository root:

```
cargo run --release -p sdr-rtl --example tables
```

(`-- --data DIR --out DIR` to read other records or write elsewhere.)
Running it twice writes identical files; the generator's unit tests
check that, that every table is non-empty, and that the gain tables are
monotonic where findings.md says they are:

```
cargo test --release -p sdr-rtl --example tables
```

This README is written by hand and left alone by the generator.

## The files

| file | tables | from |
|---|---|---|
| `mod.rs` | the module tree (`pub mod` per file) | — |
| `types.rs` | the named types every table is declared with | — |
| `pll.rs` | `XTAL_HZ`, `REF_DIV2`, `F_REF_HZ`, `NINT_RANGE`, `VCO_RECEIVED_HZ`, `VCO_RANGE_HZ`, `SEL_DIV` (code → divider, with how each was established), `SWEEP_RUNS`, `LO_REACH`, `LO_UNREACHABLE_HZ`, `R2_VCO_INDICATOR` (per Nint × code), `R2_BY_LOCK_STATE` | `step4-pll-sweep.jsonl`, `step4-lo-find-sel1xx.jsonl` (findings.md steps 4c, 4d) |
| `gain.rs` | `LOOP_METER_COUNTS_PER_DB`, `LNA_GAIN_DB`, `MIX_GAIN_DB`, `VGA_GAIN_DB` (dB per code, comparator first column, the earlier record second) | `step4-gain-ladder-comparator.json` (step 4j), `step4-gain-ladder.json` (4i), `step4-meter.json` (4b, the slope) |
| `if_filter.rs` | `REFERENCE`, `FILT_CODE_UPPER_3DB`, `FILT_BW_UPPER_3DB`, `HPF_EFFECT`, `REFERENCE_TWO_STATE_DB`, `HPF_HAS_NO_MEASURABLE_EFFECT` | `step4-filter.jsonl` (step 4g) |
| `band.rs` | `BYPASS`, `IF_HZ`, `STAGE_A_DEAD_COUNT`, `BEST_PATH_COUNT`, `BAND_GRID` (per grid frequency: best path, corner codes, readings vs bypass, the 16-path stage-A matrix), `BAND_PATH_SEGMENTS`, `BAND_SEGMENTS` | `step4-band-table.jsonl` (step 4f) |
| `tuner_image.rs` | `TUNER_IMAGE` (R5–R31 with the reason per register) | `step4-tune-300.000MHz.json` (the harness's `tuner::IMAGE_NOTES` as the record copied them) — documented choices, not measurements |

The meter behind every dB that is not a comparator flip level is
`if_agc_val` (MSB-first) with the IF AGC loop closed through the VAGC
pin (findings.md step 4b); its scale is fitted from `step4-meter.json`
(about 207 counts per dB) and used by the gain, filter and band tables.

## Validity caveats

- **Band table, single tone.** Each setting's reading is its passband
  gain at the tone and nothing else: the corner codes (TF_NCH, TF_LP)
  move the best path's reading by little (`band.rs`'s doc comment gives
  the median and the worst case from the record) and their rejection of
  an out-of-band tone is unmeasured. The best corner codes
  of a single-snapshot reading are set by the loop's two-state
  alternation (about 1.7 dB) more than by the filter, so
  `BAND_SEGMENTS` is the record's answer, not a resolved table;
  `BAND_PATH_SEGMENTS` (RFFILT, RFMUX only) is the robust part. Stage A's
  16-path matrix is indicative: a reading 20 ms after a strong path can
  still show that path, so the best path is decided by stage B and the
  table says where the two disagree. The source was re-levelled once
  per frequency at the bypass path and held, so readings are relative
  to bypass at that frequency, not across frequencies.
- **Image rejection is uncalibrated.** The wanted sideband is below the
  LO and the upper sideband is rejected by only 10–20 dB with
  IMR_G = IMR_P = 32 (findings.md step 4h); no table here calibrates it.
- **The LO gap.** No SEL_DIV code puts an LO between about 114 and
  214 MHz with the VCO inside `VCO_RANGE_HZ` (RF about 110–208 MHz with
  the 4.57 MHz IF); `BAND_GRID` marks 112–202 MHz unmeasured and
  `LO_UNREACHABLE_HZ` states the span. Whether the VCO runs lower under
  settings the sweep did not vary is an open search.
- **PLL.** The /1 divider could not be tested (its LO is above the
  source); SEL_DIV 110 and 111 are unusable; the sweep's Nint step is 5,
  so the VCO's received bounds are known to one step (144 MHz).
  `VCO_RANGE_HZ` is a choice (the received bounds moved 30 MHz inward,
  rounded inward to 10 MHz: 1.71–3.65 GHz); the harness's own
  `tuner::VCO_RANGE_HZ` is (1.70, 3.65) GHz, 10 MHz lower at the bottom.
- **Gain.** 1 dB resolution; levels above 0 dB rest on the source's
  nominal txvga steps. VGA code 15 is beyond the measured range (a lower
  bound only). All three stages at 300 MHz with the other two at code 8.
- **IF filter.** The curves carry the demodulator's response at the
  tone (the DDC tracked the tone to limit it); corners are at one LO;
  the HPF codes show no effect above 0.1 MHz within the reference's own
  alternation.
- **Register image.** Documented-choice values from [R820T2] Tables 1-2
  and 1-3 plus one bench-forced bit (R15 CLK_OUT_ENB = 0); R16–R31
  cannot be read back on this board.
