//! The refused request at a stream's finish (findings 16.6, 16.7),
//! repeated without a power cycle between runs: an application's plain
//! sequence (open, one-number gain, tune, stream, finish) through the
//! public API alone.
//!
//!   finishloop [--runs N] [--rate SPS] [--seconds S] [--gain DB | --codes L M V]
//!              [--status] [--cycle] [--timeout S] [--no-start] [--depth N]
//!
//! Each run: open, gain (one number, or stage codes as `tiercheck`), rate,
//! tune 300 MHz, the tuner's status read (with `--status`), the stream
//! the rate allows, `start()` (unless `--no-start`: the
//! first receive starts it), receive for S seconds (`--timeout`, default
//! 1 s), finish. `--cycle` power-cycles before each run, as `tiercheck`
//! does. One JSON line per run.

mod bench;

use std::time::{Duration, Instant};

use sdr_core::gain::Stage;
use sdr_core::{Iq, RxSource, TuneRequest};
use sdr_rtl::Rtl;

fn arg<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// What a run varies.
struct Opts {
    rate: f64,
    secs: f64,
    /// One-number gain, or stage codes (LNA, mixer, VGA) as `tiercheck`.
    gain: f64,
    codes: Option<[usize; 3]>,
    status: bool,
    /// The receive timeout: `sdr_probe`'s 1 s, `tiercheck`'s 0.25 s.
    timeout: f64,
    /// Start with `start()` before the first receive (`sdr_probe`), or
    /// let the first receive start it (`tiercheck`).
    explicit_start: bool,
    /// Transfers in flight (0: the driver's default).
    depth: usize,
}

fn run_once(o: &Opts) -> Result<String, String> {
    let mut rtl = Rtl::open("").map_err(|e| e.to_string())?;
    match o.codes {
        Some([l, m, v]) => {
            for (stage, code) in [(Stage::Lna, l), (Stage::Mixer, m), (Stage::Vga, v)] {
                rtl.set_stage_level(stage, code).map_err(|e| e.to_string())?;
            }
        }
        None => rtl.set_gain(o.gain).map_err(|e| e.to_string())?,
    }
    rtl.set_rate(o.rate).map_err(|e| e.to_string())?;
    rtl.set_freq(&TuneRequest::auto(300e6)).map_err(|e| e.to_string())?;
    if o.status {
        rtl.tuner_status().map_err(|e| e.to_string())?;
    }
    let mut s = rtl.into_stream(0, o.depth).map_err(|e| e.error.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    if o.explicit_start {
        s.start().map_err(|e| e.to_string())?;
    }
    let t = Instant::now();
    let mut n = 0usize;
    while t.elapsed() < Duration::from_secs_f64(o.secs) {
        n += s.recv_into(&mut buf, o.timeout).map_err(|e| e.to_string())?.n_samples;
    }
    let rate = o.rate;
    let finish_at = Instant::now();
    let f = s.finish().map_err(|e| e.to_string())?;
    let finish_took = finish_at.elapsed();
    let r = &f.report;
    // Each refusal by when it came: ms after the finish began (negative:
    // before it, from the start), and how long the chip took to refuse.
    let ms = |at: Instant| if at >= finish_at { (at - finish_at).as_secs_f64() * 1e3 } else { -((finish_at - at).as_secs_f64() * 1e3) };
    let refused: Vec<String> = f
        .rtl
        .link()
        .refusals()
        .iter()
        .map(|x| format!("\"{} 0x{:04X} 0x{:04X} at {:+.2} ms, refused after {:.2} ms\"", if x.set { "set" } else { "get" }, x.value, x.index, ms(x.at), x.took.as_secs_f64() * 1e3))
        .collect();
    Ok(format!(
        "{{\"rate\":{rate},\"depth\":{},\"tier\":\"{:?}\",\"finish_ms\":{:.1},\"tail\":{},\"received\":{n},\"samples\":{},\"dropped_blocks\":{},\"saturated\":{},\"stall_retries\":{},\"refused\":[{}]}}",
        o.depth,
        r.tier,
        finish_took.as_secs_f64() * 1e3,
        f.tail.len(),
        r.samples,
        r.dropped_blocks,
        r.saturated,
        r.stall_retries,
        refused.join(",")
    ))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let runs: usize = arg(&args, "--runs", 6);
    let codes = args.iter().position(|a| a == "--codes").map(|i| {
        let c = |k: usize| args.get(i + k).and_then(|v| v.parse().ok()).unwrap_or(0);
        [c(1), c(2), c(3)]
    });
    let o = Opts {
        rate: arg(&args, "--rate", 2.4e6),
        secs: arg(&args, "--seconds", 3.0),
        gain: arg(&args, "--gain", 20.0),
        codes,
        status: args.iter().any(|a| a == "--status"),
        timeout: arg(&args, "--timeout", 1.0),
        explicit_start: !args.iter().any(|a| a == "--no-start"),
        depth: arg(&args, "--depth", 0),
    };
    let cycle = args.iter().any(|a| a == "--cycle");
    for _ in 0..runs {
        if cycle && let Err(e) = bench::power_cycle() {
            println!("{{\"error\":\"power cycle: {e}\"}}");
            continue;
        }
        match run_once(&o) {
            Ok(line) => println!("{line}"),
            Err(e) => println!("{{\"error\":\"{e}\"}}"),
        }
    }
}
