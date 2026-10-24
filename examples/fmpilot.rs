//! The sample clock against a broadcast FM (frequency modulation)
//! station's stereo pilot (findings 29): the pilot is 19 kHz at the
//! station, so its frequency in a capture's demodulated audio, read at
//! the rate the capture claims, is 19 kHz over the true rate's excess —
//! a sample clock running e fast puts it at 19 kHz / (1 + e). The tuner's
//! LO (local oscillator) error moves the carrier, not the pilot.
//!
//!   fmpilot NAME.sigmf-data --rate SPS --scan
//!   fmpilot NAME.sigmf-data --rate SPS --offset HZ [--seconds S]
//!
//! No device: reads a `record` capture (ci16_le, the stream's 8-bit
//! samples widened). `--scan` lists the strongest 200 kHz channels by
//! their offset from the capture's centre. `--offset` takes one station:
//! mixed to 0 Hz, low-passed to ±100 kHz and decimated to about 240 kHz,
//! FM-discriminated, and the pilot found by a single-frequency DFT
//! (discrete Fourier transform) stepped 0.5 Hz over 18.9-19.1 kHz, then
//! 0.01 Hz around the peak; also in each half of the capture, as a check.
//! The carrier's offset from the centre is the discriminator's mean: the
//! LO's error shows there (a crystal e fast puts it e × RF low).
//! One JSON line.

use std::f64::consts::{PI, TAU};

const PILOT_HZ: f64 = 19_000.0;
/// The audio rate the station is decimated to, about.
const AUDIO_RATE_SPS: f64 = 240e3;
/// The channel kept around the station: broadcast FM's ±75 kHz deviation
/// plus its subcarriers.
const CHANNEL_HALF_HZ: f64 = 100e3;
const LOWPASS_TAPS: usize = 255;
const SEARCH_HALF_HZ: f64 = 100.0;
const COARSE_STEP_HZ: f64 = 0.5;
const FINE_HALF_HZ: f64 = 1.0;
const FINE_STEP_HZ: f64 = 0.01;
const SCAN_FFT: usize = 4096;
const SCAN_FRAMES: usize = 400;
const SCAN_CHANNEL_HZ: f64 = 200e3;
const SCAN_LISTED: usize = 12;

#[derive(Clone, Copy, Default)]
struct C {
    re: f64,
    im: f64,
}

impl C {
    fn mul(self, o: C) -> C {
        C {
            re: self.re * o.re - self.im * o.im,
            im: self.re * o.im + self.im * o.re,
        }
    }
    fn conj(self) -> C {
        C {
            re: self.re,
            im: -self.im,
        }
    }
    fn norm2(self) -> f64 {
        self.re * self.re + self.im * self.im
    }
    fn cis(a: f64) -> C {
        C {
            re: a.cos(),
            im: a.sin(),
        }
    }
}

fn read_ci16(path: &str, max: usize) -> Result<Vec<C>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .take(max)
        .map(|b| C {
            re: i16::from_le_bytes([b[0], b[1]]) as f64 / 32768.0,
            im: i16::from_le_bytes([b[2], b[3]]) as f64 / 32768.0,
        })
        .collect())
}

/// In-place radix-2 FFT (fast Fourier transform); `x.len()` a power of 2.
fn fft(x: &mut [C]) {
    let n = x.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            x.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let w = C::cis(-TAU / len as f64);
        for start in (0..n).step_by(len) {
            let mut wk = C { re: 1.0, im: 0.0 };
            for k in 0..len / 2 {
                let a = x[start + k];
                let b = x[start + k + len / 2].mul(wk);
                x[start + k] = C {
                    re: a.re + b.re,
                    im: a.im + b.im,
                };
                x[start + k + len / 2] = C {
                    re: a.re - b.re,
                    im: a.im - b.im,
                };
                wk = wk.mul(w);
            }
        }
        len <<= 1;
    }
}

