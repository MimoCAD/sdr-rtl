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
- A `Stream` that cannot lose samples unseen. The chip marks nothing in
  its sample stream, and a control request served while samples flow can
  cost some; so the receiver streams by becoming a `Stream`, which offers
  only what keeps every sample:
  - `std::io::Read`: the wire bytes, unsigned 8-bit I then Q — the
    layout RTL-SDR tools read and write. `recv_into` hands out the same
    samples widened to 16-bit I/Q.
  - Retune, gain and filter changes inside a pause that loses nothing;
    each change is a `Break` at an exact sample position.
  - `split()` into a `ReadHalf` and a `Control`, for a reading thread and
    a changing one; `Stream::reunite` joins them again.
  - `finish` hands the receiver back with a `StreamReport`: the samples
    delivered and the chip's own count of every block it dropped.
- The configuration EEPROM: read, parse, compose a new identity, write
  page by page with a verified read-back.
- `no_std` at its core (with `alloc`): the register encodings, the
  synthesizer, gain, filter, taps and the EEPROM layout build for bare
  metal with `default-features = false`; the default `std` feature adds
  the receiver itself (the USB transport, `Rtl`, `Stream`).

```rust
use std::io::Read;
use sdr_core::TuneRequest;
use sdr_rtl::{Rtl, Stream};

let mut rtl = Rtl::open("")?;
rtl.set_rate(2.4e6)?;
rtl.set_freq(&TuneRequest::auto(100.1e6))?;
rtl.set_gain(30.0)?;
let (mut reader, control) = rtl.into_stream(0, 0).map_err(|e| e.error)?.split();

// Read on one thread …
let reading = std::thread::spawn(move || {
    let mut buf = [0u8; 16384];
    let n = reader.read(&mut buf)?; // I, Q, I, Q, … as unsigned bytes
    std::io::Result::Ok(reader)
});

// … retune on another; samples from `at.settled_at` on are 101.1 MHz.
let (_, at) = control.retune(&TuneRequest::auto(101.1e6))?;

let reader = reading.join().unwrap()?;
let finished = Stream::reunite(reader, control).ok().unwrap().finish()?;
// finished.report.dropped_blocks: the chip's count of blocks lost in the
// run; finished.rtl: the receiver, ready for the next stream.
```

[nusb]: https://crates.io/crates/nusb

License: MIT OR Apache-2.0.
