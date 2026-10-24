//! Step 4: the tuner brought up from the documents and measured against a
//! cabled tone. Each subcommand is one declared sweep writing one record;
//! the source is fixed within every comparison, and where a gain sweep
//! would push the meter out of range the source is moved in recorded
//! steps (`meter::relevel`), so a difference between two codes is always
//! (meter difference) + (source difference).
//!
//! ```text
//! meter        the positive control: every documented readback with the
//!              tone off, on, and across source levels, in each tuner
//!              configuration; first a probe of every SEL_DIV code at the
//!              frequency, since the divider rule is not documented
//! pll-sweep    every SEL_DIV code x Nint in coarse steps: R0-R4 after each,
//!              and the readbacks with the tone at the LO each divider
//!              hypothesis predicts
//! tune HZ      one frequency: PLL words, status, readbacks off/on
//! gain-ladder  LNA_GAIN, MIX_GAIN, VGA_CODE 0..=15 one stage at a time,
//!              the meter re-levelled
//! band-table   a frequency grid x RFFILT x RFMUX x TF_NCH x TF_LP
//!              (staged, or --full)
//! filter       FILT_CODE, FILT_BW, HPF: the tone stepped across the IF
//! if-choice    the demodulator's DDC IF x Spec_inv x tone side: which
//!              readback locates the tone
//! ```

use std::collections::HashSet;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::json::{J, hex8};
use crate::meter::{self, Reading, Spec};
use crate::regs::DEMOD_CTL;
use crate::source::{self, Level, Source, Tone};
use crate::tuner::{self, Config, DemodIf, Front, Pll};
use crate::usb::{self, Rtl};

/// The source's tuning range (its own documentation): the sweeps skip a
/// tone outside it.
pub const SOURCE_MIN_HZ: f64 = 1e6;
pub const SOURCE_MAX_HZ: f64 = 6e9;
/// The tuner's documented range ([R820T] Table 1-2).
pub const TUNER_MIN_HZ: f64 = 42e6;
pub const TUNER_MAX_HZ: f64 = 1002e6;
/// Where a single-frequency step runs unless told otherwise: inside the
/// tuner's range, away from both edges.
pub const DEFAULT_FREQ_HZ: f64 = 300e6;
/// The tone sits this far above the DDC's IF when the DDC tracks the tone
/// (`filter --ddc track`): off DC, and the source's LO leakage (0.4 MHz
/// below the tone) stays above DC too.
pub const DDC_TRACK_OFFSET_HZ: f64 = 1e6;
/// Settle after a tuner register write before a reading.
const TUNER_SETTLE: Duration = Duration::from_millis(20);

/// The meter step's source ladder: txvga x backoff, taken in ascending
/// order of level.
pub const METER_TXVGA: [i32; 5] = [0, 5, 10, 15, 20];
pub const METER_BACKOFF: [i32; 4] = [-20, -12, -6, 0];

pub struct Opts {
    pub freq_hz: f64,
    pub div: Option<u8>,
    pub ref_div2: bool,
    pub if_hz: f64,
    pub high_side: bool,
    pub config: Config,
    pub level: Level,
    pub meter: Option<Spec>,
    pub page3: bool,
    pub out: Option<PathBuf>,
    pub grid_mhz: f64,
    pub full: bool,
    pub nint_step: u16,
    pub ddc_track: bool,
    pub r30_b6: Option<bool>,
    pub lna: u8,
    pub mix: u8,
    pub vga: u8,
    pub if_step_hz: Option<f64>,
    pub if_max_hz: f64,
    pub snapshots: Option<usize>,
    pub limit: Option<usize>,
    /// pll-sweep: the Nint span (defaults: the whole range).
    pub nint_from: Option<u16>,
    pub nint_to: Option<u16>,
    /// lo-find: the Nint held, and the tone's RF span and step.
    pub nint: Option<u16>,
    pub rf_from_hz: f64,
    pub rf_to_hz: f64,
    pub rf_step_hz: f64,
}

pub const USAGE: &str = "options: --freq HZ | --div CODE | --ref-div2 0|1 | --if HZ | --lo-side high|low | --config manual|vagc|auto | --txvga N | --backoff N | --meter NAME LO HI up|down | --page3 | --out PATH | --grid-mhz N | --full | --nint-step N | --ddc track|fixed | --r30b6 0|1 | --lna N | --mix N | --vga N | --if-step HZ | --if-max HZ | --snapshots N | --limit N | --settle MS | --nint-from N | --nint-to N | --nint N | --rf-from HZ | --rf-to HZ | --rf-step HZ";

fn val<'a>(args: &'a [String], i: usize, flag: &str) -> Result<&'a str, String> {
    args.get(i + 1)
        .map(String::as_str)
        .ok_or(format!("{flag} needs a value; {USAGE}"))
}

fn num(s: &str, flag: &str) -> Result<f64, String> {
    s.parse().map_err(|e| format!("{flag} {s}: {e}"))
}

fn int(s: &str, flag: &str, max: i64) -> Result<i64, String> {
    let v: i64 = s.parse().map_err(|e| format!("{flag} {s}: {e}"))?;
    if v < 0 || v > max {
        return Err(format!("{flag} {s}: 0..={max}"));
    }
    Ok(v)
}

fn bit(s: &str, flag: &str) -> Result<bool, String> {
    match s {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(format!("{flag} {s}: 0 | 1")),
    }
}

impl Opts {
    /// (options, positional arguments).
    pub fn parse(args: &[String]) -> Result<(Opts, Vec<String>), String> {
        let mut o = Opts {
            freq_hz: DEFAULT_FREQ_HZ,
            div: None,
            ref_div2: tuner::REF_DIV2_BY_DOCUMENT,
            if_hz: tuner::IF_HZ,
            high_side: true,
            config: Config::Manual,
            level: Level {
                txvga_db: 10,
                backoff_db: -20,
            },
            meter: None,
            page3: false,
            out: None,
            grid_mhz: 10.0,
            full: false,
            nint_step: 5,
            ddc_track: true,
            r30_b6: None,
            lna: 8,
            mix: 8,
            vga: 8,
            if_step_hz: None,
            if_max_hz: 12e6,
            snapshots: None,
            limit: None,
            nint_from: None,
            nint_to: None,
            nint: None,
            rf_from_hz: 2e6,
            rf_to_hz: 1300e6,
            rf_step_hz: 3e6,
        };
        let mut pos = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let a = args[i].as_str();
            match a {
                "--freq" => o.freq_hz = num(val(args, i, a)?, a)?,
                "--div" => o.div = Some(int(val(args, i, a)?, a, 7)? as u8),
                "--ref-div2" => o.ref_div2 = bit(val(args, i, a)?, a)?,
                "--if" => o.if_hz = num(val(args, i, a)?, a)?,
                "--lo-side" => {
                    o.high_side = match val(args, i, a)? {
                        "high" => true,
                        "low" => false,
                        s => return Err(format!("--lo-side {s}: high | low")),
                    }
                }
                "--config" => o.config = Config::parse(val(args, i, a)?)?,
                "--txvga" => {
                    o.level.txvga_db = int(val(args, i, a)?, a, source::TXVGA_MAX as i64)? as i32
                }
                "--backoff" => {
                    let v: i64 = val(args, i, a)?.parse().map_err(|e| format!("{a}: {e}"))?;
                    if !(-60..=0).contains(&v) {
                        return Err(format!("--backoff {v}: -60..=0"));
                    }
                    o.level.backoff_db = v as i32;
                }
                "--meter" => {
                    let end = (i + 5).min(args.len());
                    o.meter = Some(Spec::parse(&args[i + 1..end])?);
                    i += 3;
                }
                "--page3" => {
                    o.page3 = true;
                    i += 1;
                    continue;
                }
                "--out" => o.out = Some(val(args, i, a)?.into()),
                "--grid-mhz" => o.grid_mhz = num(val(args, i, a)?, a)?,
                "--full" => {
                    o.full = true;
                    i += 1;
                    continue;
                }
                "--nint-step" => o.nint_step = int(val(args, i, a)?, a, 255)?.max(1) as u16,
                "--ddc" => {
                    o.ddc_track = match val(args, i, a)? {
                        "track" => true,
                        "fixed" => false,
                        s => return Err(format!("--ddc {s}: track | fixed")),
                    }
                }
                "--r30b6" => o.r30_b6 = Some(bit(val(args, i, a)?, a)?),
                "--lna" => o.lna = int(val(args, i, a)?, a, 15)? as u8,
                "--mix" => o.mix = int(val(args, i, a)?, a, 15)? as u8,
                "--vga" => o.vga = int(val(args, i, a)?, a, 15)? as u8,
                "--if-step" => o.if_step_hz = Some(num(val(args, i, a)?, a)?),
                "--if-max" => o.if_max_hz = num(val(args, i, a)?, a)?,
                "--snapshots" => o.snapshots = Some(int(val(args, i, a)?, a, 100)?.max(1) as usize),
                "--limit" => o.limit = Some(int(val(args, i, a)?, a, i64::MAX)? as usize),
                "--settle" => source::set_settle_ms(int(val(args, i, a)?, a, 10_000)? as u64),
                "--nint-from" => {
                    o.nint_from = Some(int(val(args, i, a)?, a, tuner::NINT_MAX as i64)? as u16)
                }
                "--nint-to" => {
                    o.nint_to = Some(int(val(args, i, a)?, a, tuner::NINT_MAX as i64)? as u16)
                }
                "--nint" => o.nint = Some(int(val(args, i, a)?, a, tuner::NINT_MAX as i64)? as u16),
                "--rf-from" => o.rf_from_hz = num(val(args, i, a)?, a)?,
                "--rf-to" => o.rf_to_hz = num(val(args, i, a)?, a)?,
                "--rf-step" => o.rf_step_hz = num(val(args, i, a)?, a)?,
                s if s.starts_with("--") => return Err(format!("unknown option {s}; {USAGE}")),
                s => {
                    pos.push(s.to_string());
                    i += 1;
                    continue;
                }
            }
            i += 2;
        }
        Ok((o, pos))
    }

    /// The front end from the configuration and the fixed gain codes.
    pub fn front(&self) -> Front {
        let mut f = self.config.front();
        f.lna_gain = self.lna;
        f.mix_gain = self.mix;
        f.vga_code = self.vga;
        if let Some(b) = self.r30_b6 {
            f.r30_b6 = b;
        }
        f
    }

    pub fn demod(&self) -> DemodIf {
        DemodIf {
            if_hz: self.if_hz,
            spec_inv: self.high_side,
            ..DemodIf::default()
        }
    }

    /// The LO for an RF on the chosen side.
    pub fn lo_hz(&self, rf_hz: f64) -> f64 {
        tuner::lo_for(rf_hz, self.if_hz, self.high_side)
    }

    /// The RF whose IF lands on the DDC for an LO.
    pub fn rf_hz(&self, lo_hz: f64) -> f64 {
        if self.high_side {
            lo_hz - self.if_hz
        } else {
            lo_hz + self.if_hz
        }
    }

    pub fn pll(&self, lo_hz: f64) -> Result<Pll, String> {
        let code = match self.div {
            Some(c) => c,
            None => tuner::choose_div(lo_hz, self.ref_div2)?,
        };
        Pll::program(lo_hz, code, self.ref_div2)
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.out
            .clone()
            .unwrap_or_else(|| crate::data_dir().join(name))
    }

    pub fn record(&self) -> J {
        J::obj()
            .with("freq_hz", self.freq_hz)
            .with("div", self.div)
            .with("ref_div2", self.ref_div2)
            .with("if_hz", self.if_hz)
            .with("lo_side", if self.high_side { "high" } else { "low" })
            .with("config", self.config.name())
            .with("source_level_start", self.level.tone(self.freq_hz).record())
            .with("meter", self.meter.as_ref().map(Spec::record))
            .with("page3", self.page3)
            .with(
                "gain_codes_fixed",
                J::obj()
                    .with("lna", self.lna)
                    .with("mix", self.mix)
                    .with("vga", self.vga),
            )
            .with("r30_b6", self.r30_b6)
            .with("settle_ms", source::settle().as_millis() as u64)
    }
}

