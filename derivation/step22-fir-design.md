# Step 22 — FIR candidates for P25 channels anywhere in a 2.4 Msps window (2026-10-12) — MODELLED, BENCH PENDING

Every number below is from the model unless it is marked **measured**.
Program: `crates/sdr-rtl/examples/firdesign.rs` (std only, default
features). Outputs: `data/step22-fir-candidates.jsonl` (ours and the eight
candidates), `data/step22-fir-predict.jsonl` (each set's modelled sweep
over ±6 MHz, for the bench), `data/step22-fir-validation.txt` (the model
against the records), `data/step22-fir-chain.jsonl` (the model's tables).

Abbreviations: FIR, finite impulse response filter (the 20 tap registers,
findings 12.4); P25, Project 25; IF, intermediate frequency; DDC, the
demodulator's digital down-converter; RF, radio frequency; ADC,
analogue-to-digital converter; SINR, signal to interference-plus-noise
ratio; p10, the 10th percentile over channel positions; A/B, the
two-driver comparison bench (`docs/rtl-ab-bench.md`).

## 1. Clean room

Inputs: this crate's step-12 records, findings 4g/4h/12/15, `taps.rs`,
`demod.rs` (the IF, f_symbol = 4 × the output rate) and
`tables/if_filter.rs` (IF filter corners), and the caller's 2026-10-12
measurements quoted in the brief. The program reads from the records only
our design, the chip's power-on image (`step12-poweron.jsonl`) and field
lists (`fields:`, `single:`, `pair:`); any other set's lines are skipped
unparsed and counted (486 lines, all in `step12-fold-clean.jsonl`: the
third set there, which findings 12.6 calls the step-9 fixture). During
exploration a set-name listing of that file printed that set's register
image as its name; it was not decoded, compared with or used. No other
driver's code or coefficient table was read or recalled; no published
table came to mind while working, so none had to be set aside. Every
candidate comes from windowed-sinc seeds (Hamming, Kaiser) and an integer
search on the model described next.

## 2. The model, and what is new in it

At output rate R (FIR at f_xtal = 28.8 MHz, f_symbol = 4R = 9.6 MHz at
2.4 Msps, IF 3.57 MHz, local oscillator above the RF):

1. **Two copies of every RF component reach the FIR**: at ν = δ, and at
   ν = 2·IF − δ = 7.14 MHz − δ. The tuner's output is one real IF
   (documents.md, Figure D), so the DDC passes both sides of it.
2. **After the FIR, the chain drops to f_symbol with no filter of its
   own**, then decimates by 4 through a fixed filter D4:
   D(ν) = D4(fold(ν, f_symbol)). D4 is findings 12.5's measured chain
   (the c15-only sweep less its own FIR). This is new: 12.5 measured only
   the direct landings to ±6 MHz. The mirror copies in the same c15-only
   sweep show the chain passes **ν = 8.5–10.5 MHz (9.6 ± 1.0) within
   0–3.6 dB** (a tone at RF −2.0 MHz with c15 alone puts a line at
   −0.46 MHz only 7 dB under the in-band tone). At 2.4 Msps the
   programmable FIR is the only filter at 9.6 ± 1.2 MHz, and a dense band
   fills that region at full strength through the mirror of the RF at
   −1.3…−3.7 MHz.
3. Every ν = L + k·R (k ≠ 0, one 28.8 MHz period, 11 terms) lands on the
   channel at L, weighted by the band's occupancy S(ν) (the IF filter of
   findings 4g at g = |fold(ν − IF, 28.8 MHz)|), |H(ν)/H(0)|² and D(ν)².
   The tuner's own image (findings 15: ≥ 44 dB, mostly ≥ 62 dB) does not
   depend on the FIR and is left out.

D4 at 2.4 Msps (dB re 0 Hz, both RF sides averaged where the IF filter
is flat): 0 to 0.8 MHz within ±0.5; 1.0 −1.5; 1.1 −3.7; 1.2 −6.3; 1.3
−11.8; 1.4 −19.2; 1.5 −38.0 (a null); 1.6 −32.3; 2.0 −32.1; 2.4 −35.0;
2.8 −38.8; 3.6 −45.6; 4.0–4.8 −46…−49, the last at the bench's floor
(used as the value: an upper bound). The design uses D4's upper envelope
over ±100 kHz so no candidate leans on its narrow nulls; reported figures
use D4 itself.

