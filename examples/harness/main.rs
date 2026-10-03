//! A bench harness that establishes, from the vendor documents plus
//! exhaustive black-box measurement, how to talk to an RTL2832U with an
//! R820T-family tuner and how to make it stream raw IQ.
//!
//! It talks to the dongle through nusb directly (its own vendor-request
//! layer in `usb.rs`, written from the RTL2832U datasheet's Tables 27-28),
//! never through the sdr-rtl library. Every measurement writes one record
//! under `derivation/data/`: what was measured, the candidate space, the
//! settings held fixed, every candidate's raw result and the harness
//! version. Records carry no device serials, host names or port paths.
//!
//! ```text
//! cargo run --release -p sdr-rtl --example harness -- <step>
//!   recovery    step 0: prove power-cycle recovery through the analyzer
//!   protocol    step 1: vendor-request encodings against documented resets
//!   protocol-readonly  step 1 phases A-B only: no power cycle, no writes
//!   tuner       step 2: tuner access through the I2C repeater
//!   baseline    step 3: the documented-only streaming baseline
//!   scan        step 3: the exhaustive single-bit scan (resumable)
//!   confirm P O B   step 3: confirm a scan hit (page, offset, bit)
//!   bytescan [--limit N]  step 3: every byte value of demodulator pages
//!               0-2 (resumable; at most N new candidates per run)
//! ```

mod bytescan;
mod json;
mod regs;
mod step0;
mod step1;
mod step2;
mod step3;
mod usb;

use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use json::J;

pub fn data_dir() -> PathBuf {
    let p = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/derivation/data"));
    std::fs::create_dir_all(&p).expect("create derivation/data");
    p
}

/// The harness version: crates/rtl's git short hash, marked when the
/// harness sources differ from it.
pub fn version() -> J {
    let dir = env!("CARGO_MANIFEST_DIR");
    let hash = Command::new("git")
        .args(["-C", dir, "rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let dirty = Command::new("git")
        .args(["-C", dir, "status", "--porcelain", "--", "examples/harness"])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty());
    J::obj()
        .with("crates_rtl_head", hash)
        .with("harness_uncommitted", dirty)
}

pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A record header every data file starts with.
pub fn header(step: &str, what: &str) -> J {
    J::obj()
        .with("step", step)
        .with("measured", what)
        .with("harness", version())
        .with("unix_time", unix_now())
        .with(
            "bench",
            "RTL2832U + R820T-family tuner, 28.8 MHz crystal, behind a USB analyzer with switchable TARGET-A power; no signal source, antenna input sees noise only",
        )
}

pub fn write_record(name: &str, rec: &J) {
    let path = data_dir().join(name);
    std::fs::write(&path, rec.render() + "\n").expect("write record");
    println!("wrote {}", path.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap_or(&path).display());
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("recovery") => step0::run(),
        Some("protocol") => step1::run(),
        Some("protocol-readonly") => step1::run_readonly(),
        Some("tuner") => step2::run(),
        Some("baseline") => step3::baseline(),
        Some("scan") => step3::scan(),
        Some("confirm") => step3::confirm(&args[1..]),
        Some("bytescan") => bytescan::run(&args[1..]),
        _ => Err("usage: harness recovery | protocol | tuner | baseline | scan | confirm PAGE OFFSET BIT | bytescan [--limit N]".into()),
    };
    if let Err(e) = result {
        eprintln!("harness: {e}");
        std::process::exit(1);
    }
}
