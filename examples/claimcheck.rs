//! The interface claim at open, on the bench (review A3, A4): while one
//! receiver streams, a second open of the same dongle is refused before it
//! writes anything, and the stream goes on unharmed.
//!
//!   claimcheck
//!
//! Run from the workspace root (`cargo run --release -p sdr-rtl
//! --features bench --example claimcheck`), one dongle attached. The first
//! receiver streams; after 1 s a second `Rtl::open` of the same dongle is
//! made; the stream reads 1 s more and finishes. One JSON line: the second
//! open's result, the bytes read before and after it against the rate,
//! and the finished stream's dropped blocks. Then, the first receiver
//! closed, a third open must succeed (the claim is released with it).

use std::io::Read;
use std::time::{Duration, Instant};

use sdr_rtl::blocking::Rtl;

/// How long each half of the stream is read.
const HALF: Duration = Duration::from_secs(1);

fn read_for(s: &mut sdr_rtl::blocking::Stream, d: Duration) -> Result<usize, String> {
    let mut buf = vec![0u8; 16_384];
    let (mut n, t) = (0usize, Instant::now());
    while t.elapsed() < d {
        n += s.read(&mut buf).map_err(|e| e.to_string())?;
    }
    Ok(n)
}

fn main() {
    // `SDR_BENCH_RTL`: which dongle on a host with more than one.
    let spec = std::env::var("SDR_BENCH_RTL").unwrap_or_default();
    let r = (|| -> Result<String, String> {
        let first = Rtl::open(&spec).map_err(|e| format!("first open: {e}"))?;
        let rate = first.rate_set();
        let mut s = first
            .into_stream(0, 0)
            .map_err(|e| format!("stream: {e}"))?;
        let before = read_for(&mut s, HALF)?;
        let second = match Rtl::open(&spec) {
            Ok(_) => "opened (NOT refused)".to_string(),
            Err(e) => format!("refused: {e}"),
        };
        let after = read_for(&mut s, HALF)?;
        let done = s.finish().map_err(|e| format!("finish: {e}"))?;
        drop(done);
        let third = match Rtl::open(&spec) {
            Ok(_) => "opened".to_string(),
            Err(e) => format!("failed: {e}"),
        };
        let expected = (rate * 2.0 * HALF.as_secs_f64()) as usize;
        Ok(format!(
            "{{\"rate\":{rate},\"second_open\":{:?},\"bytes_before\":{before},\"bytes_after\":{after},\"expected_each\":{expected},\"third_open_after_close\":{:?}}}",
            second, third
        ))
    })();
    match r {
        Ok(line) => println!("{line}"),
        Err(e) => println!("{{\"error\":{e:?}}}"),
    }
}