fn source_in_range(hz: f64) -> bool {
    (SOURCE_MIN_HZ..=SOURCE_MAX_HZ).contains(&hz)
}

/// The bench for one step: source, dongle with the demodulator in IF mode
/// and the tuner image written for `freq_hz`.
struct Bench {
    src: Source,
    rtl: Rtl,
    pll: Pll,
    front: Front,
    start: Instant,
    setup: J,
}

fn bench(o: &Opts) -> Result<Bench, String> {
    usb::guard()?;
    let src = Source::open()?;
    let lo = o.lo_hz(o.freq_hz);
    let pll = o.pll(lo)?;
    let front = o.front();
    let (rtl, setup) = tuner::fresh(&o.demod(), &front, &pll)?;
    Ok(Bench {
        src,
        rtl,
        pll,
        front,
        start: Instant::now(),
        setup,
    })
}

impl Bench {
    fn snapshots(&self, o: &Opts, default: usize) -> usize {
        o.snapshots.unwrap_or(default)
    }

    fn reading(&self, o: &Opts, n: usize) -> Reading {
        Reading::take_n(
            &self.rtl,
            self.start,
            o.page3,
            o.meter.and_then(|m| m.meter.page3_offset()),
            n,
        )
    }

    /// Write one front-end register of the current image.
    fn write_front(&mut self, front: Front, regs: &[u8]) -> Vec<String> {
        self.front = front;
        let img = tuner::image(&front, &self.pll);
        let list: Vec<(u8, u8)> = regs.iter().map(|&r| (r, img[r as usize])).collect();
        let e = tuner::write_regs(&self.rtl, &list);
        std::thread::sleep(TUNER_SETTLE);
        e
    }
}

/// Which named meters moved between two readings.
fn moved_names(off: &Reading, on: &Reading) -> J {
    let mut names = Vec::new();
    for (m, n) in meter::ALL {
        if let Some((_, _, true)) = meter::moved(off, on, m) {
            names.push(J::from(n));
        }
    }
    J::Arr(names)
}

fn errs(v: Vec<String>) -> J {
    J::Arr(v.into_iter().map(J::from).collect())
}

/// A JSONL record: a header line when the file is new, then lines.
struct Jsonl {
    file: std::fs::File,
    path: PathBuf,
}

impl Jsonl {
    fn open(path: PathBuf, header: J) -> Result<Jsonl, String> {
        let fresh = std::fs::metadata(&path)
            .map(|m| m.len() == 0)
            .unwrap_or(true);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let mut j = Jsonl { file, path };
        if fresh {
            j.line(header.with("kind", "header"))?;
        }
        Ok(j)
    }

    fn line(&mut self, j: J) -> Result<(), String> {
        writeln!(self.file, "{}", j.render()).map_err(|e| format!("{}: {e}", self.path.display()))
    }

    fn lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.path)
            .map(|t| t.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }
}

/// Write a JSON record, with the step's error (if any) in it.
/// Run a step's body so that a panic becomes an Err and the record is
/// still written (the first gain-ladder run lost its record to a panic).
fn guarded(f: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(p) => {
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown panic".into());
            Err(format!("panic: {msg}"))
        }
    }
}

fn finish(name: &PathBuf, mut rec: J, r: Result<(), String>) -> Result<(), String> {
    if let Err(e) = &r {
        rec.set("aborted", e.as_str());
    }
    std::fs::write(name, rec.render() + "\n").map_err(|e| e.to_string())?;
    println!("wrote {}", name.display());
    r
}

fn header(step: &str, what: &str, o: &Opts) -> J {
    crate::header_with_source(step, what).with("options", o.record())
}

// ---- clkout-check ----

/// The first meter run died at R15 (data/step4-meter-run1-aborted-at-R15.json):
/// every USB request after the write of R15 = 0x38 (CLK_OUT_ENB = 1, the
/// tuner's clock output off) failed with kIOReturnNotResponding, R5-R14
/// having been accepted. Hypothesis: the demodulator is clocked from the
/// tuner's clock output ([R820T] 6.3 "Share Crystal"; [RTL2832U] 9.10).
/// Test and null: with CLK_OUT_ENB = 0 the whole image is written and the
/// dongle keeps answering; then R15 alone with CLK_OUT_ENB = 1 and the
/// dongle is asked again; then a power cycle brings it back. No source.
pub fn clkout_check(args: &[String]) -> Result<(), String> {
    let (o, _) = Opts::parse(args)?;
    usb::guard()?;
    let path = o.path("step4-clkout-check.json");
    let mut rec = header("4-clkout-check", "does the tuner's clock output clock the demodulator: the dongle's liveness with CLK_OUT_ENB 0, then 1, then after a power cycle", &o)
        .with("hypothesis", "R15 CLK_OUT_ENB = 1 (clock output off) stops the demodulator, so every USB request fails until a power cycle; = 0 keeps it alive");
    let r = guarded(|| {
        let lo = o.lo_hz(o.freq_hz);
        let pll = (0..4u8)
            .find_map(|c| Pll::program(lo, c, o.ref_div2).ok())
            .ok_or("no documented divider can program this LO")?;
        let front = o.front();
        let (rtl, setup) = tuner::fresh(&o.demod(), &front, &pll)?;
        rec.set("setup_clock_on", setup);
        let alive = |rtl: &Rtl| {
            J::obj()
                .with(
                    "sys_read",
                    rtl.sys_read8(DEMOD_CTL)
                        .map(hex8)
                        .map_err(|e| e.to_string())
                        .unwrap_or_else(J::from),
                )
                .with(
                    "tuner_status",
                    tuner::read_status(rtl)
                        .map(|s| tuner::status_record(&s))
                        .unwrap_or_else(|e| J::from(format!("error: {e}"))),
                )
        };
        rec.set("alive_with_clock_on", alive(&rtl));
        let mut off = front;
        off.clk_out_off = true;
        let img = tuner::image(&off, &pll);
        let w = tuner::write_regs(&rtl, &[(15, img[15])]);
        std::thread::sleep(Duration::from_millis(50));
        rec.set(
            "r15_clock_off_written",
            J::obj()
                .with("value", hex8(img[15]))
                .with("write_errors", errs(w)),
        );
        rec.set("alive_with_clock_off", alive(&rtl));
        let listed = usb::matching().len();
        rec.set("devices_listed_with_clock_off", listed);
        drop(rtl);
        let cyc = usb::power_cycle()?;
        let rtl = Rtl::open()?;
        rec.set("power_cycle", cyc);
        rec.set("alive_after_power_cycle", alive(&rtl));
        Ok(())
    });
    finish(&path, rec, r)
}

// ---- meter ----

/// The configurations the meter step compares: tuner configuration and
/// the demodulator's AGC pin polarity.
const METER_CONFIGS: [(Config, bool); 4] = [
    (Config::Manual, false),
    (Config::Vagc, false),
    (Config::Vagc, true),
    (Config::Auto, false),
];

/// (SEL_DIV code, divider hypothesis) pairs the divider probe tries.
fn divider_probe_space() -> Vec<(u8, u32)> {
    let mut v = Vec::new();
    for code in 0..8u8 {
        match tuner::divider(code) {
            Some(d) => v.push((code, d)),
            None => v.extend(tuner::UNDESCRIBED_DIV_HYPOTHESES.iter().map(|&d| (code, d))),
        }
    }
    v
}

pub fn meter(args: &[String]) -> Result<(), String> {
    let (o, _) = Opts::parse(args)?;
    let path = o.path("step4-meter.json");
    let mut rec = header(
        "4-meter",
        "every documented level readback with the tone off, on and across source levels, per tuner configuration; a probe of every SEL_DIV code first",
        &o,
    );
    rec.set(
        "candidate_space",
        J::obj()
            .with("readbacks", "dagc_val (p3 0x05, Table 10); if_agc_val (p3 0x59-0x5A), rf_agc_val (p3 0x5B-0x5C), aagc_lock (p3 0x50) (Table 2); sfreq_off (p3 0x18-0x19), cfreq_off (p3 0x5F-0x61) (Table 12); inc_det_cnt (p3 0x28-0x29) (Table 9); Est_kq, Est_sin (p1 0x66-0x69) (Table 5); tuner R1-R4 ([R820T2] Table 1-2); page 3 whole with --page3")
            .with("divider_probe", "every SEL_DIV code 0..=7 at the frequency; the documented codes with their divider, the undescribed ones under each hypothesis 1, 2, 4, 8, 16, 32; configurations manual and vagc; tone off then on at the start level; a readback that moves marks the code as locking")
            .with("configurations", "manual (every gain manual); vagc (VGA_MODE = 1, IF gain from the VAGC pin) with polar_if_agc 0 and 1; auto (LNA and mixer gain auto, detectors 1 and 3 on)")
            .with("source_ladder", format!("off; txvga {METER_TXVGA:?} x backoff {METER_BACKOFF:?} in ascending level; off again"))
            .with("snapshots_per_reading", o.snapshots.unwrap_or(meter::SNAPSHOTS)),
    );
    let r = guarded(|| meter_body(&o, &mut rec));
    finish(&path, rec, r)
}