## 3. Validation against the records

`data/step22-fir-validation.txt`, all sweeps of allowed sets:

- **Direct path** (tone where it lands, the c15-only chain at the same δ
  plus the set's FIR): at 2.4 Msps, in band, mean −0.1…+0.1 dB, rms
  0.5–0.6 dB; beyond Nyquist, rms 0.7–1.4 dB, worst +4.5 dB (ours, power-on,
  C2, C4, D1, D3). 2.048 Msps is similar except at the power-on set's and
  three candidates' deep nulls (single points to −31 dB). At 2.88 Msps,
  in band rms ≤ 0.8 dB; beyond Nyquist rms ≤ 2.2 dB, worst +7.5 dB.
- **Mirror path** (D = D4(fold(ν, f_symbol)), the line at fold(7.14 MHz − δ)),
  2.4 Msps: c15 alone n 77, mean −1.0, rms 1.7 dB. **Four other sets
  (C2, C4, D1, D3, none used to build D4): n 33–38 each, mean −0.5…−0.8,
  rms 0.8–1.2 dB.** For ours (deep stopband) no mirror line is reported,
  and the model keeps all 180 points under the weakest reported line.
  2.048 Msps holds too (rms 1.1–1.8 dB). **2.88 Msps fails** (mean +10,
  worst +43 dB): the fractional 2.5× step to f_symbol is not modelled,
  and nothing below is claimed for 2.88 Msps.
- **filterfit's figures at 2.4 Msps** (worst alias re the centre inside
  ±0.7 / ±0.8 / ±1.0 MHz; spread over ±0.96 MHz), model against record:
  ours 45.2 / 42.0 / 26.9 / 4.5 against **measured** 42.8–44.7 /
  41.3–41.8 / 25.8–26.3 / 4.6–4.9; power-on 30.7 / 29.7 / 17.7 / 2.0
  against **29.4–30.9 / 17.8 / 2.9**; D1 41.7 / 38.5 / 23.5 / 2.1 against
  **40.4 / 37.6 / 22.7 / 2.5**; D3 within 1.1 dB; C2 within 1 dB; C4
  **5.5 dB optimistic** at ±0.7/±0.8, where its alias sits in a FIR null.
  So the model is good to ~1–2 dB, and optimistic where a figure depends
  on a deep null.
- **The 2026-10-12 tone test, ours** (re the centre; the bench quote in
  brackets): +1.6 → −0.8 MHz 42.9 [**43–44**]; −1.6 → +0.8 42.9
  [**37–42**]; +2.0 → −0.4 50.6 [**52–59**]; +1.3 → −1.1 18.4 [**~16**].
  The 300 MHz chain reproduces the 852 MHz bench within ~2–8 dB, mostly
  within 2.

### Does the old driver's 65 dB at +1.6 MHz contradict 12.7?

The analysis supports **the analogue side (ahead of the DDC)**, not a
missing piece of the post-FIR chain. |H| of a symmetric real FIR and D4
are both even in frequency, so nothing after the DDC can tell +1.6 from
−1.6 MHz. The old driver reads **65 dB at +1.6 but 40 at −1.6**, which is
within ours' **37–42**. Whatever the old FIR does at ±1.6 MHz is
therefore worth no more than what ours does on the −1.6 side; the extra
~21 dB on the + side was made before the DDC. All four points fit a
one-sided analogue edge between RF +1.3 and +1.6 MHz on the old driver:
+1.3 → 16 dB like ours, +1.6 and +2.0 → 65, −1.6 → 40.

