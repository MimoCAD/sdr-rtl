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
//!   scan [--baseline zero-if|low-if|power-on] [--limit N]
//!               step 3: the exhaustive single-bit scan (resumable; at
//!               most N new candidates per run) from the documented
//!               zero-IF baseline (default, data/step3-scan.jsonl), the
//!               step-4 low-IF state with the tuner tuned (-lowif), or
//!               the power-on state (-poweron); see baselines.rs. A new
//!               record starts with the baseline measured alone (low-if:
//!               with --source-cmd, also the tone off/on control).
//!               --baseline low-if-open: low-if plus page 0 0x07 bit 7
//!               (the endpoint path held open), each window's content
//!               judged by content.rs; an interesting candidate ends the
//!               chunk and is confirmed (confirm-write) at once
//!               --baseline zero-if-open: the documented zero-IF input
//!               (9.1, 9.3) with the tuner tuned and the path held open
//!               (step 8; record data/step8-scan-zero-if-open.jsonl)
//!   fifo-watch [--baseline B] [--seconds S]  step 8: the replayed FIFO
//!               pattern window by window, tone off/on alternating: when
//!               it changes, and the new bytes (0x47 test)
//!   confirm P O B   step 3: confirm a scan hit (page, offset, bit)
//!   survey      step 6, read-only: demodulator pages 5-15 and the 8051's
//!               external data outside the scanned ranges (64-byte reads),
//!               two passes each, from the low-if-open held state
//!   confirm-write P O VALUE [--baseline B] [--windows N] [--buf BYTES]
//!               [--no-minimise] [--out PATH]  step 3: one byte written on
//!               a baseline: bytes/s at 2.4/1.2/2.048 Msps, the FIFO's
//!               dropped blocks, structure and spectrum of the bytes, tone
//!               off/on (low-if, --source-cmd), the baseline minimised
//!   bytescan [--limit N]  step 3: every byte value of demodulator pages
//!               0-2 (resumable; at most N new candidates per run)
//!   bytescan --restart [--limit N]  the same, each candidate followed by
//!               the documented demodulator and FIFO restarts
//!   bytescan options: --out PATH writes (and resumes) PATH instead of the
//!               record under derivation/data (a scratch run); --from P OFF
//!               [VAL] starts at page P, offset OFF, value VAL (hex with
//!               0x, else decimal)
//!   --no-cycle  (any step) open the dongle as found instead of power-
//!               cycling it first; the record says so. For runs under the
//!               USB witness, which holds the analyzer that switches power
//!   blockscan --block sys|usb|p3|p4 [--limit N] [--out PATH] [--from OFF [VAL]]
//!               step 3: every byte value at every address of the system
//!               block (0x3000-0x30FF), the USB block (0x2000-0x21FF) or
//!               demodulator page 3 / 4, plain method, one record per
//!               block (resumable; at most N new candidates per run)
//!   restart-probe  step 3: what each documented restart does to the
//!               demodulator's registers (fixes the --restart order)
//!   detector-check  step 3: the bulk detector's self-check across a
//!               deliberately stalled endpoint (USB_EPA_CTL bit 4)
//!
//!   Step 4, the tuner against a cabled tone (every one of these needs
//!   --source-cmd "<command line>", the tone source driven over stdin):
//!   clkout-check  no source: is the demodulator clocked from the tuner's
//!               clock output (R15 CLK_OUT_ENB) — alive with 0, with 1,
//!               after a power cycle
//!   meter       every documented level readback: tone off, on, and across
//!               source levels, per tuner configuration; a probe of every
//!               SEL_DIV code first
//!   pll-sweep   SEL_DIV x Nint: R0-R4 after each, the readbacks with the
//!               tone where each divider hypothesis predicts the LO
//!   lo-find --div CODE [--nint N]  the tone stepped across the source's
//!               range with the PLL held: where the receiver answers is
//!               the LO (no divider hypothesis)
//!   tune HZ     one frequency: PLL words, status, readbacks off and on
//!   gain-ladder LNA_GAIN, MIX_GAIN, VGA_CODE 0..=15, the meter re-levelled
//!   band-table  a frequency grid x RFFILT x RFMUX x TF_NCH x TF_LP
//!   filter      FILT_CODE, FILT_BW, HPF: the tone stepped across the IF
//!   if-choice   the DDC IF x Spec_inv x tone side: which readback finds it
//!
//!   rom-dump    step 5: read the ROM through GetROMCode (read-only); the
//!               image goes to ~/mimosdr-captures/rtl2832u-rom/ (outside
//!               every repository), the record holds only its size,
//!               SHA-256, the read method and the consistency check
//!   rom-dump --probe [ADDR:LEN ...]  GetROMCode reads with timing and
//!               the chip's liveness after each (prints only)
//!   rom-dump --xdata-check  blocks 1 and 2 at the same wValue, page
//!               nibble 0 and 1 (prints only)
//!   stream-confirm replay|rate|tone|pause|coeff-probe|coeffs|minimise|seldiv
//!               [--level TXVGA BACKOFF] [--gain LNA MIX VGA] [--windows N]
//!               step 10: the vendor driver's stream-start sequence
//!               (findings 9b/9h) from a power cycle, proven to be samples
//!               (rate, tone, pause), the coefficient registers, the
//!               minimisation, SEL_DIV 011; data/step10-*.json
//!   step 4 options (step4::USAGE): --freq HZ --div CODE --ref-div2 0|1
//!               --if HZ --lo-side high|low --config manual|vagc|auto
//!               --txvga N --backoff N --meter NAME LO HI up|down --page3
//!               --out PATH --grid-mhz N --full --nint-step N
//!               --ddc track|fixed --r30b6 0|1 --lna N --mix N --vga N
//!               --if-step HZ --if-max HZ --snapshots N --limit N
//! ```