fn meter_body(o: &Opts, rec: &mut J) -> Result<(), String> {
    usb::guard()?;
    let mut src = Source::open()?;
    rec.set("source", src.describe());
    let lo = o.lo_hz(o.freq_hz);
    let n = o.snapshots.unwrap_or(meter::SNAPSHOTS);
    // The bench starts with the first programmable documented code (or
    // --div); the probe retunes from there.
    let first = match o.div {
        Some(c) => Pll::program(lo, c, o.ref_div2)?,
        None => (0..4u8)
            .find_map(|c| Pll::program(lo, c, o.ref_div2).ok())
            .ok_or("no documented divider can program this LO")?,
    };
    let front = o.front();
    let (rtl, setup) = tuner::fresh(&o.demod(), &front, &first)?;
    rec.set("setup", setup);
    let start = Instant::now();
    let page3 = o.page3;

    // Phase 1: the divider probe.
    let mut probe = J::Arr(Vec::new());
    let mut responding: Vec<u8> = Vec::new();
    if o.div.is_none() {
        for (code, div) in divider_probe_space() {
            source::check()?;
            let pll = match Pll::program_with(lo, div, code, o.ref_div2) {
                Ok(p) => p,
                Err(e) => {
                    probe.push(
                        J::obj()
                            .with("sel_div", code)
                            .with("div_hypothesis", div)
                            .with("not_programmable", e),
                    );
                    continue;
                }
            };
            let (werr, status) = tuner::retune(&rtl, &pll);
            let mut configs = J::Arr(Vec::new());
            let mut any = false;
            for cfg in [Config::Manual, Config::Vagc] {
                let mut f = cfg.front();
                f.lna_gain = o.lna;
                f.mix_gain = o.mix;
                f.vga_code = o.vga;
                let img = tuner::image(&f, &pll);
                let ferr = tuner::write_regs(
                    &rtl,
                    &[(5, img[5]), (7, img[7]), (12, img[12]), (6, img[6])],
                );
                std::thread::sleep(TUNER_SETTLE);
                let off_src = src.off()?;
                let off = Reading::take_n(&rtl, start, false, None, n);
                let on_src = src.level(o.freq_hz, o.level)?;
                let on = Reading::take_n(&rtl, start, false, None, n);
                let mv = moved_names(&off, &on);
                if let J::Arr(v) = &mv
                    && !v.is_empty()
                {
                    any = true;
                }
                configs.push(
                    J::obj()
                        .with("config", cfg.name())
                        .with("front_write_errors", errs(ferr))
                        .with("off_source", off_src)
                        .with("off", off.medians())
                        .with("on_source", on_src)
                        .with("on", on.medians())
                        .with("moved", mv)
                        .with("off_raw", off.compact())
                        .with("on_raw", on.compact()),
                );
            }
            if any && !responding.contains(&code) {
                responding.push(code);
            }
            println!(
                "probe SEL_DIV {code:03b} /{div}: Nint {} -> {}",
                pll.nint,
                if any {
                    "a readback moved"
                } else {
                    "nothing moved"
                }
            );
            probe.push(
                J::obj()
                    .with("sel_div", code)
                    .with("div_hypothesis", div)
                    .with("pll", pll.record())
                    .with("retune_errors", errs(werr))
                    .with("status", status.map(|s| tuner::status_record(&s)))
                    .with("configs", configs)
                    .with("any_readback_moved", any),
            );
        }
    }
    rec.set("divider_probe", probe);
    rec.set(
        "responding_sel_div",
        J::Arr(responding.iter().map(|c| J::from(*c)).collect()),
    );

    // Phase 2: the full ladder on the responding codes (all four
    // documented ones if none responded), each configuration.
    let codes: Vec<u8> = match o.div {
        Some(c) => vec![c],
        None if responding.is_empty() => (0..4u8).collect(),
        None => responding.clone(),
    };
    rec.set(
        "ladder_sel_div",
        J::Arr(codes.iter().map(|c| J::from(*c)).collect()),
    );
    let mut ladders = J::Arr(Vec::new());
    for code in codes {
        let pll = match tuner::divider(code) {
            Some(_) => Pll::program(lo, code, o.ref_div2),
            // An undescribed code that responded: the hypothesis under
            // which it responded is in the probe; the ladder uses the
            // first of them that is programmable.
            None => tuner::UNDESCRIBED_DIV_HYPOTHESES
                .iter()
                .find_map(|&d| Pll::program_with(lo, d, code, o.ref_div2).ok())
                .ok_or("not programmable".to_string()),
        };
        let pll = match pll {
            Ok(p) => p,
            Err(e) => {
                ladders.push(J::obj().with("sel_div", code).with("error", e));
                continue;
            }
        };
        let (werr, status) = tuner::retune(&rtl, &pll);
        for (cfg, polar) in METER_CONFIGS {
            source::check()?;
            let mut f = cfg.front();
            f.lna_gain = o.lna;
            f.mix_gain = o.mix;
            f.vga_code = o.vga;
            if let Some(b) = o.r30_b6 {
                f.r30_b6 = b;
            }
            let img = tuner::image(&f, &pll);
            let applied = tuner::apply_image(&rtl, &img);
            let pol = rtl
                .demod_rmw(0, 0x0E, 0x03, (polar as u8) << 1 | polar as u8)
                .map(|(a, b)| J::obj().with("before", hex8(a)).with("after", hex8(b)))
                .unwrap_or_else(|e| J::from(format!("error: {e}")));
            std::thread::sleep(Duration::from_millis(50));
            let mut states = J::Arr(Vec::new());
            let mut series: Vec<(Option<Tone>, Reading)> = Vec::new();
            let push = |states: &mut J,
                        series: &mut Vec<(Option<Tone>, Reading)>,
                        src_rec: J,
                        tone: Option<Tone>,
                        r: Reading| {
                states.push(
                    J::obj()
                        .with("source", src_rec)
                        .with("medians", r.medians())
                        .with("snapshots", r.full()),
                );
                series.push((tone, r));
            };
            let s = src.off()?;
            push(
                &mut states,
                &mut series,
                s,
                None,
                Reading::take_n(&rtl, start, page3, None, n),
            );
            for txvga in METER_TXVGA {
                for backoff in METER_BACKOFF {
                    source::check()?;
                    let t = Tone {
                        hz: o.freq_hz,
                        txvga_db: txvga,
                        backoff_db: backoff,
                    };
                    let s = src.tone(t)?;
                    push(
                        &mut states,
                        &mut series,
                        s,
                        Some(t),
                        Reading::take_n(&rtl, start, page3, None, n),
                    );
                }
            }
            let s = src.off()?;
            push(
                &mut states,
                &mut series,
                s,
                None,
                Reading::take_n(&rtl, start, page3, None, n),
            );
            let analysis = analyse_ladder(&series);
            println!(
                "SEL_DIV {code:03b} {} polar {}: {}",
                cfg.name(),
                polar as u8,
                summary(&analysis)
            );
            ladders.push(
                J::obj()
                    .with("sel_div", code)
                    .with("pll", pll.record())
                    .with("retune_errors", errs(werr.clone()))
                    .with(
                        "status_after_retune",
                        status.map(|s| tuner::status_record(&s)),
                    )
                    .with("config", cfg.name())
                    .with("polar_if_agc_and_rf_agc", polar)
                    .with("polar_write", pol)
                    .with("front", f.record())
                    .with("tuner_applied", applied)
                    .with("states", states)
                    .with("analysis", analysis),
            );
        }
    }
    rec.set("ladders", ladders);
    src.quit();
    Ok(())
}

/// Per named meter over a ladder: the medians per level in ascending
/// order of level (txvga + backoff; the ladder itself is taken txvga
/// outer, backoff inner), the span, the number of direction reversals
/// along the sorted levels (0 = monotonic), and whether off -> the top
/// level moved. (The first meter record, data/step4-meter.json, was
/// analysed in ladder order; its per_level lists are in that order.)
fn analyse_ladder(series: &[(Option<Tone>, Reading)]) -> J {
    let mut out = J::obj();
    let mut on: Vec<&(Option<Tone>, Reading)> =
        series.iter().filter(|(t, _)| t.is_some()).collect();
    on.sort_by_key(|(t, _)| t.map(|t| t.level_db()).unwrap_or(0));
    let off_first = series.first().filter(|(t, _)| t.is_none());
    let top = on.last();
    out.set(
        "levels_db_sorted",
        J::Arr(
            on.iter()
                .filter_map(|(t, _)| t.map(|t| J::from(t.level_db())))
                .collect(),
        ),
    );
    for (m, name) in meter::ALL {
        let vals: Vec<Option<f64>> = on.iter().map(|(_, r)| r.median(m)).collect();
        let present: Vec<f64> = vals.iter().flatten().cloned().collect();
        if present.is_empty() {
            continue;
        }
        let span = present.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
            - present.iter().cloned().fold(f64::INFINITY, f64::min);
        let mut up = 0;
        let mut down = 0;
        for w in present.windows(2) {
            if w[1] > w[0] {
                up += 1;
            } else if w[1] < w[0] {
                down += 1;
            }
        }
        let reversals = up.min(down);
        let moved = match (off_first, top) {
            (Some((_, off)), Some((_, t))) => meter::moved(off, t, m).map(|x| x.2),
            _ => None,
        };
        out.set(
            name,
            J::obj()
                .with("per_level", J::Arr(vals.into_iter().map(J::from).collect()))
                .with("span", span)
                .with("steps_up", up)
                .with("steps_down", down)
                .with("reversals", reversals)
                .with(
                    "rises_with_signal",
                    if up > down {
                        Some(true)
                    } else if down > up {
                        Some(false)
                    } else {
                        None
                    },
                )
                .with("off_to_top_moved", moved),
        );
    }
    out
}

fn summary(a: &J) -> String {
    let mut s = Vec::new();
    if let J::Obj(fields) = a {
        for (name, v) in fields {
            if let J::Obj(f) = v {
                let get = |k: &str| {
                    f.iter()
                        .find(|(x, _)| x == k)
                        .map(|(_, v)| v.render())
                        .unwrap_or_default()
                };
                if get("off_to_top_moved") == "true" {
                    s.push(format!(
                        "{name} span {} reversals {}",
                        get("span"),
                        get("reversals")
                    ));
                }
            }
        }
    }
    if s.is_empty() {
        "no readback moved".into()
    } else {
        s.join("; ")
    }
}

// ---- tune ----