fn scan(x: &[C], rate: f64) -> String {
    let mut power = vec![0.0f64; SCAN_FFT];
    let frames = (x.len() / SCAN_FFT).min(SCAN_FRAMES);
    let window: Vec<f64> = (0..SCAN_FFT)
        .map(|i| 0.5 - 0.5 * (TAU * i as f64 / SCAN_FFT as f64).cos())
        .collect();
    for f in 0..frames {
        let mut buf: Vec<C> = x[f * SCAN_FFT..(f + 1) * SCAN_FFT]
            .iter()
            .zip(&window)
            .map(|(s, w)| C {
                re: s.re * w,
                im: s.im * w,
            })
            .collect();
        fft(&mut buf);
        for (p, b) in power.iter_mut().zip(&buf) {
            *p += b.norm2();
        }
    }
    // Bins in order of frequency, -rate/2 .. rate/2.
    let bin_hz = rate / SCAN_FFT as f64;
    let ordered: Vec<(f64, f64)> = (0..SCAN_FFT)
        .map(|i| {
            let k = (i + SCAN_FFT / 2) % SCAN_FFT;
            ((i as f64 - (SCAN_FFT / 2) as f64) * bin_hz, power[k])
        })
        .collect();
    let mut sorted: Vec<f64> = ordered.iter().map(|&(_, p)| p).collect();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let median = sorted[sorted.len() / 2];
    // Channels: the total in each 200 kHz span around a bin, the
    // strongest first, none within a channel of one already listed.
    let half = (SCAN_CHANNEL_HZ / 2.0 / bin_hz) as usize;
    let mut chans: Vec<(f64, f64)> = (half..SCAN_FFT - half)
        .map(|i| {
            (
                ordered[i].0,
                ordered[i - half..=i + half]
                    .iter()
                    .map(|&(_, p)| p)
                    .sum::<f64>()
                    / ((2 * half + 1) as f64 * median),
            )
        })
        .collect();
    chans.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut listed: Vec<(f64, f64)> = Vec::new();
    for (f, p) in chans {
        if listed.len() == SCAN_LISTED {
            break;
        }
        if listed
            .iter()
            .all(|&(g, _)| (g - f).abs() >= SCAN_CHANNEL_HZ)
        {
            listed.push((f, p));
        }
    }
    let items: Vec<String> = listed
        .iter()
        .map(|(f, p)| format!("[{:.0},{:.1}]", f, 10.0 * p.log10()))
        .collect();
    format!(
        "{{\"kind\":\"scan\",\"rate\":{rate},\"frames\":{frames},\"channels_offset_hz_db_over_median\":[{}]}}",
        items.join(",")
    )
}

/// The station mixed to 0 Hz, low-passed and decimated, then the
/// discriminator's audio: the phase step per sample over 2π, in cycles.
fn audio(x: &[C], rate: f64, offset: f64) -> (Vec<f64>, f64) {
    let decim = (rate / AUDIO_RATE_SPS).round().max(1.0) as usize;
    let fc = CHANNEL_HALF_HZ / rate;
    let m = (LOWPASS_TAPS - 1) as f64 / 2.0;
    let taps: Vec<f64> = (0..LOWPASS_TAPS)
        .map(|i| {
            let t = i as f64 - m;
            let sinc = if t == 0.0 {
                2.0 * fc
            } else {
                (TAU * fc * t).sin() / (PI * t)
            };
            let blackman = 0.42 - 0.5 * (TAU * i as f64 / (LOWPASS_TAPS - 1) as f64).cos()
                + 0.08 * (2.0 * TAU * i as f64 / (LOWPASS_TAPS - 1) as f64).cos();
            sinc * blackman
        })
        .collect();
    let step = -TAU * offset / rate;
    let mixed: Vec<C> = x
        .iter()
        .enumerate()
        .map(|(n, s)| s.mul(C::cis(step * n as f64)))
        .collect();
    let mut base = Vec::with_capacity(mixed.len() / decim);
    let mut n = LOWPASS_TAPS;
    while n < mixed.len() {
        let mut acc = C::default();
        for (k, t) in taps.iter().enumerate() {
            let s = mixed[n - k];
            acc.re += s.re * t;
            acc.im += s.im * t;
        }
        base.push(acc);
        n += decim;
    }
    let out: Vec<f64> = base
        .windows(2)
        .map(|w| {
            let d = w[1].mul(w[0].conj());
            d.im.atan2(d.re) / TAU
        })
        .collect();
    (out, rate / decim as f64)
}

/// The power of `a` at `f` Hz (a single-frequency DFT).
fn power_at(a: &[f64], fs: f64, f: f64) -> f64 {
    let w = C::cis(-TAU * f / fs);
    let mut ph = C { re: 1.0, im: 0.0 };
    let mut acc = C::default();
    for (n, &s) in a.iter().enumerate() {
        acc.re += s * ph.re;
        acc.im += s * ph.im;
        ph = ph.mul(w);
        // Keep the rotation's magnitude at 1 over millions of steps.
        if n % 4096 == 0 {
            let r = ph.norm2().sqrt();
            ph = C {
                re: ph.re / r,
                im: ph.im / r,
            };
        }
    }
    acc.norm2() / (a.len() as f64 * a.len() as f64)
}

