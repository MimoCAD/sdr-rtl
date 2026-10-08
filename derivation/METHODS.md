# Methods and practices

How this driver's values were derived, written so the same process can
bring up another chip (an E4000, an FC0013, a different demodulator)
without repeating our mistakes. [`README.md`](README.md) states the
provenance rules; this file is the working method behind them.

## 1. The clean room

- **Who may read what.** The person or tool writing derived values
  reads only: the vendor documents, our own extract of them
  ([`documents.md`](documents.md)), the bench records, the harness, and
  general USB / I²C references. They never read another driver's source
  code (any license), its comments, or pages describing its register
  sequences. That includes this crate's own earlier code if it had any
  outside provenance.
- **Separation of roles.** Anyone who has seen another driver's values
  may coordinate and run the bench, but may not choose a search, a
  narrowing or a value. Implementation goes to someone who hasn't. When
  the coordinator knows an answer, they say so and step back from that
  decision.
- **No knowing, only searching.** If a value "seems obvious", it is not
  used. It is either cited from a document or found by a search whose
  candidate space is stated before the search runs.

## 2. Read the documents first, as images

- Read vendor PDFs as page images. Extracted text has turned "–" into
  "A" and invented contradictions.
- Extract everything into one cited file: per fact, the table, page and
  exact numbers. Say plainly what the documents do *not* give.
- Name every self-contradiction (two tables disagreeing, a misprinted
  sign, a field width that does not fit its bits). Don't resolve it on
  paper; it becomes a bench question.

## 3. The bench

- **Instrument the wire before anything else.** A USB analyzer
  (Cynthion) on the dongle's port, capturing from the first request:
  - **Pin the endpoint explicitly.** An analyzer that auto-selects "the
    busiest endpoint" will watch the control pipe while the bulk pipe is
    silent. That cost us a false alarm.
  - Keep captures of every step as evidence. Redact string descriptors:
    the serial travels in enumeration.