pub fn tune(args: &[String]) -> Result<(), String> {
    let (mut o, pos) = Opts::parse(args)?;
    if let Some(h) = pos.first() {
        o.freq_hz = num(h, "tune HZ")?;
    }
    let path = o.path(&format!("step4-tune-{:.3}MHz.json", o.freq_hz / 1e6));
    let mut rec = header(
        "4-tune",
        "one frequency: the PLL words from the documented formula, the tuner's status, every readback with the tone off and on",
        &o,
    );
    let r = guarded(|| {
        let mut b = bench(&o)?;
        rec.set("source", b.src.describe());
        rec.set("setup", b.setup.clone());
        let n = b.snapshots(&o, meter::SNAPSHOTS);
        let off_src = b.src.off()?;
        let off = b.reading(&o, n);
        let on_src = b.src.level(o.freq_hz, o.level)?;
        let on = b.reading(&o, n);
        let status = tuner::read_status(&b.rtl)
            .map(|s| tuner::status_record(&s))
            .map_err(|e| e.to_string());
        let mv = moved_names(&off, &on);
        println!(
            "tune {} Hz: LO {:?} Hz, SEL_DIV {:03b}, Nint {}; moved: {}",
            o.freq_hz,
            b.pll.lo_hz(),
            b.pll.sel_div,
            b.pll.nint,
            mv.render()
        );
        rec.set("pll", b.pll.record());
        rec.set("off_source", off_src);
        rec.set(
            "off",
            J::obj()
                .with("medians", off.medians())
                .with("snapshots", off.full()),
        );
        rec.set("on_source", on_src);
        rec.set(
            "on",
            J::obj()
                .with("medians", on.medians())
                .with("snapshots", on.full()),
        );
        rec.set("status_with_tone", status.ok());
        rec.set("moved", mv);
        b.src.quit();
        Ok(())
    });
    finish(&path, rec, r)
}

// ---- pll-sweep ----

pub fn pll_sweep(args: &[String]) -> Result<(), String> {
    let (o, _) = Opts::parse(args)?;
    usb::guard()?;
    let path = o.path("step4-pll-sweep.jsonl");
    let hdr = header("4-pll-sweep", "every SEL_DIV code x Nint in steps: the tuner's R0-R4 after each, and the readbacks with the tone at the LO each divider hypothesis predicts", &o)
        .with("candidate_space", format!("SEL_DIV 0..=7 x Nint {}..={} step {} (the last value included) with Nfra = 0, REF_DIV2 {}; documented codes under their divider, undescribed codes under each of {:?}", tuner::NINT_MIN, tuner::NINT_MAX, o.nint_step, o.ref_div2 as u8, tuner::UNDESCRIBED_DIV_HYPOTHESES))
        .with("hypothesis", "f_LO = f_ref * 2 * (Nint + Nfra) / div; the tone is placed at the RF whose IF lands on the DDC for that LO (the IF side per --lo-side) and the readbacks are read; a readback that moves off -> on shows the PLL locked there and the hypothesis holds for that code")
        .with("procedure", "write R16, R20, R21, R22; 10 ms; read R0-R4; 50 ms more; read again; tone off; reading; for each hypothesis: tone on at its RF (skipped when outside the source's range); reading; tone off")
        .with("line_legend", "k pll; sd SEL_DIV; rd REF_DIV2; n Nint; s0 R0-R4 bit-reversed at 10 ms, s1 at 60 ms; off the medians of the named meters with the tone off; on per hypothesis: div, lo, rf (Hz), m medians, mv the meters that moved vs off, r raw snapshots (d dagc_val, a AGC bytes 0x59-0x5C, l aagc_lock, t R0-R4), or skip; e errors");
    let mut out = Jsonl::open(path, hdr)?;
    let mut src = Source::open()?;
    let lo0 = o.lo_hz(o.freq_hz);
    let first = (0..4u8)
        .find_map(|c| Pll::program(lo0, c, o.ref_div2).ok())
        .ok_or("no documented divider can program the start LO")?;
    let (rtl, setup) = tuner::fresh(&o.demod(), &o.front(), &first)?;
    out.line(setup.with("kind", "setup").with("source", src.describe()))?;
    let start = Instant::now();
    let n = o.snapshots.unwrap_or(1);
    let (n_lo, n_hi) = (
        o.nint_from.unwrap_or(tuner::NINT_MIN).max(tuner::NINT_MIN),
        o.nint_to.unwrap_or(tuner::NINT_MAX).min(tuner::NINT_MAX),
    );
    let mut nints: Vec<u16> = (n_lo..=n_hi).step_by(o.nint_step as usize).collect();
    if nints.last() != Some(&n_hi) {
        nints.push(n_hi);
    }
    let codes: Vec<u8> = match o.div {
        Some(c) => vec![c],
        None => (0..8u8).collect(),
    };
    out.line(
        J::obj()
            .with("kind", "span")
            .with(
                "sel_div",
                J::Arr(codes.iter().map(|c| J::from(*c)).collect()),
            )
            .with("nint_from", n_lo)
            .with("nint_to", n_hi)
            .with("nint_step", o.nint_step),
    )?;
    let mut count = 0usize;
    'codes: for code in codes {
        let hyps: Vec<u32> = match tuner::divider(code) {
            Some(d) => vec![d],
            None => tuner::UNDESCRIBED_DIV_HYPOTHESES.to_vec(),
        };
        for &nint in &nints {
            source::check()?;
            if o.limit.is_some_and(|l| count >= l) {
                break 'codes;
            }
            count += 1;
            let pll = Pll {
                sel_div: code,
                ref_div2: o.ref_div2,
                nint,
                sdm: 0,
            };
            let mut e = Vec::new();
            let (werr, s0) = tuner::retune(&rtl, &pll);
            e.extend(werr);
            std::thread::sleep(Duration::from_millis(50));
            let s1 = tuner::read_status(&rtl).ok();
            src.off()?;
            let off = Reading::take_n(&rtl, start, false, None, n);
            let mut on = J::Arr(Vec::new());
            for div in hyps.iter().copied() {
                let lo = pll.lo_hz_for(div);
                let rf = o.rf_hz(lo);
                if !source_in_range(rf) {
                    on.push(
                        J::obj()
                            .with("div", div)
                            .with("lo", lo)
                            .with("rf", rf)
                            .with("skip", "outside the source's range"),
                    );
                    continue;
                }
                src.level(rf, o.level)?;
                let r = Reading::take_n(&rtl, start, false, None, n);
                on.push(
                    J::obj()
                        .with("div", div)
                        .with("lo", lo)
                        .with("rf", rf)
                        .with("m", r.medians())
                        .with("mv", moved_names(&off, &r))
                        .with("r", r.compact()),
                );
            }
            src.off()?;
            let mut line = J::obj()
                .with("k", "pll")
                .with("sd", code)
                .with("rd", o.ref_div2 as u8)
                .with("n", nint)
                .with(
                    "s0",
                    s0.map(|s| J::Arr(s.iter().map(|b| J::from(*b)).collect())),
                )
                .with(
                    "s1",
                    s1.map(|s| J::Arr(s.iter().map(|b| J::from(*b)).collect())),
                )
                .with("off", off.medians())
                .with("on", on);
            if !e.is_empty() {
                line.set("e", errs(e));
            }
            out.line(line)?;
        }
        println!("SEL_DIV {code:03b} done ({count} settings so far)");
    }
    src.quit();
    Ok(())
}

// ---- lo-find ----

/// Hypothesis-free LO search for one SEL_DIV code: the PLL held at one
/// Nint inside the VCO range, the tone stepped across the source's range,
/// the readbacks at each step. The receiver answers where the tone is an
/// IF away from the LO (either side), so the LO is read off the record,
/// and LO / f_VCO is the code's divider. Written for the undescribed
/// codes 100-111, whose six hypotheses in `pll-sweep` did not fit.
pub fn lo_find(args: &[String]) -> Result<(), String> {
    let (o, _) = Opts::parse(args)?;
    let code = o.div.ok_or("lo-find needs --div CODE")?;
    usb::guard()?;
    let nint = o.nint.unwrap_or(88);
    let pll = Pll {
        sel_div: code,
        ref_div2: o.ref_div2,
        nint,
        sdm: 0,
    };
    let path = o.path(&format!("step4-lo-find-sel{code:03b}.jsonl"));
    let hdr = header("4-lo-find", "where the receiver answers a tone stepped across the source's range, with the PLL held at one Nint: the LO for one SEL_DIV code, without a divider hypothesis", &o)
        .with("candidate_space", format!("SEL_DIV {code:03b}, Nint {nint} (f_VCO {} Hz under f_VCO = f_ref * 2 * Nint), Nfra 0, REF_DIV2 {}; the tone's RF from {} Hz to {} Hz in {} Hz steps, ascending", pll.vco_hz(), o.ref_div2 as u8, o.rf_from_hz, o.rf_to_hz, o.rf_step_hz))
        .with("how_the_lo_is_read", "the receiver's IF path passes about 0.57-8.57 MHz either side of the LO (the 8 MHz channel at IF 4.57 MHz), so a response spans two windows around the LO; the LO is their centre and the divider is f_VCO / LO")
        .with("line_legend", "k lof; rf the tone's RF (Hz); m the medians of every named meter; mv the meters that moved vs the off reading; r raw; e errors");
    let mut out = Jsonl::open(path, hdr)?;
    let mut src = Source::open()?;
    let (rtl, setup) = tuner::fresh(&o.demod(), &o.front(), &pll)?;
    out.line(setup.with("kind", "setup").with("source", src.describe()))?;
    let start = Instant::now();
    let n = o.snapshots.unwrap_or(1);
    src.off()?;
    let off = Reading::take_n(&rtl, start, false, None, n);
    out.line(
        J::obj()
            .with("k", "off")
            .with("m", off.medians())
            .with("r", off.compact()),
    )?;
    let steps = ((o.rf_to_hz - o.rf_from_hz) / o.rf_step_hz).floor() as usize + 1;
    for k in 0..steps {
        source::check()?;
        if o.limit.is_some_and(|l| k >= l) {
            break;
        }
        let rf = o.rf_from_hz + k as f64 * o.rf_step_hz;
        if !source_in_range(rf) {
            continue;
        }
        src.level(rf, o.level)?;
        let r = Reading::take_n(&rtl, start, false, None, n);
        out.line(
            J::obj()
                .with("k", "lof")
                .with("rf", rf)
                .with("m", r.medians())
                .with("mv", moved_names(&off, &r))
                .with("r", r.compact()),
        )?;
        if k % 50 == 0 {
            println!("SEL_DIV {code:03b}: {:.0} MHz", rf / 1e6);
        }
    }
    src.quit();
    Ok(())
}

// ---- gain-ladder ----

#[derive(Clone, Copy, Debug)]
enum Stage {
    Lna,
    Mix,
    Vga,
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Stage::Lna => "LNA_GAIN (R5[3:0], 15 max .. 0 min)",
            Stage::Mix => "MIX_GAIN (R7[3:0], 0000 min .. 1111 max)",
            Stage::Vga => "VGA_CODE (R12[3:0], 0000 -12.0 dB .. 1111 +40.5 dB)",
        }
    }

    fn reg(self) -> u8 {
        match self {
            Stage::Lna => 5,
            Stage::Mix => 7,
            Stage::Vga => 12,
        }
    }

    fn set(self, f: &mut Front, code: u8) {
        match self {
            Stage::Lna => f.lna_gain = code,
            Stage::Mix => f.mix_gain = code,
            Stage::Vga => f.vga_code = code,
        }
    }
}