One mechanism already in our records: the IF passband falls below about
0.5 MHz IF for every filter code (findings 4g). That edge sits at
RF = IF − 0.5 MHz, which is +3.07 MHz at our IF of 3.57 MHz, out of the
tone test's reach. A lower IF would move it inward. This is a hypothesis
with a measurement (section 7), not a finding. The 12.7 statement "the
chain's cut-off caps it near 44 dB" holds only under 12.7's flat-passband
constraint: allowing droop, the FIR alone reaches 55–66 dB modelled at
+1.6 → −0.8 (B1, A08 below).

## 4. Noise and the objective

- **Quantisation (after the FIR, flat)**: the tone-off median with the
  FIR's DC gain at 0 (`step12-narrow-weight`, pair 7:127 / 15:−127) is
  −81.5 dBFS per bin; **measured**. The ideal 8-bit floor in the bench's
  normalisation is −84.4.
- **ADC (ahead of the FIR, shaped like the signal)**: ours' median
  (−76.6) less the above is −78.3 dBFS per bin; **measured**, derived.
- A dense band whose composite sits at **−12 dBFS** at the output (an
  assumption: 12 dB of headroom for many carriers' peaks) puts these at
  **q = −35.1 dB** and **n = −31.9 dB** relative to one channel. The
  quantisation term scales with the output's mean power, because the gain
  ahead of the output is what sets the composite, so a filter cannot buy
  SINR with gain. Most of the window therefore sits at ~31 dB, the ADC's
  cap, and the FIR moves the positions under it.
- Per landing L on a 10 kHz grid over ±1.18 MHz:
  SINR(L) = G(L) / (A(L) + n·G(L) + q·P_out).
- Objectives (larger is better), each over the integer fields (c0–c7 in
  −128…127, c8–c15 in −2048…2047), DC gain exactly 4,096, and a guard:
  |H/H(0)| ≤ −45 dB from 3.6 to 14.4 MHz (ours holds −51.6). Beyond
  3.6 MHz D4 is a floor-limited bound, and other rates fold there
  (12.7's lesson at 2.88 Msps). A −40 dB guard gains at most 0.6 dB of
  p10 (the run with −40 is described here only).
  - **B1** mean SINR (dB) over the window; **E1** mean over |L| ≥ 0.6;
    **M1** mean over |L| ≤ 0.8;
  - **A08 / A10**: alias only, the soft minimum over |L| ≤ 0.8 / 1.0 MHz
    of the worst single alias re the landing (a tone at R − u);
  - **H1** hedged: B1's mean averaged with the same at the output 10 dB
    under its target (q + 10 dB), so it should not lose if the gain runs
    low;
  - **AB1**: H1 with the band only to ±2.4 MHz (the A/B's 4.8 Msps beds);
  - **F1**: H1 with nothing beyond ±1.2 MHz (a flat-passband control).
- Search: pair moves (one field +s, another −s, s = 128…1) to a local
  optimum from 37 seeds, then 24 basin hops. Deterministic (same output
  every run, checked).

## 5. Results (2.4 Msps)

Fields c0…c15; droop is the FIR alone (dB at |L| = 0.8 / 1.0 / 1.1 MHz);
"dense" is the mean / p10 SINR over the window and the mean over
0.9 ≤ |L| ≤ 1.18; "low" is the same mean / p10 with the output 10 dB
under target; "no spill" has nothing beyond ±1.2 MHz; tones are single-tone
rejection re the landing (dB) for +1.6 → −0.8, +2.0 → −0.4 and
+1.3 → −1.1; the mirror column is −2.0 → −0.46 and −3.0 → +0.54.

| Set | c0 … c15 | droop | dense | edge | low | no spill | tones | mirror | FIR max 1.6–3.6 / 8.4–10.8 |
|---|---|---|---|---|---|---|---|---|---|
| ours | −6 −5 −5 −2 3 15 34 61 96 138 185 233 279 317 345 360 | −2.3/−3.7/−4.5 | 27.0/13.4 | 16.7 | 22.3/12.6 | 29.8/27.7 | 41.0/49.5/10.1 | 66.0/56.7 | −10.6/−53.7 |
| **H1** | 6 17 28 46 69 91 112 127 156 170 187 195 204 210 213 217 | −6.1/−10.1/−12.8 | 27.7/19.4 | 19.6 | 21.9/12.9 | 28.6/23.0 | 56.0/54.6/16.3 | 58.3/60.6 | −23.9/−49.8 |
| **B1** | 11 29 47 66 88 107 123 127 142 147 160 171 187 203 215 225 | −7.0/−11.9/−15.3 | 27.6/20.0 | 19.6 | 21.5/11.4 | 28.2/21.2 | 54.9/80.3/19.3 | 62.9/62.4 | −23.6/−48.9 |
| E1 | 14 29 48 67 90 107 123 127 140 147 158 171 185 204 213 225 | −7.1/−12.1/−15.5 | 27.6/20.0 | 19.6 | 21.4/11.3 | 28.1/21.1 | 54.9/97.1/19.7 | 53.7/63.4 | −23.4/−51.1 |
| **M1** | −3 2 12 30 47 70 94 121 140 165 186 208 225 239 253 259 | −4.6/−7.5/−9.3 | 27.5/16.4 | 18.5 | 22.2/13.6 | 29.2/25.3 | 54.4/61.7/12.8 | 83.7/74.9 | −26.2/−54.7 |
| **A08** | 10 21 36 47 66 84 104 121 141 157 175 191 209 220 230 236 | −5.9/−9.7/−12.1 | 27.5/17.8 | 19.0 | 21.9/13.0 | 28.8/23.6 | 66.1/69.4/14.4 | 74.1/78.8 | −33.5/−52.7 |
| A10 | 18 33 53 71 92 109 120 127 134 142 151 167 184 204 216 227 | −7.3/−12.4/−15.9 | 27.5/19.8 | 19.4 | 21.4/11.0 | 28.1/20.8 | 57.2/64.2/19.7 | 57.2/56.8 | −21.9/−51.0 |
| AB1 | 7 19 31 49 71 93 116 127 155 170 184 195 201 205 211 214 | −6.2/−10.5/−13.4 | 27.6/19.7 | 19.6 | 21.8/12.7 | 28.5/22.6 | 54.3/54.8/17.0 | 63.4/**43.8** | −23.7/−45.1 |
| F1 | 12 9 2 −16 −38 −63 −85 −89 −66 −6 89 213 355 487 593 651 | +0.5/+0.5/+0.4 | 26.0/11.2 | 14.9 | 21.7/10.9 | **30.1/29.3** | 33.9/35.0/8.3 | 54.3/54.8 | −0.7/−49.8 |

Notes on the table:

- E1 and AB1 converge on B1 and H1 (fields within a few counts, figures
  within 0.3 dB). AB1 lets the 9.6 MHz region slip (a −3.0 MHz tone's
  mirror at 43.8 dB) because its band stops at ±2.4 MHz. Neither is
  proposed for the bench.
- Every dense-band candidate has c7 = 127: the 8-bit fields bind (they
  want larger outer taps).
- Tone figures above ~57 dB re the centre are under the bench's floor and
  will read as floor. Figures that depend on a FIR null (E1's 97, B1's 80
  at +2.0 MHz) carry C4's lesson: the model was 5.5 dB optimistic there.
- At 2.048 Msps (the open rate; model validated there) every dense-band
  candidate scores at least ours: mean 27.2–27.5 against 26.8, p10
  15.5–18.4 against 13.4, worst alias inside ±0.8 MHz 40–52 against 33.8.
  Spread over ±0.82 MHz is 5–8 dB against 2.7.
- The JSON lines carry each set's register image (hex), the full
  profiles (gain, alias, SINR every 0.1 MHz), all eight tone points with
  their mirror landings, the stopbands, and the sensitivity rows (narrow
  IF filter, the D4 envelope, q ± 10 dB, the A/B band, no spill).

### What a FIR change can and cannot buy (modelled)

- **Edges, dense band:** p10 13.4 → 19–20 dB and the 0.9–1.18 MHz mean
  16.7 → 19.6 dB (H1/B1). At |L| = 1.0 MHz SINR goes 21.0 → 24–25 dB; at
  1.1 MHz, 10.0 → 15–16 dB. Beyond ~1.1 MHz the alias comes from just
  past the edge through D4's fixed transition (−6 dB at 1.2, −19 at 1.4)
  and no 32-tap FIR at 28.8 MHz removes it.
- **Strong single carriers** (the case uniform-band SINR hides): the
  rejection a channel at 0.4–0.8 MHz gets from one carrier 1.6–2.0 MHz
  out rises from 41–50 dB (ours) to 54–56 (H1), 55–80 (B1) and 66–69
  (A08). On the 800 MHz band (strong and weak carriers mixed) this is the
  term most likely to show.
- **The price is droop**: 6–12 dB more at 1.0–1.1 MHz than ours. It
  costs against the output's quantisation floor only when the output runs
  under its target. With the output 10 dB low ("low" column), H1 and A08
  give up 0.3–0.4 dB of mean and gain 0.3–0.4 dB of p10 against ours,
  while B1 loses 0.8 / 1.2 dB. With nothing outside the window ("no
  spill") ours beats every dense-band candidate (p10 27.7 against
  21–25), and only F1 is better (29.3).
- **Mid-window in a uniform dense band** is already at the ADC's cap with
  ours (aliases −41 to −63 dB under the channel). The FIR cannot lower
  the ADC floor, and the A/B's −0.6 points near the centre are not a FIR
  matter in this model.
- **Not the FIR's**: the ±1.6 MHz lopsidedness (analogue, section 3); the
  tuner image; the composite level the gain distribution leaves at the
  output (the "low" column). Since the A/B matched levels on a tone, the
  drivers' different stage gains (`docs/rtl-ab-bench.md`: old 20 ≈ new
  40) may also leave different ADC loading. Whether that explains the
  busy-band floor being 0.5–3 dB higher is unmeasured.

## 6. Which to bench first

1. **H1**: the hedge. Best or equal dense-band mean, p10 +6 dB, single-tone
   rejection +10–15 dB at 0.4–0.8 MHz, about level with ours if the output
   runs low.
2. **M1**: the cautious one. Least extra droop (+3 dB at 1.0 MHz), the
   deepest 9.6 MHz stopband (−54.7), +13 dB on +1.6 → −0.8 and +12 on
   +2.0 → −0.4. Level with ours when the output runs low (mean −0.1,
   p10 +1.0) and behind only on "no spill" (p10 25.3 against 27.7).
3. **A08**: the alias-only design: the most strong-carrier rejection
   (66–69 dB modelled at 0.4–0.8 MHz landings), mid droop.
4. **B1**: only if H1 wins and the output is measured near its target
   (it is the more aggressive version of H1).
5. **F1** as a control: it should lose at the edges on 4.8 Msps beds. If
   it wins, droop matters more than aliasing on the bench, and this model's
   weighting is wrong.

The sets apply per rate: designed and validated for 2.4 Msps, also
modelled at 2.048. 2.88 Msps is not modelled, and the application carries
P25 only at multiples of 2.4 Msps (`docs/rtl-ab-bench.md`). Shipping one
would need the driver to load taps per rate.

## 7. The bench: what to measure and how to decide

1. **Sweep (model check, no decoding).** `filterstudy matrix --rate
   2400000 --from -6000000 --to 6000000 --step 50000` with `--set
   fields:<c0,…,c15>` for ours, H1, M1, A08 (and F1), the USRP tone
   source through the pad as the A/B bench cables it (`--source-cmd`), at
   8/8/2. Compare each set with its lines in
   `data/step22-fir-predict.jsonl` (`db_re_ref` at `lands_hz`;
   `mirror_db_re_ref` at `mirror_lands_hz`).
   - **Accept the set's model** if the direct landings within 10 dB of the
     floor agree to ≤ 2 dB rms, and the droop at ±0.8, ±1.0 and ±1.1 MHz
     to ≤ 1 dB.
   - **Reject the set** if a mirror line for a tone at −3.7…−1.3 MHz (the
     9.6 MHz region) stands more than 6 dB above its prediction, or above
     −45 dB re the reference.
   - A second RF (852 MHz, the A/B's) repeats the check at the band's own
     analogue settings.
2. **The 2026-10-12 tone test with each set** (852.30625 MHz, same
   dongle, same gains), rejection re the centre. Modelled: H1 +1.6 → −0.8
   61.6 (floor-limited ~57), +2.0 → −0.4 56.6, +1.3 → −1.1 32.9, −1.6 →
   +0.8 61.6. M1 58.5 / 63.4 / 25.8 / 58.5. A08 71.5 / 71.4 / 30.2 / 71.5.
   Ours 42.9 / 50.6 / 18.4 / 42.9. Also read the mirror landings (e.g.
   H1's −2.0 MHz tone at −0.46 MHz, 58 dB). **Reject** a set that does not
   beat ours by ≥ 6 dB on ±1.6 → ∓0.8.
3. **The output's composite level** on a busy band and on each A/B bed:
   the dBFS root-mean-square level of the 2.4 Msps recording, both
   drivers, at the A/B's gain pairs. Within ~5 dB of −12 dBFS, the "dense"
   column applies. ≥ 10 dB under it, the "low" column applies, and before
   any FIR the gain distribution is the first thing to fix (more analogue
   gain, or more digital gain if the 8-bit fields allow it).
4. **A/B, same driver, taps only** (ours against H1, then M1 and A08),
   the harness's beds, pairs and both dongle configurations. Score by
   |offset| bands (0–0.3, 0.3–0.6, 0.6–0.9, 0.9–1.18 MHz). **Accept** a
   set that gains ≥ 1 point in 0.6–1.18 MHz with a sign test p < 0.05 over
   channels, and loses none significantly in 0–0.6 MHz. Then A/B the
   accepted set against the old driver.
5. **The lopsidedness hypothesis** (section 3): sweep +1.0…+2.5 MHz and
   −1.0…−2.5 MHz in 100 kHz steps on the old driver (black-box: its output
   only), to find where its extra rejection begins. Repeat on ours with a
   bench build whose DDC IF and tuner LO offset are lowered so that the
   IF filter's 0.5 MHz low edge falls near RF +1.4 MHz, and check that
   the image (findings 15) stays down. If ours then reads ~60 dB at +1.6
   and ~16 at +1.3, the old driver's advantage is IF placement. That is
   an analogue lever worth up to ~20 dB on one side, more than any FIR
   can give at ±1.6.

## 8. Limits

- D4 comes from one RF (300 MHz), one dongle, one gain setting (8/8/2),
  and a 50 kHz grid. Beyond 3.6 MHz it is floor-bounded (hence the
  guard).
- The IF filter's skirt is modelled from its corners and the "dB at 2 MHz
  past" column, with a 6.5 dB/MHz slope, −3 dB at the 0.5 MHz low edge,
  and a −60 dB floor. The wide setting is used; the narrowest (7.28 MHz
  corner) changes no mean or p10 by more than 0.01 dB. At 2.4 Msps the
  energy that matters lies within ±3.7 MHz of RF and in the mirror of
  −1.3…−3.7 MHz, inside every setting.
- A uniform dense band; real bands are lumpy. The single-tone columns
  cover the lumpy case.
- The 8-bit output floor and the ADC floor are tone-off medians at the
  bench's gains. Field gain settings move n and q.
- 2.88 Msps is not modelled (the mirror check fails there).

## Replay (from the workspace root)

```
cargo run --release -p sdr-rtl --example firdesign -- validate > crates/sdr-rtl/derivation/data/step22-fir-validation.txt
cargo run --release -p sdr-rtl --example firdesign -- chain    > crates/sdr-rtl/derivation/data/step22-fir-chain.jsonl
cargo run --release -p sdr-rtl --example firdesign -- design   > crates/sdr-rtl/derivation/data/step22-fir-candidates.jsonl
cargo run --release -p sdr-rtl --example firdesign -- predict 6,17,28,46,69,91,112,127,156,170,187,195,204,210,213,217
cargo run --release -p sdr-rtl --example firdesign -- score   <c0,…,c15>
```

`design` takes about 50 s and gives the same output on every run.