mod baselines;
mod blockscan;
mod bytescan;
mod confirm_write;
mod content;
mod fifo_watch;
mod json;
mod meter;
mod regs;
mod restart;
mod rom_dump;
mod source;
mod step0;
mod step1;
mod step2;
mod step3;
mod step4;
mod stream_confirm;
mod survey;
mod tuner;
mod usb;

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use json::J;

static NO_CYCLE: AtomicBool = AtomicBool::new(false);

/// True when `--no-cycle` was given: setups open the dongle as found.
pub fn no_cycle() -> bool {
    NO_CYCLE.load(Ordering::Relaxed)
}

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

/// The header for the step-4 records: the bench now has the cabled source.
pub fn header_with_source(step: &str, what: &str) -> J {
    header(step, what).with(
        "bench",
        "RTL2832U + R820T-family tuner, 28.8 MHz crystal, behind a USB analyzer with switchable TARGET-A power; a signal source's transmit port cabled to the antenna input through a 20 dB fixed attenuator, nothing radiating; source levels relative only",
    )
}

pub fn write_record(name: &str, rec: &J) {
    let path = data_dir().join(name);
    std::fs::write(&path, rec.render() + "\n").expect("write record");
    println!("wrote {}", path.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap_or(&path).display());
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(i) = args.iter().position(|a| a == "--no-cycle") {
        args.remove(i);
        NO_CYCLE.store(true, Ordering::Relaxed);
    }
    if let Some(i) = args.iter().position(|a| a == "--source-cmd") {
        if i + 1 >= args.len() {
            eprintln!("harness: --source-cmd needs a command line");
            std::process::exit(2);
        }
        source::set_command(args.remove(i + 1));
        args.remove(i);
    }
    source::catch_interrupt();
    let result = match args.first().map(String::as_str) {
        Some("recovery") => step0::run(),
        Some("protocol") => step1::run(),
        Some("protocol-readonly") => step1::run_readonly(),
        Some("tuner") => step2::run(),
        Some("baseline") => step3::baseline(),
        Some("scan") => step3::scan(&args[1..]),
        Some("confirm") => step3::confirm(&args[1..]),
        Some("confirm-write") => confirm_write::run(&args[1..]),
        Some("survey") => survey::run(),
        Some("fifo-watch") => fifo_watch::run(&args[1..]),
        Some("bytescan") => bytescan::run(&args[1..]),
        Some("blockscan") => blockscan::run(&args[1..]),
        Some("restart-probe") => restart::probe(),
        Some("detector-check") => step3::detector_check(),
        Some("clkout-check") => step4::clkout_check(&args[1..]),
        Some("meter") => step4::meter(&args[1..]),
        Some("pll-sweep") => step4::pll_sweep(&args[1..]),
        Some("lo-find") => step4::lo_find(&args[1..]),
        Some("tune") => step4::tune(&args[1..]),
        Some("gain-ladder") => step4::gain_ladder(&args[1..]),
        Some("band-table") => step4::band_table(&args[1..]),
        Some("filter") => step4::filter(&args[1..]),
        Some("if-choice") => step4::if_choice(&args[1..]),
        Some("rom-dump") => rom_dump::run(&args[1..]),
        Some("stream-confirm") => stream_confirm::run(&args[1..]),
        _ => Err(format!(
            "usage: harness recovery | protocol | tuner | baseline | rom-dump [--probe ADDR:LEN...|--xdata-check] | scan [--baseline zero-if|low-if|low-if-open|power-on] [--limit N] | confirm-write P O VALUE | confirm PAGE OFFSET BIT | bytescan [--restart] [--limit N] [--out PATH] [--from P OFF [VAL]] | blockscan --block sys|usb|p3|p4 [--limit N] [--out PATH] [--from OFF [VAL]] | restart-probe | detector-check | meter | pll-sweep | tune HZ | gain-ladder | band-table | filter | if-choice; any step: --no-cycle; step 4: --source-cmd \"<command line>\" and {}",
            step4::USAGE
        )),
    };
    if let Err(e) = result {
        eprintln!("harness: {e}");
        std::process::exit(1);
    }
}