/// The comparator meter for a stage the loop cannot see through: with
/// VGA_MODE = 0 the IF loop's output drives nothing and `if_agc_val`
/// (MSB-first) rails at +8191 below the loop's target and -8192 above
/// it (data/step4-meter.json, manual configuration). A bisection on the
/// source's relative level in 1 dB steps (backoff, exact, for levels
/// <= 0; txvga above) finds the level at which it flips; the gain of a
/// code is minus that level, relative. Every reading is recorded.
const THRESHOLD_LO_DB: i32 = source::BACKOFF_FLOOR_DB;
const THRESHOLD_HI_DB: i32 = source::TXVGA_MAX;

fn level_for_db(db: i32) -> Level {
    if db <= 0 {
        Level {
            txvga_db: 0,
            backoff_db: db,
        }
    } else {
        Level {
            txvga_db: db,
            backoff_db: 0,
        }
    }
}

fn threshold_search(
    src: &mut Source,
    rtl: &Rtl,
    start: Instant,
    hz: f64,
    n: usize,
) -> Result<(Option<i32>, J), String> {
    let mut log = J::Arr(Vec::new());
    let above = |r: &Reading| r.median(meter::Meter::IfAgcMsbFirst).map(|v| v < 0.0);
    let mut probe = |db: i32, log: &mut J| -> Result<Option<bool>, String> {
        source::check()?;
        src.level(hz, level_for_db(db))?;
        let r = Reading::take_n(rtl, start, false, None, n);
        let a = above(&r);
        log.push(
            J::obj()
                .with("level_db", db)
                .with(
                    "if_agc_val_msb_first",
                    r.median(meter::Meter::IfAgcMsbFirst),
                )
                .with("above_target", a)
                .with("r", r.compact()),
        );
        Ok(a)
    };
    let (mut lo, mut hi) = (THRESHOLD_LO_DB, THRESHOLD_HI_DB);
    let done = |log: J, stop: Option<&str>| J::obj().with("readings", log).with("stop", stop);
    match probe(lo, &mut log)? {
        Some(false) => {}
        _ => {
            return Ok((
                None,
                done(log, Some("already above the target at the lowest level")),
            ));
        }
    }
    match probe(hi, &mut log)? {
        Some(true) => {}
        _ => {
            return Ok((
                None,
                done(log, Some("still below the target at the highest level")),
            ));
        }
    }
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        match probe(mid, &mut log)? {
            Some(true) => hi = mid,
            Some(false) => lo = mid,
            None => return Ok((None, done(log, Some("the meter could not be read")))),
        }
    }
    Ok((Some(hi), done(log, None)))
}

pub fn gain_ladder(args: &[String]) -> Result<(), String> {
    let (o, _) = Opts::parse(args)?;
    let spec = o
        .meter
        .ok_or("gain-ladder needs --meter NAME LO HI up|down (from the meter step's record)")?;
    let path = o.path("step4-gain-ladder.json");
    let mut rec = header("4-gain-ladder", "LNA_GAIN, MIX_GAIN and VGA_CODE 0..=15, one stage at a time with the other two fixed, every readback at each code with the source re-levelled to keep the meter mid-range", &o)
        .with("candidate_space", "stage in [LNA_GAIN, MIX_GAIN, VGA_CODE] x code 0..=15, ascending; the other stages at --lna/--mix/--vga (default 8 each); tuner configuration manual")
        .with("procedure", format!("tone at --freq; the meter re-levelled at the fixed codes first; per code: the register written, {} ms, re-level (source moved in {} dB steps, backoff first, every move recorded), reading; after a stage its register goes back to the fixed code. A gain difference between codes = (meter difference) + (source difference) in the record", TUNER_SETTLE.as_millis(), meter::RELEVEL_STEP_DB))
        .with("meter_spec", spec.record());
    let r = guarded(|| {
        let mut b = bench(&o)?;
        rec.set("source", b.src.describe());
        rec.set("setup", b.setup.clone());
        let n = b.snapshots(&o, meter::SNAPSHOTS);
        let mut level = o.level;
        b.src.off()?;
        rec.set("off_before", b.reading(&o, n).full());
        if b.front.vga_from_pin {
            b.src.level(o.freq_hz, level)?;
            let (r0, adj0) = meter::relevel(
                &mut b.src, &b.rtl, b.start, &spec, o.freq_hz, &mut level, o.page3,
            )?;
            rec.set(
                "initial_relevel",
                J::obj()
                    .with("adjustments", adj0)
                    .with("reading", r0.full())
                    .with("source", level.tone(o.freq_hz).record()),
            );
        } else {
            rec.set(
                "initial_relevel",
                "none: manual configuration, every stage by the comparator threshold",
            );
        }
        let base = b.front;
        let mut stages = J::Arr(Vec::new());
        for stage in [Stage::Lna, Stage::Mix, Stage::Vga] {
            let mut codes = J::Arr(Vec::new());
            // The VGA is inside the loop that the meter reads, so its code
            // is measured with the loop opened (VGA_MODE = 0) by the
            // comparator threshold instead; the loop closes again after.
            // In the manual configuration the loop is open for every
            // stage, so every stage is measured by the comparator.
            let comparator = matches!(stage, Stage::Vga) || !base.vga_from_pin;
            for code in 0..=15u8 {
                source::check()?;
                let mut f = base;
                stage.set(&mut f, code);
                if comparator {
                    f.vga_from_pin = false;
                }
                let werr = b.write_front(f, &[stage.reg()]);
                if comparator {
                    let (t, log) = threshold_search(&mut b.src, &b.rtl, b.start, o.freq_hz, n)?;
                    println!(
                        "{} code {code:2}: comparator threshold at source {:?} dB",
                        stage.name(),
                        t
                    );
                    codes.push(
                        J::obj()
                            .with("code", code)
                            .with("write_errors", errs(werr))
                            .with("threshold_level_db", t)
                            .with("search", log),
                    );
                    continue;
                }
                let (reading, adj) = meter::relevel(
                    &mut b.src, &b.rtl, b.start, &spec, o.freq_hz, &mut level, o.page3,
                )?;
                let v = reading.median(spec.meter);
                println!(
                    "{} code {code:2}: meter {:?} at source {:+} dB",
                    stage.name(),
                    v,
                    level.tone(o.freq_hz).level_db()
                );
                codes.push(
                    J::obj()
                        .with("code", code)
                        .with("write_errors", errs(werr))
                        .with("relevel", adj)
                        .with("source", level.tone(o.freq_hz).record())
                        .with("meter_value", v)
                        .with("medians", reading.medians())
                        .with("snapshots", reading.full()),
                );
            }
            let werr = b.write_front(base, &[stage.reg()]);
            stages.push(
                J::obj()
                    .with("stage", stage.name())
                    .with("register", stage.reg())
                    .with("method", if comparator { "comparator threshold: VGA_MODE = 0, the source level bisected in 1 dB steps to the flip of if_agc_val (MSB-first) between +8191 and -8192; gain difference between codes = minus the threshold difference" } else { "loop meter: VGA_MODE = 1, if_agc_val (MSB-first) with the source re-levelled; gain difference = meter difference / slope + source difference" })
                    .with("codes", codes)
                    .with("restore_errors", errs(werr)),
            );
        }
        rec.set("stages", stages);
        b.src.off()?;
        rec.set("off_after", b.reading(&o, n).full());
        b.src.quit();
        Ok(())
    });
    finish(&path, rec, r)
}

// ---- band-table ----

fn bt_done(lines: &[String]) -> HashSet<(u32, u8, u8, u8, u8)> {
    let mut set = HashSet::new();
    for l in lines {
        if !l.starts_with("{\"k\":\"bt\"") {
            continue;
        }
        let f = |k| crate::bytescan::field(l, k);
        if let (Some(f0), Some(rf), Some(mx), Some(n), Some(lp)) =
            (f("f"), f("rf"), f("mx"), f("n"), f("l"))
        {
            set.insert((f0, rf as u8, mx as u8, n as u8, lp as u8));
        }
    }
    set
}

