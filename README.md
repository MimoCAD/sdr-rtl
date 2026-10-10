# sdr-rtl

A pure-Rust driver for the Realtek RTL2832U with a Rafael Micro R820T /
R820T2 tuner, over [nusb]. No libusb, no librtlsdr: this crate → nusb →
the OS.

It is written from the vendor documents and our own bench records, and
from nothing else: every register value cites a document table, a bench
step or a generated table. The derivation — the method, the records and
every finding — is in [`derivation/`](derivation/) (start with
[`STATUS.md`](derivation/STATUS.md)). What the bench has not established
is refused with the reason, never guessed.

- Open by spec: `Rtl::open("")` (first found), `Rtl::open("0")` (index),
  `Rtl::open("serial=00000001")`.
- Tuning 42–1002 MHz, the actual frequency from the synthesizer words
  written.
- Gain as one number or per stage (LNA, mixer, VGA — 16 levels each, dB
  measured on the bench), tuner and demodulator AGC, the IF filter.
- Output rates 1.024–2.88 Msps, with our own decimation filter taps.
- Streams that cannot lose samples unseen. The chip marks nothing in its
  sample stream, and every control request served while samples flow
  loses some; so a receiver streams by becoming the stream its rate
  allows, and each kind offers only what is safe at that rate:
  - `LosslessStream` (≤ 1.2 Msps): retune and gain changes inside a
    pause that loses nothing; each change is a `Break` at an exact
    sample position.
  - `AccountedStream` (≤ 1.8 Msps) and `RestrictedStream` (any rate):
    receive and finish only.
  - `finish` hands the receiver back with a `StreamReport`: the samples
    delivered and the chip's own count of every block it dropped.
- The configuration EEPROM: read, parse, compose a new identity, write
  page by page with a verified read-back.

```rust
use sdr_core::{RxSource, TuneRequest};
use sdr_rtl::Rtl;

let mut rtl = Rtl::open("")?;
rtl.set_rate(1.2e6)?;
rtl.set_freq(&TuneRequest::auto(100.1e6))?;
rtl.set_gain(30.0)?;
let mut stream = rtl.into_lossless(0, 0).map_err(|e| e.error)?;
let mut buf = vec![Default::default(); stream.max_num_samps()];
let info = stream.recv_into(&mut buf, 1.0)?;
let (_, at) = stream.retune(&TuneRequest::auto(101.1e6))?;
// Samples from `at.settled_at` on are 101.1 MHz.
let finished = stream.finish()?;
// finished.report.dropped_blocks: the chip's count of blocks lost in the
// run (0 in every bench run at this rate); finished.rtl: the receiver.
```

[nusb]: https://crates.io/crates/nusb

License: MIT OR Apache-2.0.
