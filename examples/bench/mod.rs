//! What the bench programs share: the HackRF tone source over stdin,
//! captures from a stream, and FFT power spectra.

#![allow(dead_code)]

use std::f64::consts::TAU;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use sdr_core::{Iq, RxSource};
use sdr_rtl::RtlStream;

pub const FFT_N: usize = 4096;
/// Segments averaged per spectrum.
pub const SEGMENTS: usize = 16;
/// Bins either side of DC left out of peak searches and the median.
pub const DC_GUARD: usize = 2;
/// Settle after the source acknowledges a tone (tone_source's own
/// advice: ≥ 100 ms of queued samples).
pub const SOURCE_SETTLE: Duration = Duration::from_millis(150);
/// Samples discarded after any change before a capture.
pub const DISCARD: Duration = Duration::from_millis(100);
pub const TRANSFER_BYTES: usize = 16 * 1024;
pub const QUEUE_DEPTH: usize = 16;
pub const RECV_TIMEOUT_S: f64 = 0.25;
/// The default source command, run from the workspace root. The source
/// runs at 8 Msps: at its own default 2.4 Msps its DAC images of the tone
/// at ± 2.4 MHz reach the dongle 15-18 dB under the tone and read as a
/// 2.4 MHz alias of the dongle's (findings 12).
pub const SOURCE_CMD: &str = "cargo run --release --example tone_source -- --cabled --rate 8000000";
/// Full scale of the widened samples (`WireSample::OffsetU8`: (b − 128) << 8).
pub const FULL_SCALE: f64 = 32768.0;

pub struct Source {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    pub txvga: u16,
    pub backoff: i32,
}

impl Source {
    pub fn spawn(cmd: &str, txvga: u16, backoff: i32) -> Result<Source, String> {
        let parts: Vec<&str> = cmd.split_whitespace().collect();
        let (prog, args) = parts.split_first().ok_or("empty --source-cmd")?;
        let mut child = Command::new(prog)
            .args(args)
            .current_dir(workspace_root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn {cmd}: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("no stdout")?);
        let mut s = Source { child, stdin, stdout, txvga, backoff };
        let ready = s.line()?;
        if !ready.starts_with("ready") {
            return Err(format!("source: {ready}"));
        }
        eprintln!("source {ready}");
        Ok(s)
    }

    fn line(&mut self) -> Result<String, String> {
        let mut l = String::new();
        self.stdout.read_line(&mut l).map_err(|e| e.to_string())?;
        if l.is_empty() {
            return Err("source exited".into());
        }
        Ok(l.trim().to_string())
    }

    fn command(&mut self, c: &str) -> Result<String, String> {
        writeln!(self.stdin, "{c}").map_err(|e| e.to_string())?;
        self.stdin.flush().map_err(|e| e.to_string())?;
        let r = self.line()?;
        if !r.starts_with("ok") {
            return Err(format!("source: {c} -> {r}"));
        }
        std::thread::sleep(SOURCE_SETTLE);
        Ok(r)
    }

    pub fn tone(&mut self, hz: f64) -> Result<String, String> {
        self.command(&format!("tone {hz} {} {}", self.txvga, self.backoff))
    }

    pub fn tone_at(&mut self, hz: f64, txvga: u16, backoff: i32) -> Result<String, String> {
        self.command(&format!("tone {hz} {txvga} {backoff}"))
    }

    pub fn off(&mut self) -> Result<String, String> {
        self.command("off")
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        let _ = writeln!(self.stdin, "quit");
        let _ = self.stdin.flush();
        let _ = self.child.wait();
    }
}

/// Off time and re-enumeration wait of the power cycle (findings step 0:
/// gone after ~60 ms, back ~530 ms after power returns).
pub const VBUS_OFF: Duration = Duration::from_millis(500);
pub const ENUMERATE_DEADLINE: Duration = Duration::from_secs(5);