pub fn band_table(args: &[String]) -> Result<(), String> {
    let (o, _) = Opts::parse(args)?;
    let spec = o
        .meter
        .ok_or("band-table needs --meter NAME LO HI up|down")?;
    usb::guard()?;
    let path = o.path("step4-band-table.jsonl");
    let hdr = header("4-band-table", "a frequency grid across the tuner's range; at each frequency the meter for every front-end path and corner code", &o)
        .with("candidate_space", format!("frequency {} MHz to {} MHz in {} MHz steps (the last included), those within --rf-from {} MHz .. --rf-to {} MHz, x RFFILT 0..=3 x RFMUX 0..=3 x TF_NCH 0..=15 x TF_LP 0..=15", TUNER_MIN_HZ / 1e6, TUNER_MAX_HZ / 1e6, o.grid_mhz, o.rf_from_hz / 1e6, o.rf_to_hz / 1e6))
        .with("search", if o.full {
            "full: every one of the 4096 combinations at every frequency".to_string()
        } else {
            "staged: stage A every RFFILT x RFMUX (16) with TF_NCH = TF_LP = 0000 (both corners highest); stage B the two best paths of stage A x every TF_NCH x TF_LP (512). The mux and band fields select a path and the corner codes tune within it; stage A ranks paths with the corners at their least-attenuating codes, so the staging loses nothing unless a path is best only with a corner lowered, which --full at sample frequencies checks".to_string()
        })
        .with("procedure", format!("per frequency: LO for the RF (IF side per --lo-side), PLL words, R16/R20-R22 written, status read; tone on; the source re-levelled at the reference path (RFMUX 01 bypass, RFFILT 00, TF 0000/0000) so the meter sits in its window; then the source stays fixed for every combination at that frequency; per combination R26 and R27 written, {} ms, a reading of {} snapshot(s)", TUNER_SETTLE.as_millis(), o.snapshots.unwrap_or(1)))
        .with("meter_spec", spec.record())
        .with("line_legend", "k ref: the frequency's reference (f, pll, status, relevel, source, m); k bt: f Hz, rf RFFILT, mx RFMUX, n TF_NCH, l TF_LP, m the meter's median, md the medians of every named meter, r raw snapshots; e errors; resumable by (f, rf, mx, n, l)");
    let mut out = Jsonl::open(path, hdr)?;
    let done = bt_done(&out.lines());
    let mut src = Source::open()?;
    let mut freqs: Vec<f64> = Vec::new();
    let mut f = TUNER_MIN_HZ;
    while f < TUNER_MAX_HZ {
        freqs.push(f);
        f += o.grid_mhz * 1e6;
    }
    freqs.push(TUNER_MAX_HZ);
    // The grid stays anchored at TUNER_MIN_HZ; --rf-from / --rf-to (2 to
    // 1300 MHz by default, the whole grid) keep a span of it.
    freqs.retain(|&f| f >= o.rf_from_hz && f <= o.rf_to_hz);
    if freqs.is_empty() {
        return Err(format!(
            "no grid frequency within {} .. {} Hz",
            o.rf_from_hz, o.rf_to_hz
        ));
    }
    let reference = Front {
        rfmux: 0b01,
        rffilt: 0b00,
        tf_nch: 0,
        tf_lp: 0,
        ..o.front()
    };
    let first = o.pll(o.lo_hz(freqs[0]))?;
    let (rtl, setup) = tuner::fresh(&o.demod(), &reference, &first)?;
    out.line(setup.with("kind", "setup").with("source", src.describe()))?;
    let start = Instant::now();
    let n = o.snapshots.unwrap_or(1);
    let mut level = o.level;
    let paths: Vec<(u8, u8)> = (0..4u8)
        .flat_map(|rf| (0..4u8).map(move |mx| (rf, mx)))
        .collect();
    let mut count = 0usize;
    for f_hz in freqs {
        source::check()?;
        if o.limit.is_some_and(|l| count >= l) {
            break;
        }
        let fk = f_hz.round() as u32;
        let all_done = |combos: &[(u8, u8, u8, u8)]| {
            combos
                .iter()
                .all(|&(rf, mx, nch, lp)| done.contains(&(fk, rf, mx, nch, lp)))
        };
        let stage_a: Vec<(u8, u8, u8, u8)> = paths.iter().map(|&(rf, mx)| (rf, mx, 0, 0)).collect();
        if !o.full && all_done(&stage_a) && done.iter().filter(|d| d.0 == fk).count() >= 16 + 512 {
            continue;
        }
        if o.full
            && all_done(
                &paths
                    .iter()
                    .flat_map(|&(rf, mx)| {
                        (0..16u8).flat_map(move |nch| (0..16u8).map(move |lp| (rf, mx, nch, lp)))
                    })
                    .collect::<Vec<_>>(),
            )
        {
            continue;
        }
        let lo = o.lo_hz(f_hz);
        let pll = match o.pll(lo) {
            Ok(p) => p,
            Err(e) => {
                out.line(
                    J::obj()
                        .with("k", "ref")
                        .with("f", fk)
                        .with("e", errs(vec![format!("pll: {e}")])),
                )?;
                continue;
            }
        };
        let (werr, status) = tuner::retune(&rtl, &pll);
        let img = tuner::image(&reference, &pll);
        let ferr = tuner::write_regs(&rtl, &[(26, img[26]), (27, img[27])]);
        std::thread::sleep(TUNER_SETTLE);
        src.level(f_hz, level)?;
        let (r, adj) = meter::relevel(&mut src, &rtl, start, &spec, f_hz, &mut level, false)?;
        out.line(
            J::obj()
                .with("k", "ref")
                .with("f", fk)
                .with("pll", pll.record())
                .with("status", status.map(|s| tuner::status_record(&s)))
                .with("relevel", adj)
                .with("source", level.tone(f_hz).record())
                .with("m", r.median(spec.meter))
                .with("md", r.medians())
                .with("e", errs(werr.into_iter().chain(ferr).collect())),
        )?;
        let measure =
            |out: &mut Jsonl, rf: u8, mx: u8, nch: u8, lp: u8| -> Result<Option<f64>, String> {
                source::check()?;
                if done.contains(&(fk, rf, mx, nch, lp)) {
                    return Ok(None);
                }
                let fr = Front {
                    rfmux: mx,
                    rffilt: rf,
                    tf_nch: nch,
                    tf_lp: lp,
                    ..reference
                };
                let img = tuner::image(&fr, &pll);
                let e = tuner::write_regs(&rtl, &[(26, img[26]), (27, img[27])]);
                std::thread::sleep(TUNER_SETTLE);
                let r = Reading::take_n(&rtl, start, false, spec.meter.page3_offset(), n);
                let m = r.median(spec.meter);
                let mut line = J::obj()
                    .with("k", "bt")
                    .with("f", fk)
                    .with("rf", rf)
                    .with("mx", mx)
                    .with("n", nch)
                    .with("l", lp)
                    .with("m", m)
                    .with("md", r.medians())
                    .with("r", r.compact());
                if !e.is_empty() {
                    line.set("e", errs(e));
                }
                out.line(line)?;
                Ok(m)
            };
        if o.full {
            for &(rf, mx) in &paths {
                for nch in 0..16u8 {
                    for lp in 0..16u8 {
                        measure(&mut out, rf, mx, nch, lp)?;
                    }
                }
            }
        } else {
            let mut ranked: Vec<((u8, u8), f64)> = Vec::new();
            for &(rf, mx) in &paths {
                if let Some(m) = measure(&mut out, rf, mx, 0, 0)? {
                    ranked.push(((rf, mx), m));
                }
            }
            // Best = most signal: the largest reading if the meter rises
            // with signal, else the smallest.
            ranked.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            if spec.rises_with_signal {
                ranked.reverse();
            }
            let best: Vec<(u8, u8)> = ranked.iter().take(2).map(|(p, _)| *p).collect();
            out.line(
                J::obj().with("k", "stage_a").with("f", fk).with(
                    "best_paths_rffilt_rfmux",
                    J::Arr(
                        best.iter()
                            .map(|(rf, mx)| J::Arr(vec![J::from(*rf), J::from(*mx)]))
                            .collect(),
                    ),
                ),
            )?;
            for (rf, mx) in best {
                for nch in 0..16u8 {
                    for lp in 0..16u8 {
                        measure(&mut out, rf, mx, nch, lp)?;
                    }
                }
            }
        }
        count += 1;
        println!(
            "{:.1} MHz done (source {:+} dB)",
            f_hz / 1e6,
            level.tone(f_hz).level_db()
        );
    }
    src.quit();
    Ok(())
}

// ---- path-settle ----

/// The frequencies `path-settle` measures at: two where the band table's
/// single snapshots threw outliers (152, 202 MHz, the /16 span) and two
/// spread over the rest of the range.
const SETTLE_FREQS_HZ: [f64; 4] = [152e6, 202e6, 502e6, 852e6];
/// The paths `path-settle` switches between, (RFFILT, RFMUX): a dead one
/// (it passes nothing at every frequency of step 4f), the bypass, and the
/// two that won stage B most.
const SETTLE_PATHS: [(u8, u8); 4] = [(0b11, 0b11), (0b00, 0b01), (0b10, 0b01), (0b00, 0b00)];
/// How long the meter is read back-to-back after each switch, and how long
/// the `from` path is held before it.
const SETTLE_WINDOW: Duration = Duration::from_millis(1000);
/// Each transition this many times.
const SETTLE_REPS: usize = 5;
/// The tail of the window whose median is the settled value.
const SETTLE_TAIL_MS: f64 = 200.0;

/// How long the meter takes to settle after a front-end path change: at
/// each of [`SETTLE_FREQS_HZ`], every ordered pair of [`SETTLE_PATHS`]
/// (each path to itself as well, the null: the write alone), the `from`
/// path written and held [`SETTLE_WINDOW`], then the `to` path written and
/// the meter's register pair read back-to-back for [`SETTLE_WINDOW`].
pub fn path_settle(args: &[String]) -> Result<(), String> {
    let (o, _) = Opts::parse(args)?;
    let spec = o
        .meter
        .ok_or("path-settle needs --meter NAME LO HI up|down")?;
    usb::guard()?;
    let path = o.path("step4-path-settle.jsonl");
    let hdr = header("4-path-settle", "the meter's settling after a front-end path change (R26 RFFILT/RFMUX written, TF 0000/0000)", &o)
        .with("candidate_space", format!("RF {:?} Hz x (from, to) over every ordered pair of (RFFILT, RFMUX) {:?}, self-pairs included, x {} repetitions", SETTLE_FREQS_HZ, SETTLE_PATHS, SETTLE_REPS))
        .with("procedure", format!("per frequency: LO for the RF, PLL words written, the source re-levelled once at the bypass path (RFFILT 00, RFMUX 01) and then fixed; per transition: the from path written, {} ms held, the to path written, then page 3 0x59-0x5C read back-to-back for {} ms, each read timestamped from the write's return; the settled value is the median of the last {} ms", SETTLE_WINDOW.as_millis(), SETTLE_WINDOW.as_millis(), SETTLE_TAIL_MS))
        .with("meter_spec", spec.record())
        .with("line_legend", "k ref: the frequency's reference (f, pll, status, relevel, source, m); k tr: f Hz, rep, from and to [RFFILT, RFMUX], t ms after the write, m the meter per read (null if the read failed), fin the settled value; e errors");
    let mut out = Jsonl::open(path, hdr)?;
    let mut src = Source::open()?;
    let reference = Front {
        rfmux: 0b01,
        rffilt: 0b00,
        tf_nch: 0,
        tf_lp: 0,
        ..o.front()
    };
    let first = o.pll(o.lo_hz(SETTLE_FREQS_HZ[0]))?;
    let (rtl, setup) = tuner::fresh(&o.demod(), &reference, &first)?;
    out.line(setup.with("kind", "setup").with("source", src.describe()))?;
    let start = Instant::now();
    let mut level = o.level;
    for f_hz in SETTLE_FREQS_HZ {
        source::check()?;
        let fk = f_hz.round() as u32;
        let pll = o.pll(o.lo_hz(f_hz))?;
        let (werr, status) = tuner::retune(&rtl, &pll);
        let img = tuner::image(&reference, &pll);
        let ferr = tuner::write_regs(&rtl, &[(26, img[26]), (27, img[27])]);
        std::thread::sleep(TUNER_SETTLE);
        src.level(f_hz, level)?;
        let (r, adj) = meter::relevel(&mut src, &rtl, start, &spec, f_hz, &mut level, false)?;
        out.line(
            J::obj()
                .with("k", "ref")
                .with("f", fk)
                .with("pll", pll.record())
                .with("status", status.map(|s| tuner::status_record(&s)))
                .with("relevel", adj)
                .with("source", level.tone(f_hz).record())
                .with("m", r.median(spec.meter))
                .with("e", errs(werr.into_iter().chain(ferr).collect())),
        )?;
        let write_path = |(rf, mx): (u8, u8)| {
            let img = tuner::image(
                &Front {
                    rffilt: rf,
                    rfmux: mx,
                    ..reference
                },
                &pll,
            );
            tuner::write_regs(&rtl, &[(26, img[26]), (27, img[27])])
        };
        for rep in 0..SETTLE_REPS {
            for from in SETTLE_PATHS {
                for to in SETTLE_PATHS {
                    source::check()?;
                    let mut e = write_path(from);
                    std::thread::sleep(SETTLE_WINDOW);
                    e.extend(write_path(to));
                    let t0 = Instant::now();
                    let (mut ts, mut ms): (Vec<f64>, Vec<Option<f64>>) = (Vec::new(), Vec::new());
                    while t0.elapsed() < SETTLE_WINDOW {
                        let t = t0.elapsed().as_secs_f64() * 1e3;
                        let m = rtl.demod_read(3, 0x59, 4).ok().and_then(|v| {
                            meter::Snapshot {
                                agc: Some([v[0], v[1], v[2], v[3]]),
                                ..Default::default()
                            }
                            .value(spec.meter)
                        });
                        ts.push((t * 10.0).round() / 10.0);
                        ms.push(m);
                    }
                    let tail_from = SETTLE_WINDOW.as_secs_f64() * 1e3 - SETTLE_TAIL_MS;
                    let mut tail: Vec<f64> = ts
                        .iter()
                        .zip(&ms)
                        .filter(|(t, _)| **t >= tail_from)
                        .filter_map(|(_, m)| *m)
                        .collect();
                    tail.sort_by(f64::total_cmp);
                    let fin = tail.get(tail.len() / 2).copied();
                    let pair = |(a, b): (u8, u8)| J::Arr(vec![J::from(a), J::from(b)]);
                    let mut line = J::obj()
                        .with("k", "tr")
                        .with("f", fk)
                        .with("rep", rep)
                        .with("from", pair(from))
                        .with("to", pair(to))
                        .with("t", J::Arr(ts.into_iter().map(J::from).collect()))
                        .with(
                            "m",
                            J::Arr(ms.into_iter().map(|m| m.map_or(J::Null, J::from)).collect()),
                        )
                        .with("fin", fin);
                    if !e.is_empty() {
                        line.set("e", errs(e));
                    }
                    out.line(line)?;
                }
            }
            println!("{:.1} MHz rep {} done", f_hz / 1e6, rep + 1);
        }
    }
    src.quit();
    Ok(())
}