- **Prove recovery before the first write.** Scripted power control
  (the analyzer's TARGET power switch) must make the device disappear
  and come back, measured. Then every later step can afford to wedge
  the chip. Allow a longer off-time retry; one short cycle once left the
  device absent.
- **One process on the hardware at a time.** The passive witness is the
  only exception. While it holds the analyzer, power recovery is
  unavailable, so keep captures short.
- **Cable, never radiate.** A signal source goes in through fixed
  attenuation (40–60 dB), never over the air, and stays below the
  receiver's absolute maximum input.

## 4. Finding the protocol with oracles

- **Documented reset values are the oracle.** To learn an undocumented
  encoding (how a register address is packed into a request), state
  every plausible form. Then pick the one under which reads return the
  documented power-on values. Score every form; the winner must stand
  far apart from the rest (ours: 58/59 against ≤ 3).
- **A known constant is a second oracle.** A chip ID register (the
  R820T's R0 = 0x96, sent LSB-first) confirms the tuner path.
- **Read first, write later.** Writes start only after the read path is
  proven, and every write is verified by read-back and restored.
- **Ablation.** Find the minimal preparation for each capability by
  removing one step at a time from a fresh power-on.
- **Start from a true power-on.** Values left by an earlier driver look
  like reset values. Power-cycle before reading anything you'll call a
  reset value.

## 5. Searches

- **A datasheet documents the product the vendor sold.** A mode the
  vendor never sold (raw samples out of a DVB-T demodulator) is, by
  that logic, outside the documented map. The documented pages are the
  natural first search space, but when they are exhausted the next
  spaces are the ones the design has room for and does not describe:
  request blocks the protocol names but never maps, the gaps and
  "reserved" ranges around documented registers, pages that read as
  mirrors. Cover every declared space once (breadth) before repeating
  any space with a variant hypothesis (depth).
- **Declare the candidate space up front, and cover it in plain order**
  (page, offset, value, ascending). Never reorder or cut by a hunch.
  - If time forces it, *reorder* by a mechanical rule written into the
    record header before the list is computed. Don't *cut*.
  - A confirmed hit is proof in itself. Exhaustiveness proves only that
    nothing steered the search.
- **Every candidate is one log line**, including skips (the register's
  current value) and failures. Make it resumable by key, and keep lines
  compact: our first full scan reached 54 MB.
- **Restart between candidates.** Each candidate runs from an identical
  state: documented reset, baseline, candidate, then any documented
  pipeline restart. Probe once which restarts revert which registers, and
  order the steps accordingly.
- **Run long scans in chunks** (`--limit N`), under a time limit, so an
  interrupted run loses at most one candidate.

## 6. Measurements must be able to see

- **Positive and null controls.** Before believing a long run of zeros,
  show the detector would see a positive. Ours: the transport-stream
  bursts at one register prove bulk data reaches the counter. Also show,
  on the wire, that the host was polling throughout every window.
- **Count everything, drop nothing.** Every transfer completion is
  recorded, errors by kind; a stale completion is still counted. Each
  window carries its own self-check (submitted / completed / errored /
  halted).
- **Clear endpoint halts.** A stalled bulk endpoint stops host polling
  until ClearFeature(ENDPOINT_HALT). A detector that only resubmits will
  read the halt as a quiet device.
- **Judge a record's validity explicitly.** Per record: valid / valid
  where proven live / not evidence, with the reason.

## 7. Records

- One file per step: what was measured, the candidate space, settings
  held fixed, every raw result, harness version.
- No device serials, host names or USB port paths. Never record EEPROM
  contents.
- [`findings.md`](findings.md) gets one section per step: the result,
  its evidence file, the replay command.
- Generated tables cite the record they came from and say "do not edit".

## 8. Measuring with a source

- **The meter is the positive control.** Before any gain table, show
  that a documented readback responds to the tone — off, on, and up a
  ladder of source levels — and record its direction, span, resolution
  and reversals. If none responds, there is nothing to measure with;
  stop and say so.
- **Levels are relative; fix the source within a comparison.** When a
  sweep would push the meter out of its range, move the source in exact
  steps (digital backoff before the analog gain), log every move, and
  read a gain difference as (meter difference) + (source difference).
- **Break the circle with a probe.** Seeing the tone needs a locked PLL
  and knowing the PLL locked needs the meter: probe every divider code
  at one frequency with off/on readings, the undescribed codes under
  declared hypotheses, and let the readbacks say which locked.
- **Remove the instrument's own response.** The demodulator's channel
  filter shapes every tuner-filter curve: either move its DDC with the
  tone, or divide by a reference curve taken at the widest codes, and
  say which was done.
- **A staged search states what it could miss** and runs the full space
  at sample points to check.
- **Never switch a clock off before proving nothing hangs on it.** A
  tuner's clock output may be the demodulator's crystal (the documents
  allow it; the board decides). Ours stopped answering USB at the write
  of that bit and needed a power cycle. "The demodulator answered before
  the tuner was powered" proved only that the GPIO did not cut the
  tuner's supply. Write power and clock bits last, one at a time, with a
  liveness check after each.
- **Take a level ladder in level order, and analyse it sorted.** A ladder
  taken as (coarse knob outer, fine knob inner) is not monotonic in
  level; reversal counts over it are artefacts. Sort by the relative
  level before judging monotonicity, and let the loop settle (a reading's
  spread taken too soon after a step shows it).
- **When hypotheses do not fit, sweep the stimulus instead.** For an
  undescribed divider code, placing the tone where each guessed divider
  predicts gave scattered hits; holding the PLL and stepping the tone
  across the whole source range found the LO directly (and showed two
  codes with no clean LO at all). A hypothesis list is a shortcut; the
  exhaustive sweep is the proof.
- **The meter must be outside the loop it measures.** A gain stage inside
  the closed AGC loop (the VGA) cannot be measured by the loop's own
  control value; open the loop and use the control value as a comparator,
  bisecting the source level to its flip. Say per stage which meter
  applies.
- **A probe below the detector's threshold proves nothing.** The divider
  probe at −30 dB saw nothing for most codes because the manual-mode
  readback is a comparator at about −8 dB; run probes at a level the
  meter has been shown to resolve.
- **The source's identity line is never recorded.** The client keeps the
  numeric fields of `ready` and nothing else.
- **The source must die unkeyed.** Run it in its own process group so a
  Ctrl-C stops the harness first, send `off` and `quit` on every exit
  path, and rely on EOF-unkeys as the backstop.

## 9. Onboarding another tuner (checklist)

1. Gather the documents; extract to a cited `documents.md`; list the
   gaps and contradictions.
2. Wire the analyzer, pin the endpoints, prove power recovery.
3. Establish the access path with oracles (chip ID, documented resets).
4. Write the probe steps as harness subcommands with declared candidate
   spaces; run them with the witness capturing.
5. Measure what the documents leave out (gain per code, filter
   bandwidths, band tables, PLL range) with a cabled source.
6. Generate the tables from the records. Implement from `documents.md`
   and the generated tables only.
7. Re-pin any traffic tests against the new behaviour; replay the
   harness to confirm.
