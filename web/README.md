# sdr-rtl over WebUSB

The proof page: sdr-rtl compiled to WebAssembly, driving an RTL2832U +
R820T dongle from a Chromium-family browser (Chrome, Edge, …; Firefox and
Safari have no WebUSB). Two buttons:

- **Stream check** — the browser's device chooser on first use, the
  EEPROM's identity, 4 s at 2.4 Msps read as wire bytes on one task while
  another retunes three times, then the chip's count of dropped blocks.
- **EEPROM round trip** — one free byte past the strings set to 0x55 and
  restored, each write read back, the whole part compared.

Build (from this directory; `wasm-bindgen` the command-line tool at the
version in `Cargo.lock`):

```
cargo build --release
wasm-bindgen --target web --out-dir pkg target/wasm32-unknown-unknown/release/sdr_rtl_web.wasm
```

Serve this directory over `http://localhost` (WebUSB needs a secure
context, which localhost is), for example `python3 -m http.server 8000`, and
open `http://localhost:8000/`.

Per platform: macOS works as is; Windows needs the WinUSB driver for the
dongle (e.g. through Zadig); on Linux the kernel's DVB driver must not hold
the dongle (blocklist `dvb_usb_rtl28xxu`) and the user needs access to the
device node.