// ---- band-paths ----

/// After a path change, how long before the meter is read: twice the
/// ~150 ms the switch's transient lasted in `path-settle` (findings step
/// 4f2: up to ~6000 counts in the first 50 ms, like an unswitched path's
/// wander from 150 ms on).
const PATH_SETTLE: Duration = Duration::from_millis(300);
/// How long the meter is read back-to-back and averaged: `path-settle`'s
/// mean over 300-1000 ms repeated to 0.12 dB (median std, 0.26 worst)
/// across repetitions and preceding paths; a 50 ms window to 0.35-0.7 dB.
const PATH_AVERAGE: Duration = Duration::from_millis(700);
/// Attempts at a register write that stalls (`path-settle` saw 11 stalls
/// in 640 writes); every attempt is recorded.
const WRITE_ATTEMPTS: usize = 3;

/// A write retried while it stalls: (attempts, every attempt's errors);
/// the write stands when the last attempt returned none.
fn write_retried(rtl: &Rtl, regs: &[(u8, u8)]) -> (usize, Vec<String>) {
    let mut all = Vec::new();
    for attempt in 1..=WRITE_ATTEMPTS {
        let e = tuner::write_regs(rtl, regs);
        if e.is_empty() {
            return (attempt, all);
        }
        all.extend(e.into_iter().map(|s| format!("attempt {attempt}: {s}")));
    }
    (WRITE_ATTEMPTS, all)
}

/// The meter's page-3 pair read back-to-back for `window`: (mean, sample
/// standard deviation, reads, failed reads, lowest, highest) in counts.
fn meter_average(rtl: &Rtl, meter: meter::Meter, window: Duration) -> J {
    let t0 = Instant::now();
    let (mut v, mut failed) = (Vec::new(), 0usize);
    while t0.elapsed() < window {
        match rtl.demod_read(3, 0x59, 4).ok().and_then(|b| {
            meter::Snapshot {
                agc: Some([b[0], b[1], b[2], b[3]]),
                ..Default::default()
            }
            .value(meter)
        }) {
            Some(m) => v.push(m),
            None => failed += 1,
        }
    }
    if v.is_empty() {
        return J::obj().with("n", 0).with("nf", failed);
    }
    let n = v.len() as f64;
    let mean = v.iter().sum::<f64>() / n;
    let sd = if v.len() > 1 {
        (v.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / (n - 1.0)).sqrt()
    } else {
        0.0
    };
    let r1 = |x: f64| (x * 10.0).round() / 10.0;
    J::obj()
        .with("mean", r1(mean))
        .with("sd", r1(sd))
        .with("n", v.len())
        .with("nf", failed)
        .with("lo", v.iter().cloned().fold(f64::INFINITY, f64::min))
        .with("hi", v.iter().cloned().fold(f64::NEG_INFINITY, f64::max))
}

/// The front-end path per grid frequency, every path compared (findings
/// step 4f2): the bypass, the 16 (RFFILT, RFMUX) at TF 0000/0000, the
/// bypass again; each written (retried while it stalls), [`PATH_SETTLE`],
/// then the meter averaged over [`PATH_AVERAGE`]. Resumable by frequency
/// (a frequency with its closing `fd` line is skipped).
pub fn band_paths(args: &[String]) -> Result<(), String> {
    let (o, _) = Opts::parse(args)?;
    let spec = o
        .meter
        .ok_or("band-paths needs --meter NAME LO HI up|down")?;
    usb::guard()?;
    let path = o.path("step4f2-band-paths.jsonl");
    let hdr = header("4f2-band-paths", "the front-end path per grid frequency: every (RFFILT, RFMUX) at TF 0000/0000, each reading a settled average", &o)
        .with("candidate_space", format!("frequency {} MHz to {} MHz in {} MHz steps (the last included), those within --rf-from {} MHz .. --rf-to {} MHz, x RFFILT 0..=3 x RFMUX 0..=3, TF_NCH = TF_LP = 0000", TUNER_MIN_HZ / 1e6, TUNER_MAX_HZ / 1e6, o.grid_mhz, o.rf_from_hz / 1e6, o.rf_to_hz / 1e6))
        .with("procedure", format!("per frequency: LO for the RF, PLL words written, the source re-levelled once at the bypass path (RFFILT 00, RFMUX 01, TF 0000/0000) and then fixed; then the bypass, the 16 paths in RFFILT-then-RFMUX order, the bypass again: per path R26 and R27 written (up to {WRITE_ATTEMPTS} attempts while a write stalls), {} ms, then page 3 0x59-0x5C read back-to-back for {} ms and averaged", PATH_SETTLE.as_millis(), PATH_AVERAGE.as_millis()))
        .with("meter_spec", spec.record())
        .with("line_legend", "k ref: the frequency's reference (f, pll, status, relevel, source); k pa: f Hz, i position in the frequency's sequence, rf RFFILT, mx RFMUX, w write attempts, mean/sd/lo/hi the meter in counts over n reads (nf failed), e errors; k fd: the frequency done");
    let mut out = Jsonl::open(path, hdr)?;
    let done: HashSet<u32> = out
        .lines()
        .iter()
        .filter(|l| l.starts_with("{\"k\":\"fd\""))
        .filter_map(|l| crate::bytescan::field(l, "f"))
        .collect();
    let mut freqs: Vec<f64> = Vec::new();
    let mut f = TUNER_MIN_HZ;
    while f < TUNER_MAX_HZ {
        freqs.push(f);
        f += o.grid_mhz * 1e6;
    }
    freqs.push(TUNER_MAX_HZ);
    freqs.retain(|&f| f >= o.rf_from_hz && f <= o.rf_to_hz && !done.contains(&(f.round() as u32)));
    if freqs.is_empty() {
        println!("band-paths: every grid frequency in the span is done");
        return Ok(());
    }
    let mut src = Source::open()?;
    let reference = Front {
        rfmux: 0b01,
        rffilt: 0b00,
        tf_nch: 0,
        tf_lp: 0,
        ..o.front()
    };
    let first = o.pll(o.lo_hz(freqs[0]))?;
    let (rtl, setup) = tuner::fresh(&o.demod(), &reference, &first)?;
    out.line(setup.with("kind", "setup").with("source", src.describe()))?;
    let start = Instant::now();
    let mut level = o.level;
    let bypass = (reference.rffilt, reference.rfmux);
    let sequence: Vec<(u8, u8)> = std::iter::once(bypass)
        .chain((0..4u8).flat_map(|rf| (0..4u8).map(move |mx| (rf, mx))))
        .chain(std::iter::once(bypass))
        .collect();
    for f_hz in freqs {
        source::check()?;
        let fk = f_hz.round() as u32;
        let lo = o.lo_hz(f_hz);
        let pll = match o.pll(lo) {
            Ok(p) => p,
            Err(e) => {
                out.line(
                    J::obj()
                        .with("k", "ref")
                        .with("f", fk)
                        .with("e", errs(vec![format!("pll: {e}")])),
                )?;
                continue;
            }
        };
        let (werr, status) = tuner::retune(&rtl, &pll);
        let img = tuner::image(&reference, &pll);
        let (_, ferr) = write_retried(&rtl, &[(26, img[26]), (27, img[27])]);
        std::thread::sleep(PATH_SETTLE);
        src.level(f_hz, level)?;
        let (r, adj) = meter::relevel(&mut src, &rtl, start, &spec, f_hz, &mut level, false)?;
        out.line(
            J::obj()
                .with("k", "ref")
                .with("f", fk)
                .with("pll", pll.record())
                .with("status", status.map(|s| tuner::status_record(&s)))
                .with("relevel", adj)
                .with("source", level.tone(f_hz).record())
                .with("m", r.median(spec.meter))
                .with("e", errs(werr.into_iter().chain(ferr).collect())),
        )?;
        for (i, &(rf, mx)) in sequence.iter().enumerate() {
            source::check()?;
            let img = tuner::image(
                &Front {
                    rffilt: rf,
                    rfmux: mx,
                    ..reference
                },
                &pll,
            );
            let (w, e) = write_retried(&rtl, &[(26, img[26]), (27, img[27])]);
            std::thread::sleep(PATH_SETTLE);
            let mut line = J::obj()
                .with("k", "pa")
                .with("f", fk)
                .with("i", i)
                .with("rf", rf)
                .with("mx", mx)
                .with("w", w);
            if let J::Obj(fields) = meter_average(&rtl, spec.meter, PATH_AVERAGE) {
                for (k, v) in fields {
                    line.set(&k, v);
                }
            }
            if !e.is_empty() {
                line.set("e", errs(e));
            }
            out.line(line)?;
        }
        out.line(J::obj().with("k", "fd").with("f", fk))?;
        println!(
            "{:.1} MHz done (source {:+} dB)",
            f_hz / 1e6,
            level.tone(f_hz).level_db()
        );
    }
    src.quit();
    Ok(())
}

// ---- filter ----

