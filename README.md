# sdr-rtl

A pure-Rust RTL-SDR driver: the Realtek RTL2832U with a Rafael Micro
R820T / R820T2 / R828D tuner (RTL-SDR Blog V3 and V4 included), over
[nusb]. No libusb, no librtlsdr: this crate → nusb → the OS.

- Open by spec: `Rtl::open("")` (first found), `Rtl::open("0")` (index),
  `Rtl::open("serial=UHF")`.
- Tuning with the actual frequency derived from the PLL registers
  written; PLL lock read back.
- Gain as one number or per stage (LNA, mixer, VGA — 16 levels each, dB
  from the tuner's step tables), tuner and demodulator AGC, bias tee.
- Sample rate via the RTL2832U's resampler, tuner bandwidth.
- Streaming through [`sdr-core`]'s bulk-IQ engine: `rx_stream()` gives a
  blocking `recv_into`, or `into_stream()` an async `IqStream`; host-side
  loss is inferred and counted, never silent.
- The configuration EEPROM: read, parse, compose a rewrite (change only
  the given strings), write with a verified readback — including parts
  that mirror 128 bytes.

```rust
use sdr_core::TuneRequest;
use sdr_rtl::Rtl;

let mut rtl = Rtl::open("")?;
rtl.set_rate(2.4e6)?;
rtl.set_freq(&TuneRequest::auto(100.1e6))?;
rtl.set_gain(30.0)?;
let mut stream = rtl.rx_stream(0, 0)?;
```

[nusb]: https://crates.io/crates/nusb
[`sdr-core`]: https://crates.io/crates/sdr-core

License: MIT OR Apache-2.0.