/// The pilot's frequency in `a`, and its level over the search's median, dB.
fn pilot(a: &[f64], fs: f64) -> (f64, f64) {
    let coarse: Vec<(f64, f64)> = (0..=((2.0 * SEARCH_HALF_HZ / COARSE_STEP_HZ) as usize))
        .map(|i| PILOT_HZ - SEARCH_HALF_HZ + i as f64 * COARSE_STEP_HZ)
        .map(|f| (f, power_at(a, fs, f)))
        .collect();
    let mut levels: Vec<f64> = coarse.iter().map(|&(_, p)| p).collect();
    levels.sort_by(|x, y| x.total_cmp(y));
    let median = levels[levels.len() / 2];
    let peak = coarse
        .iter()
        .copied()
        .max_by(|x, y| x.1.total_cmp(&y.1))
        .unwrap();
    let fine = (0..=((2.0 * FINE_HALF_HZ / FINE_STEP_HZ) as usize))
        .map(|i| peak.0 - FINE_HALF_HZ + i as f64 * FINE_STEP_HZ)
        .map(|f| (f, power_at(a, fs, f)))
        .max_by(|x, y| x.1.total_cmp(&y.1))
        .unwrap();
    (fine.0, 10.0 * (fine.1 / median).log10())
}

fn run() -> Result<String, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let path = args
        .first()
        .filter(|a| !a.starts_with("--"))
        .ok_or("usage: fmpilot NAME.sigmf-data --rate SPS (--scan | --offset HZ)")?;
    let rate: f64 = get("--rate")
        .and_then(|v| v.parse().ok())
        .ok_or("--rate SPS")?;
    let seconds: f64 = get("--seconds")
        .and_then(|v| v.parse().ok())
        .unwrap_or(f64::INFINITY);
    let max = if seconds.is_finite() {
        (seconds * rate) as usize
    } else {
        usize::MAX
    };
    let x = read_ci16(path, max)?;
    if args.iter().any(|a| a == "--scan") {
        return Ok(scan(&x, rate));
    }
    let offset: f64 = get("--offset")
        .and_then(|v| v.parse().ok())
        .ok_or("--offset HZ (or --scan)")?;
    let (a, fs) = audio(&x, rate, offset);
    let (f, level) = pilot(&a, fs);
    // The carrier: the discriminator's mean, from the offset mixed out.
    // Broadcast FM's modulation averages to the carrier.
    let carrier = offset + a.iter().sum::<f64>() / a.len() as f64 * fs;
    let (f1, l1) = pilot(&a[..a.len() / 2], fs);
    let (f2, l2) = pilot(&a[a.len() / 2..], fs);
    // A clock e fast reads the pilot at 19 kHz / (1 + e).
    let ppm = |f: f64| (PILOT_HZ / f - 1.0) * 1e6;
    Ok(format!(
        "{{\"kind\":\"pilot\",\"file\":{path:?},\"rate\":{rate},\"offset_hz\":{offset},\"carrier_offset_hz\":{carrier:.1},\"audio_rate\":{fs},\"seconds\":{:.2},\"pilot_hz\":{f:.3},\"level_db\":{level:.1},\"clock_excess_ppm\":{:.1},\"halves\":[[{f1:.3},{l1:.1},{:.1}],[{f2:.3},{l2:.1},{:.1}]]}}",
        x.len() as f64 / rate,
        ppm(f),
        ppm(f1),
        ppm(f2)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A station 400 kHz off centre, a 1 kHz tone at 50 kHz deviation and
    /// the pilot at 7.5 kHz, taken by a clock `excess` fast and read at
    /// the nominal rate.
    fn station(rate: f64, excess: f64, secs: f64) -> Vec<C> {
        let true_rate = rate * (1.0 + excess);
        let n = (secs * rate) as usize;
        let mut phase = 0.0f64;
        (0..n)
            .map(|i| {
                let t = i as f64 / true_rate;
                let dev = 50e3 * (TAU * 1e3 * t).sin() + 7.5e3 * (TAU * PILOT_HZ * t).sin();
                phase += TAU * (400e3 + dev) / true_rate;
                C::cis(phase)
            })
            .collect()
    }

    #[test]
    fn a_fast_clock_reads_the_pilot_low() {
        let rate = 2.4e6;
        for excess in [0.0, 488e-6] {
            let (a, fs) = audio(&station(rate, excess, 2.0), rate, 400e3);
            let (f, level) = pilot(&a, fs);
            let ppm = (PILOT_HZ / f - 1.0) * 1e6;
            assert!(
                (ppm - excess * 1e6).abs() < 5.0,
                "excess {excess}: read {ppm:.1} ppm at {f:.3} Hz"
            );
            assert!(level > 20.0, "{level}");
        }
    }

    #[test]
    fn the_scan_finds_the_station() {
        let line = scan(&station(2.4e6, 0.0, 1.0), 2.4e6);
        let first: f64 = line
            .split("[[")
            .nth(1)
            .unwrap()
            .split(',')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!((first - 400e3).abs() < 60e3, "{line}");
    }
}

fn main() {
    match run() {
        Ok(line) => println!("{line}"),
        Err(e) => {
            eprintln!("fmpilot: {e}");
            std::process::exit(1);
        }
    }
}