pub fn filter(args: &[String]) -> Result<(), String> {
    let (o, _) = Opts::parse(args)?;
    let spec = o.meter.ok_or("filter needs --meter NAME LO HI up|down")?;
    usb::guard()?;
    let step_hz = o.if_step_hz.unwrap_or(100e3);
    let path = o.path("step4-filter.jsonl");
    let hdr = header("4-filter", "the tuner's IF filter codes: for each FILT_CODE, FILT_BW and HPF code the tone stepped across the IF band", &o)
        .with("candidate_space", format!("stages: ref (FILT_CODE 0, FILT_BW 00, HPF 1111: the widest documented codes, also the repeatability check for code 0 of the next stage); FILT_CODE 0..=15 (BW 00, HPF 1111); FILT_BW 0..=3 (CODE 0, HPF 1111); HPF 0..=15 (CODE 0, BW 00); per setting the tone's IF from {} Hz to {} Hz in {} Hz steps; R30 bit 6 {}", step_hz, o.if_max_hz, step_hz, o.r30_b6.map(|b| b as u8 as i64).map(|v| v.to_string()).unwrap_or("the matrix's 1".into())))
        .with("procedure", format!("LO fixed at the LO for --freq; the tone at LO -/+ IF (the IF side per --lo-side); the source re-levelled once at the reference setting with the tone at the DDC's IF, then fixed for every curve, so every curve is comparable and the -3 dB corners are read against the ref curve (which carries the demodulator's own response); DDC {}: {}", if o.ddc_track { "track" } else { "fixed" }, if o.ddc_track { format!("pset_iffreq follows the tone, {} Hz below it (0 when the tone is lower), so the demodulator's channel filter is centred near the tone and the curve is the tuner's", DDC_TRACK_OFFSET_HZ) } else { "pset_iffreq stays at --if; a readback taken before the demodulator's channel filter keeps responding outside it".into() }))
        .with("meter_spec", spec.record())
        .with("line_legend", "k flt; st stage; c FILT_CODE; bw FILT_BW; h HPF; fi the tone's IF (Hz); rf the tone's RF; dd the DDC's IF (Hz); m the meter's median; md every named meter's median; r raw; e errors");
    let mut out = Jsonl::open(path, hdr)?;
    let mut src = Source::open()?;
    let lo = o.lo_hz(o.freq_hz);
    let pll = o.pll(lo)?;
    let widest = Front {
        filt_code: 0,
        filt_bw: 0,
        hpf: 15,
        ..o.front()
    };
    let (rtl, setup) = tuner::fresh(&o.demod(), &widest, &pll)?;
    out.line(setup.with("kind", "setup").with("source", src.describe()))?;
    let start = Instant::now();
    let n = o.snapshots.unwrap_or(1);
    let mut level = o.level;
    let tone_rf = |f_if: f64| if o.high_side { lo - f_if } else { lo + f_if };
    src.level(tone_rf(o.if_hz), level)?;
    let (r0, adj) = meter::relevel(
        &mut src,
        &rtl,
        start,
        &spec,
        tone_rf(o.if_hz),
        &mut level,
        false,
    )?;
    out.line(
        J::obj()
            .with("k", "relevel")
            .with("relevel", adj)
            .with("source", level.tone(tone_rf(o.if_hz)).record())
            .with("m", r0.median(spec.meter))
            .with("md", r0.medians()),
    )?;
    let mut settings: Vec<(&str, u8, u8, u8)> = vec![("ref", 0, 0, 15)];
    settings.extend((0..16u8).map(|c| ("filt_code", c, 0, 15)));
    settings.extend((0..4u8).map(|bw| ("filt_bw", 0, bw, 15)));
    settings.extend((0..16u8).map(|h| ("hpf", 0, 0, h)));
    let steps = ((o.if_max_hz - step_hz) / step_hz).round() as usize + 1;
    for (count, (stage, code, bw, hpf)) in settings.into_iter().enumerate() {
        source::check()?;
        if o.limit.is_some_and(|l| count >= l) {
            break;
        }
        let fr = Front {
            filt_code: code,
            filt_bw: bw,
            hpf,
            ..widest
        };
        let img = tuner::image(&fr, &pll);
        let werr = tuner::write_regs(&rtl, &[(10, img[10]), (11, img[11])]);
        std::thread::sleep(TUNER_SETTLE);
        for k in 0..steps {
            source::check()?;
            let f_if = step_hz * (k + 1) as f64;
            let rf = tone_rf(f_if);
            let mut e = werr.clone();
            let dd = if o.ddc_track {
                (f_if - DDC_TRACK_OFFSET_HZ).max(0.0)
            } else {
                o.if_hz
            };
            if o.ddc_track {
                e.extend(tuner::set_ddc(&rtl, dd, o.high_side));
            }
            if !source_in_range(rf) {
                out.line(
                    J::obj()
                        .with("k", "flt")
                        .with("st", stage)
                        .with("c", code)
                        .with("bw", bw)
                        .with("h", hpf)
                        .with("fi", f_if)
                        .with("rf", rf)
                        .with("skip", "outside the source's range"),
                )?;
                continue;
            }
            src.level(rf, level)?;
            let r = Reading::take_n(&rtl, start, false, spec.meter.page3_offset(), n);
            let mut line = J::obj()
                .with("k", "flt")
                .with("st", stage)
                .with("c", code)
                .with("bw", bw)
                .with("h", hpf)
                .with("fi", f_if)
                .with("rf", rf)
                .with("dd", dd)
                .with("m", r.median(spec.meter))
                .with("md", r.medians())
                .with("r", r.compact());
            if !e.is_empty() {
                line.set("e", errs(e));
            }
            out.line(line)?;
        }
        println!("{stage} code {code} bw {bw} hpf {hpf} done");
    }
    src.quit();
    Ok(())
}

// ---- if-choice ----

pub fn if_choice(args: &[String]) -> Result<(), String> {
    let (o, _) = Opts::parse(args)?;
    usb::guard()?;
    let step_hz = o.if_step_hz.unwrap_or(250e3);
    let path = o.path("step4-if-choice.jsonl");
    let hdr = header("4-if-choice", "the demodulator's DDC IF against the tuner's IF: which readback locates the tone, and whether the LO side and Spec_inv matter", &o)
        .with("candidate_space", format!("tone side in [below the LO, above the LO] x pset_iffreq IF in {:?} x Spec_inv 0/1 x the tone's IF (|RF - LO|) from {} Hz to {} Hz in {} Hz steps", tuner::IF_CANDIDATES_HZ.iter().map(|(n, f)| format!("{f} Hz ({n})")).collect::<Vec<_>>(), step_hz, o.if_max_hz, step_hz))
        .with("how_the_tone_is_found", "the DDC shifts the ADC's spectrum by pset_iffreq and the demodulator's channel filter (8 MHz, Table 7) passes what lands within about 4 MHz of it: a readback taken after that filter responds only while the tone's IF is within the band around the DDC's IF (and, if the side and Spec_inv matter, only on one side), so its response against the tone's IF locates the tone; a readback taken before the channel filter responds wherever the tuner's own IF filter passes the tone. The source stays fixed throughout; a tone-off reading opens the record")
        .with("line_legend", "k ifc; side below|above (the tone relative to the LO); pi pset_iffreq's IF (Hz); inv Spec_inv; fi the tone's IF (Hz); rf the tone's RF; m the medians of every named meter; mv the meters that moved vs the off reading; r raw; e errors");
    let mut out = Jsonl::open(path, hdr)?;
    let mut src = Source::open()?;
    let lo = o.lo_hz(o.freq_hz);
    let pll = o.pll(lo)?;
    let (rtl, setup) = tuner::fresh(&o.demod(), &o.front(), &pll)?;
    out.line(setup.with("kind", "setup").with("source", src.describe()))?;
    let start = Instant::now();
    let n = o.snapshots.unwrap_or(1);
    src.off()?;
    let off = Reading::take_n(&rtl, start, o.page3, None, n);
    out.line(
        J::obj()
            .with("k", "off")
            .with("m", off.medians())
            .with("r", off.full()),
    )?;
    let steps = ((o.if_max_hz - step_hz) / step_hz).round() as usize + 1;
    let mut count = 0usize;
    'sides: for (side, below) in [("below", true), ("above", false)] {
        for &(_, pi) in &tuner::IF_CANDIDATES_HZ {
            for inv in [false, true] {
                source::check()?;
                if o.limit.is_some_and(|l| count >= l) {
                    break 'sides;
                }
                count += 1;
                let derr = tuner::set_ddc(&rtl, pi, inv);
                for k in 0..steps {
                    source::check()?;
                    let f_if = step_hz * (k + 1) as f64;
                    let rf = if below { lo - f_if } else { lo + f_if };
                    if !source_in_range(rf) {
                        continue;
                    }
                    src.level(rf, o.level)?;
                    let r = Reading::take_n(&rtl, start, false, None, n);
                    let mut line = J::obj()
                        .with("k", "ifc")
                        .with("side", side)
                        .with("pi", pi)
                        .with("inv", inv as u8)
                        .with("fi", f_if)
                        .with("rf", rf)
                        .with("m", r.medians())
                        .with("mv", moved_names(&off, &r))
                        .with("r", r.compact());
                    if !derr.is_empty() {
                        line.set("e", errs(derr.clone()));
                    }
                    out.line(line)?;
                }
                println!("{side} the LO, DDC IF {pi} Hz, Spec_inv {} done", inv as u8);
            }
        }
    }
    src.quit();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meter::Meter;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn parses_options_and_positionals() {
        let (o, pos) = Opts::parse(&args("--freq 500e6 --div 2 --meter dagc_val 40 200 down --page3 --txvga 20 --backoff -12 --lo-side low 300e6")).unwrap();
        assert_eq!(o.freq_hz, 500e6);
        assert_eq!(o.div, Some(2));
        assert!(o.page3);
        assert!(!o.high_side);
        assert_eq!((o.level.txvga_db, o.level.backoff_db), (20, -12));
        assert_eq!(o.meter.unwrap().meter, Meter::Dagc);
        assert_eq!(pos, ["300e6"]);
        assert!(Opts::parse(&args("--bogus")).is_err());
        assert!(Opts::parse(&args("--div 9")).is_err());
    }

    #[test]
    fn lo_and_rf_invert() {
        let (o, _) = Opts::parse(&args("--if 4.57e6")).unwrap();
        let lo = o.lo_hz(300e6);
        assert_eq!(lo, 304.57e6);
        assert_eq!(o.rf_hz(lo), 300e6);
        let (o, _) = Opts::parse(&args("--lo-side low")).unwrap();
        assert_eq!(o.rf_hz(o.lo_hz(300e6)), 300e6);
    }

    #[test]
    fn divider_probe_covers_every_code() {
        let space = divider_probe_space();
        assert_eq!(space.len(), 4 + 4 * 6);
        assert!(space.contains(&(3, 1)));
        assert!(space.contains(&(7, 32)));
    }
}