/// The analyzer's TARGET A power (`cynthion-witness vbus on|off`).
pub fn witness(arg: &str) -> Result<String, String> {
    let out = Command::new("cargo")
        .args(["run", "--release", "--quiet", "--manifest-path", "tools/Cargo.toml", "--bin", "cynthion-witness", "--", "vbus", arg])
        .current_dir(workspace_root())
        .output()
        .map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The off time of the retry when one cycle leaves the dongle absent
/// (METHODS §3: "allow a longer off-time retry"; 2026-10-10 one 500 ms
/// cycle left it absent for 5 s).
pub const VBUS_OFF_RETRY: Duration = Duration::from_secs(2);

fn cycle_once(off_time: Duration) -> Result<(String, bool, u128), String> {
    let off = witness("off")?;
    std::thread::sleep(off_time);
    let on = witness("on")?;
    let t = Instant::now();
    while sdr_rtl::list().is_empty() {
        if t.elapsed() > ENUMERATE_DEADLINE {
            return Ok((format!("{off}; {on}"), false, t.elapsed().as_millis()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok((format!("{off}; {on}"), true, t.elapsed().as_millis()))
}

/// The dongle power-cycled through the analyzer, back and enumerated;
/// once more with the longer off time if the first leaves it absent.
pub fn power_cycle() -> Result<String, String> {
    let (said, back, ms) = cycle_once(VBUS_OFF)?;
    let report = if back {
        format!("{said}; back after {ms} ms")
    } else {
        let (said2, back2, ms2) = cycle_once(VBUS_OFF_RETRY)?;
        if !back2 {
            return Err(format!("the dongle did not return within {ENUMERATE_DEADLINE:?}, twice ({said}; {said2})"));
        }
        format!("{said}; absent after {ms} ms; retried with {VBUS_OFF_RETRY:?} off: {said2}; back after {ms2} ms")
    };
    // Let enumeration finish before the first request.
    std::thread::sleep(Duration::from_millis(500));
    Ok(report)
}

/// rtl → crates → the workspace.
pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Received samples and what the engine flagged while they came.
pub struct Capture {
    pub samples: Vec<Iq>,
    pub secs: f64,
    pub overflows: usize,
    pub timeouts: usize,
}

/// Read and drop for `d`.
pub fn discard(s: &mut RtlStream, buf: &mut [Iq], d: Duration) -> Result<(), String> {
    let t = Instant::now();
    while t.elapsed() < d {
        s.recv_into(buf, RECV_TIMEOUT_S).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// At least `n` samples, or whatever arrives by `deadline`.
pub fn capture(s: &mut RtlStream, buf: &mut [Iq], n: usize, deadline: Duration) -> Result<Capture, String> {
    let mut c = Capture { samples: Vec::with_capacity(n.min(FFT_N * SEGMENTS)), secs: 0.0, overflows: 0, timeouts: 0 };
    let t = Instant::now();
    while c.samples.len() < n && t.elapsed() < deadline {
        let info = s.recv_into(buf, RECV_TIMEOUT_S).map_err(|e| e.to_string())?;
        c.overflows += info.overflow as usize;
        c.timeouts += info.timed_out as usize;
        c.samples.extend_from_slice(&buf[..info.n_samples]);
    }
    c.secs = t.elapsed().as_secs_f64();
    Ok(c)
}

/// Everything that arrives in `d`.
pub fn window(s: &mut RtlStream, buf: &mut [Iq], d: Duration) -> Result<Capture, String> {
    capture(s, buf, usize::MAX, d)
}

/// A discard, then one spectrum's worth of samples.
pub fn spectrum_capture(s: &mut RtlStream, buf: &mut [Iq]) -> Result<Capture, String> {
    discard(s, buf, DISCARD)?;
    capture(s, buf, FFT_N * SEGMENTS, Duration::from_secs(2))
}

pub fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -TAU / len as f64;
        for start in (0..n).step_by(len) {
            for k in 0..len / 2 {
                let (wr, wi) = ((ang * k as f64).cos(), (ang * k as f64).sin());
                let (a, b) = (start + k, start + k + len / 2);
                let (xr, xi) = (re[b] * wr - im[b] * wi, re[b] * wi + im[b] * wr);
                re[b] = re[a] - xr;
                im[b] = im[a] - xi;
                re[a] += xr;
                im[a] += xi;
            }
        }
        len <<= 1;
    }
}

/// The averaged power spectrum, bin k at k / FFT_N cycles per sample,
/// scaled so a full-scale complex tone on a bin reads 1.0 (0 dBFS).
pub fn spectrum(x: &[Iq]) -> Option<Vec<f64>> {
    let segs = (x.len() / FFT_N).min(SEGMENTS);
    if segs == 0 {
        return None;
    }
    let win: Vec<f64> = (0..FFT_N).map(|k| 0.5 - 0.5 * (TAU * k as f64 / FFT_N as f64).cos()).collect();
    let coherent = FULL_SCALE * win.iter().sum::<f64>();
    let scale = 1.0 / (coherent * coherent * segs as f64);
    let mut p = vec![0.0; FFT_N];
    for s in 0..segs {
        let seg = &x[s * FFT_N..(s + 1) * FFT_N];
        let mi = seg.iter().map(|v| v.i as f64).sum::<f64>() / FFT_N as f64;
        let mq = seg.iter().map(|v| v.q as f64).sum::<f64>() / FFT_N as f64;
        let mut re: Vec<f64> = seg.iter().zip(&win).map(|(v, w)| (v.i as f64 - mi) * w).collect();
        let mut im: Vec<f64> = seg.iter().zip(&win).map(|(v, w)| (v.q as f64 - mq) * w).collect();
        fft(&mut re, &mut im);
        for k in 0..FFT_N {
            p[k] += (re[k] * re[k] + im[k] * im[k]) * scale;
        }
    }
    Some(p)
}

pub fn db(p: f64) -> f64 {
    10.0 * p.max(1e-30).log10()
}

pub fn is_dc(k: usize) -> bool {
    k <= DC_GUARD || k >= FFT_N - DC_GUARD
}

pub fn signed_bin(k: usize) -> f64 {
    if k < FFT_N / 2 { k as f64 } else { k as f64 - FFT_N as f64 }
}

/// The bin a frequency falls in at `rate` (aliased into the output).
pub fn bin_of(hz: f64, rate: f64) -> usize {
    ((hz / rate * FFT_N as f64).round() as i64).rem_euclid(FFT_N as i64) as usize
}

/// The median bin outside DC.
pub fn median(p: &[f64]) -> f64 {
    let mut s: Vec<f64> = (0..FFT_N).filter(|k| !is_dc(*k)).map(|k| p[k]).collect();
    s.sort_by(f64::total_cmp);
    s[s.len() / 2].max(f64::MIN_POSITIVE)
}

/// The strongest bin within ± `tol` bins of `k`.
pub fn near(p: &[f64], k: usize, tol: usize) -> usize {
    (0..=2 * tol).map(|d| (k + FFT_N + d - tol) % FFT_N).max_by(|a, b| p[*a].total_cmp(&p[*b])).unwrap_or(k)
}

/// A peak's frequency by parabolic interpolation in log power.
pub fn peak_hz(p: &[f64], k: usize, rate: f64) -> f64 {
    let l = |k: usize| p[k % FFT_N].max(f64::MIN_POSITIVE).ln();
    let (a, b, c) = (l(k + FFT_N - 1), l(k), l(k + 1));
    let frac = if a - 2.0 * b + c != 0.0 { 0.5 * (a - c) / (a - 2.0 * b + c) } else { 0.0 };
    (signed_bin(k) + frac) / FFT_N as f64 * rate
}

/// The `n` strongest local maxima outside DC, at least `sep` bins apart:
/// (bin, power).
pub fn peaks(p: &[f64], n: usize, sep: usize) -> Vec<(usize, f64)> {
    let mut order: Vec<usize> = (0..FFT_N).filter(|k| !is_dc(*k)).collect();
    order.sort_by(|a, b| p[*b].total_cmp(&p[*a]));
    let mut out: Vec<(usize, f64)> = Vec::new();
    for k in order {
        if out.len() == n {
            break;
        }
        let d = |t: usize| {
            let x = (k as i64 - t as i64).unsigned_abs() as usize;
            x.min(FFT_N - x)
        };
        if out.iter().all(|(t, _)| d(*t) > sep) {
            out.push((k, p[k]));
        }
    }
    out
}

/// Fraction of 8-bit samples at the rails.
pub fn clipped(x: &[Iq]) -> f64 {
    let rail = |v: i16| v == i16::MIN || v >> 8 == 127;
    x.iter().filter(|v| rail(v.i) || rail(v.q)).count() as f64 / x.len().max(1) as f64
}
