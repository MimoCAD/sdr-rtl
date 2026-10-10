//! The step 4 records (derivation/data/step4-*.json, .jsonl) read into
//! typed structures. Each loader names the fields it takes and the line
//! legend it follows (the record's own `line_legend` / the harness's
//! writer in examples/harness/step4.rs); a field that is missing or of
//! another type is an error, never a default.

use std::path::Path;

use crate::value::Value;

/// Where a record came from, for the generated files' headers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance {
    /// The file name under derivation/data.
    pub file: String,
    /// The record header's `harness.crates_rtl_head`.
    pub harness_head: String,
    /// The record header's `harness.harness_uncommitted`.
    pub harness_uncommitted: bool,
    /// The record header's `step`.
    pub step: String,
}

impl Provenance {
    fn from_header(file: &str, header: &Value) -> Result<Provenance, String> {
        let h = header.field("harness")?;
        Ok(Provenance {
            file: file.to_string(),
            harness_head: h.field_str("crates_rtl_head")?.to_string(),
            harness_uncommitted: h.field_bool("harness_uncommitted")?,
            step: header.field_str("step")?.to_string(),
        })
    }

    /// "derivation/data/FILE (harness HASH[, uncommitted changes])".
    pub fn cite(&self) -> String {
        let dirty = if self.harness_uncommitted { ", uncommitted changes" } else { "" };
        format!("derivation/data/{} (harness {}{dirty})", self.file, self.harness_head)
    }
}

fn read(dir: &Path, file: &str) -> Result<String, String> {
    std::fs::read_to_string(dir.join(file)).map_err(|e| format!("{}: {e}", dir.join(file).display()))
}

fn ctx<T>(r: Result<T, String>, file: &str, line: usize) -> Result<T, String> {
    r.map_err(|e| format!("{file} line {line}: {e}"))
}

/// A one-document JSON record.
pub fn load_json(dir: &Path, file: &str) -> Result<(Value, Provenance), String> {
    let text = read(dir, file)?;
    let v = Value::parse(&text).map_err(|e| format!("{file}: {e}"))?;
    if let Some(a) = v.get("aborted") {
        return Err(format!("{file}: the record is marked aborted: {}", a.as_str().unwrap_or("?")));
    }
    let p = Provenance::from_header(file, &v)?;
    Ok((v, p))
}

/// A JSONL record: the header line, then every other line parsed.
pub fn load_jsonl(dir: &Path, file: &str) -> Result<(Value, Vec<Value>, Provenance), String> {
    let text = read(dir, file)?;
    let mut lines = text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty());
    let (_, first) = lines.next().ok_or_else(|| format!("{file}: empty"))?;
    let header = Value::parse(first).map_err(|e| format!("{file} line 1: {e}"))?;
    if header.get("kind").and_then(Value::as_str) != Some("header") {
        return Err(format!("{file}: the first line is not the header"));
    }
    let p = Provenance::from_header(file, &header)?;
    let mut out = Vec::new();
    for (n, l) in lines {
        out.push(Value::parse(l).map_err(|e| format!("{file} line {}: {e}", n + 1))?);
    }
    Ok((header, out, p))
}

/// The meter every step 4 sweep reads: `if_agc_val` decoded MSB-first
/// (findings.md step 4b; the medians object's `if_agc_val_msb_first`).
pub const METER_KEY: &str = "if_agc_val_msb_first";

fn meter_of(medians: &Value) -> Result<i64, String> {
    medians.field_i64(METER_KEY)
}

// ---- pll-sweep ----

/// One hypothesis's tone placement in a `pll` line.
#[derive(Clone, Debug, PartialEq)]
pub struct Hypothesis {
    pub div: u32,
    pub lo_hz: f64,
    pub rf_hz: f64,
    /// The meter with the tone on; None when the tone was outside the
    /// source's range ("skip").
    pub reading: Option<i64>,
}

/// One `pll` line: a (SEL_DIV, Nint) setting.
#[derive(Clone, Debug, PartialEq)]
pub struct PllLine {
    pub sel_div: u8,
    pub nint: u16,
    /// R2 VCO_INDICATOR[6:0] at 10 ms and at 60 ms after the write.
    pub r2_at_10ms: u8,
    pub r2_at_60ms: u8,
    pub off_reading: i64,
    pub on: Vec<Hypothesis>,
}

#[derive(Clone, Debug)]
pub struct PllSweep {
    pub provenance: Provenance,
    pub ref_div2: bool,
    pub f_ref_hz: f64,
    pub if_hz: f64,
    pub lines: Vec<PllLine>,
}

pub fn load_pll_sweep(dir: &Path) -> Result<PllSweep, String> {
    let file = "step4-pll-sweep.jsonl";
    let (header, lines, provenance) = load_jsonl(dir, file)?;
    let options = header.field("options")?;
    let ref_div2 = options.field_bool("ref_div2")?;
    let if_hz = options.field_f64("if_hz")?;
    let setup = lines.iter().find(|l| l.get("kind").and_then(Value::as_str) == Some("setup")).ok_or("pll-sweep: no setup line")?;
    let f_ref_hz = setup.field("pll")?.field_f64("f_ref_hz")?;
    let mut out = Vec::new();
    for (n, l) in lines.iter().enumerate() {
        if l.get("k").and_then(Value::as_str) != Some("pll") {
            continue;
        }
        let line = ctx(pll_line(l), file, n + 2)?;
        out.push(line);
    }
    Ok(PllSweep { provenance, ref_div2, f_ref_hz, if_hz, lines: out })
}

const R2_INDICATOR_MASK: u8 = 0x7F;

fn status_r2(v: &Value) -> Result<u8, String> {
    let bytes = v.as_arr().ok_or("status: array expected")?;
    let r2 = bytes.get(2).and_then(Value::as_i64).ok_or("status: R2 missing")?;
    Ok((r2 as u8) & R2_INDICATOR_MASK)
}

fn pll_line(l: &Value) -> Result<PllLine, String> {
    let mut on = Vec::new();
    for h in l.field_arr("on")? {
        on.push(Hypothesis {
            div: h.field_i64("div")? as u32,
            lo_hz: h.field_f64("lo")?,
            rf_hz: h.field_f64("rf")?,
            reading: if h.get("skip").is_some() { None } else { Some(meter_of(h.field("m")?)?) },
        });
    }
    Ok(PllLine {
        sel_div: l.field_i64("sd")? as u8,
        nint: l.field_i64("n")? as u16,
        r2_at_10ms: status_r2(l.field("s0")?)?,
        r2_at_60ms: status_r2(l.field("s1")?)?,
        off_reading: meter_of(l.field("off")?)?,
        on,
    })
}

// ---- lo-find ----

#[derive(Clone, Debug)]
pub struct LoFind {
    pub provenance: Provenance,
    pub sel_div: u8,
    pub nint: u16,
    pub vco_hz: f64,
    pub if_hz: f64,
    pub rf_step_hz: f64,
    pub off_reading: i64,
    /// (the tone's RF in Hz, the meter), in sweep order.
    pub points: Vec<(f64, i64)>,
}

// ---- vco-edge (step 14) ----

/// One step 14 line: the synthesizer under one divider at one RF, R2 and
/// the tone through the raw-sample stream (`examples/vcoedge.rs`).
#[derive(Clone, Debug, PartialEq)]
pub struct VcoEdgeLine {
    pub rf_hz: f64,
    pub divider: u32,
    pub vco_hz: f64,
    /// R2 VCO_INDICATOR[6:0], 20 ms after the write.
    pub r2: u8,
    /// The tone's bin over the median bin, dB.
    pub tone_db: f64,
}

#[derive(Clone, Debug)]
pub struct VcoEdge {
    pub provenance: Provenance,
    pub lines: Vec<VcoEdgeLine>,
}

pub const VCO_EDGE_RECORD: &str = "step14-vco-edge.jsonl";

pub fn load_vco_edge(dir: &Path) -> Result<VcoEdge, String> {
    let file = VCO_EDGE_RECORD;
    let (_, lines, provenance) = load_jsonl(dir, file)?;
    let mut out = Vec::new();
    for (n, l) in lines.iter().enumerate() {
        let line = n + 2;
        out.push(VcoEdgeLine {
            rf_hz: ctx(l.field_f64("rf"), file, line)?,
            divider: ctx(l.field_i64("divider"), file, line)? as u32,
            vco_hz: ctx(l.field_f64("vco_hz"), file, line)?,
            r2: ctx(l.field_i64("r2"), file, line)? as u8,
            tone_db: ctx(l.field_f64("tone_over_median_db"), file, line)?,
        });
    }
    Ok(VcoEdge { provenance, lines: out })
}

pub fn load_lo_find(dir: &Path, sel_div: u8) -> Result<LoFind, String> {
    let file = format!("step4-lo-find-sel{sel_div:03b}.jsonl");
    let (header, lines, provenance) = load_jsonl(dir, &file)?;
    let options = header.field("options")?;
    if options.field_i64("div")? as u8 != sel_div {
        return Err(format!("{file}: options.div is not {sel_div}"));
    }
    let if_hz = options.field_f64("if_hz")?;
    let setup = lines.iter().find(|l| l.get("kind").and_then(Value::as_str) == Some("setup")).ok_or("lo-find: no setup line")?;
    let pll = setup.field("pll")?;
    let nint = pll.field_i64("nint")? as u16;
    let vco_hz = pll.field_f64("vco_hz_if_f_vco_is_f_ref_x_ndiv")?;
    let mut off_reading = None;
    let mut points = Vec::new();
    for (n, l) in lines.iter().enumerate() {
        match l.get("k").and_then(Value::as_str) {
            Some("off") => off_reading = Some(ctx(meter_of(l.field("m")?), &file, n + 2)?),
            Some("lof") => points.push(ctx((|| Ok::<_, String>((l.field_f64("rf")?, meter_of(l.field("m")?)?)))(), &file, n + 2)?),
            _ => {}
        }
    }
    let rf_step_hz = points.windows(2).map(|w| w[1].0 - w[0].0).fold(f64::INFINITY, f64::min);
    Ok(LoFind { provenance, sel_div, nint, vco_hz, if_hz, rf_step_hz, off_reading: off_reading.ok_or("lo-find: no off line")?, points })
}

// ---- gain-ladder ----

#[derive(Clone, Debug, PartialEq)]
pub struct GainCodeReading {
    pub code: u8,
    /// Comparator: the source level at which `if_agc_val` flipped; None
    /// with the search's stop reason.
    pub threshold_level_db: Option<i32>,
    pub stop: Option<String>,
    /// Comparator: the lowest and highest source level the search probed.
    pub probed_levels_db: Option<(i32, i32)>,
    /// Loop meter: the meter and the source level it was read at.
    pub meter_value: Option<i64>,
    pub source_level_db: Option<i32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GainStage {
    pub name: String,
    pub register: u8,
    pub method: String,
    pub comparator: bool,
    pub codes: Vec<GainCodeReading>,
}

#[derive(Clone, Debug)]
pub struct GainLadder {
    pub provenance: Provenance,
    pub config: String,
    pub freq_hz: f64,
    pub fixed_codes: (u8, u8, u8),
    pub stages: Vec<GainStage>,
}

pub fn load_gain_ladder(dir: &Path, file: &str) -> Result<GainLadder, String> {
    let (v, provenance) = load_json(dir, file)?;
    let options = v.field("options")?;
    let fixed = options.field("gain_codes_fixed")?;
    let mut stages = Vec::new();
    for s in v.field_arr("stages")? {
        let method = s.field_str("method")?.to_string();
        let comparator = method.starts_with("comparator");
        let mut codes = Vec::new();
        for c in s.field_arr("codes")? {
            let errors = c.field_arr("write_errors")?;
            if !errors.is_empty() {
                return Err(format!("{file}: write errors at code {}", c.field_i64("code")?));
            }
            let probed: Vec<i64> = c.path(&["search", "readings"]).and_then(Value::as_arr).map(|r| r.iter().filter_map(|x| x.get("level_db").and_then(Value::as_i64)).collect()).unwrap_or_default();
            codes.push(GainCodeReading {
                code: c.field_i64("code")? as u8,
                threshold_level_db: c.get("threshold_level_db").and_then(Value::as_i64).map(|t| t as i32),
                stop: c.path(&["search", "stop"]).and_then(Value::as_str).map(str::to_string),
                probed_levels_db: probed.iter().min().zip(probed.iter().max()).map(|(a, b)| (*a as i32, *b as i32)),
                meter_value: c.get("meter_value").and_then(Value::as_i64),
                source_level_db: c.path(&["source", "level_db_relative"]).and_then(Value::as_i64).map(|t| t as i32),
            });
        }
        stages.push(GainStage { name: s.field_str("stage")?.to_string(), register: s.field_i64("register")? as u8, method, comparator, codes });
    }
    Ok(GainLadder {
        provenance,
        config: options.field_str("config")?.to_string(),
        freq_hz: options.field_f64("freq_hz")?,
        fixed_codes: (fixed.field_i64("lna")? as u8, fixed.field_i64("mix")? as u8, fixed.field_i64("vga")? as u8),
        stages,
    })
}

// ---- meter ----

#[derive(Clone, Debug)]
pub struct MeterLadder {
    pub config: String,
    pub polar: bool,
    /// (the source's relative level in dB, the meter), tone-on states only.
    pub states: Vec<(i32, i64)>,
    /// The meter with the tone off (the ladder's first state).
    pub off_reading: i64,
}

#[derive(Clone, Debug)]
pub struct Meter {
    pub provenance: Provenance,
    pub ladders: Vec<MeterLadder>,
}

pub fn load_meter(dir: &Path) -> Result<Meter, String> {
    let file = "step4-meter.json";
    let (v, provenance) = load_json(dir, file)?;
    let mut ladders = Vec::new();
    for l in v.field_arr("ladders")? {
        let mut states = Vec::new();
        let mut off = None;
        for s in l.field_arr("states")? {
            let m = meter_of(s.field("medians")?)?;
            match s.path(&["source", "level_db_relative"]).and_then(Value::as_i64) {
                Some(level) => states.push((level as i32, m)),
                None => {
                    if off.is_none() {
                        off = Some(m);
                    }
                }
            }
        }
        ladders.push(MeterLadder { config: l.field_str("config")?.to_string(), polar: l.field_bool("polar_if_agc_and_rf_agc")?, states, off_reading: off.ok_or("meter: a ladder without an off state")? });
    }
    Ok(Meter { provenance, ladders })
}

// ---- band-table ----

#[derive(Clone, Debug, PartialEq)]
pub struct BandReading {
    pub rffilt: u8,
    pub rfmux: u8,
    pub tf_nch: u8,
    pub tf_lp: u8,
    pub reading: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BandRef {
    pub reading: i64,
    pub source_level_db: i32,
    pub sel_div: u8,
    pub nint: u16,
    pub r2: u8,
}

/// One grid frequency's lines, in file order.
#[derive(Clone, Debug, PartialEq)]
pub struct BandFreq {
    pub rf_hz: u32,
    pub reference: Option<BandRef>,
    /// The `ref` line's error when the frequency could not be tuned.
    pub error: Option<String>,
    pub stage_a_best: Vec<(u8, u8)>,
    pub readings: Vec<BandReading>,
}

#[derive(Clone, Debug)]
pub struct BandTable {
    pub provenance: Provenance,
    pub if_hz: f64,
    pub grid_step_hz: f64,
    pub staged: bool,
    pub freqs: Vec<BandFreq>,
}

pub fn load_band_table(dir: &Path) -> Result<BandTable, String> {
    let file = "step4-band-table.jsonl";
    let (header, lines, provenance) = load_jsonl(dir, file)?;
    let options = header.field("options")?;
    let staged = header.field_str("search")?.starts_with("staged");
    let mut freqs: Vec<BandFreq> = Vec::new();
    let at = |freqs: &mut Vec<BandFreq>, f: u32| -> usize {
        match freqs.iter().position(|x| x.rf_hz == f) {
            Some(i) => i,
            None => {
                freqs.push(BandFreq { rf_hz: f, reference: None, error: None, stage_a_best: Vec::new(), readings: Vec::new() });
                freqs.len() - 1
            }
        }
    };
    for (n, l) in lines.iter().enumerate() {
        let kind = l.get("k").and_then(Value::as_str);
        let r: Result<(), String> = (|| {
            match kind {
                Some("ref") => {
                    let i = at(&mut freqs, l.field_i64("f")? as u32);
                    let errors = l.field_arr("e")?;
                    if let Some(pll) = l.get("pll") {
                        if !errors.is_empty() {
                            return Err(format!("ref line with errors: {errors:?}"));
                        }
                        freqs[i].reference = Some(BandRef {
                            reading: l.field_i64("m")?,
                            source_level_db: l.field("source")?.field_i64("level_db_relative")? as i32,
                            sel_div: pll.field_i64("sel_div")? as u8,
                            nint: pll.field_i64("nint")? as u16,
                            r2: l.field("status")?.field_i64("r2_vco_indicator")? as u8,
                        });
                    } else {
                        freqs[i].error = Some(errors.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; "));
                    }
                }
                Some("stage_a") => {
                    let i = at(&mut freqs, l.field_i64("f")? as u32);
                    for p in l.field_arr("best_paths_rffilt_rfmux")? {
                        let p = p.as_arr().ok_or("best path: array expected")?;
                        freqs[i].stage_a_best.push((p[0].as_i64().ok_or("rffilt")? as u8, p[1].as_i64().ok_or("rfmux")? as u8));
                    }
                }
                Some("bt") => {
                    if l.get("e").is_some() {
                        return Err("bt line with errors".into());
                    }
                    let i = at(&mut freqs, l.field_i64("f")? as u32);
                    freqs[i].readings.push(BandReading {
                        rffilt: l.field_i64("rf")? as u8,
                        rfmux: l.field_i64("mx")? as u8,
                        tf_nch: l.field_i64("n")? as u8,
                        tf_lp: l.field_i64("l")? as u8,
                        reading: l.field_i64("m")?,
                    });
                }
                _ => {}
            }
            Ok(())
        })();
        ctx(r, file, n + 2)?;
    }
    freqs.sort_by_key(|f| f.rf_hz);
    let grid_step_hz = freqs.windows(2).map(|w| (w[1].rf_hz - w[0].rf_hz) as f64).fold(f64::INFINITY, f64::min);
    Ok(BandTable { provenance, if_hz: options.field_f64("if_hz")?, grid_step_hz, staged, freqs })
}

// ---- filter ----

#[derive(Clone, Debug, PartialEq)]
pub struct FilterCurve {
    pub stage: String,
    pub filt_code: u8,
    pub filt_bw: u8,
    pub hpf: u8,
    /// (the tone's IF in Hz, the meter), ascending in IF.
    pub points: Vec<(f64, i64)>,
}

#[derive(Clone, Debug)]
pub struct Filter {
    pub provenance: Provenance,
    pub ddc_track: bool,
    pub if_step_hz: f64,
    pub curves: Vec<FilterCurve>,
}

pub fn load_filter(dir: &Path) -> Result<Filter, String> {
    let file = "step4-filter.jsonl";
    let (header, lines, provenance) = load_jsonl(dir, file)?;
    let ddc_track = header.field_str("procedure")?.contains("DDC track");
    let mut curves: Vec<FilterCurve> = Vec::new();
    for (n, l) in lines.iter().enumerate() {
        if l.get("k").and_then(Value::as_str) != Some("flt") {
            continue;
        }
        let r: Result<(), String> = (|| {
            if l.get("skip").is_some() {
                return Err("a skipped point (outside the source's range)".into());
            }
            if l.get("e").is_some() {
                return Err("flt line with errors".into());
            }
            let key = (l.field_str("st")?.to_string(), l.field_i64("c")? as u8, l.field_i64("bw")? as u8, l.field_i64("h")? as u8);
            let point = (l.field_f64("fi")?, l.field_i64("m")?);
            match curves.iter_mut().find(|c| (c.stage.as_str(), c.filt_code, c.filt_bw, c.hpf) == (key.0.as_str(), key.1, key.2, key.3)) {
                Some(c) => c.points.push(point),
                None => curves.push(FilterCurve { stage: key.0, filt_code: key.1, filt_bw: key.2, hpf: key.3, points: vec![point] }),
            }
            Ok(())
        })();
        ctx(r, file, n + 2)?;
    }
    let mut if_step_hz = f64::INFINITY;
    for c in &mut curves {
        c.points.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        for w in c.points.windows(2) {
            if_step_hz = if_step_hz.min(w[1].0 - w[0].0);
        }
    }
    Ok(Filter { provenance, ddc_track, if_step_hz, curves })
}

// ---- tune (the register image) ----

#[derive(Clone, Debug, PartialEq)]
pub struct RegisterChoice {
    pub reg: u8,
    pub value: u8,
    pub why: String,
}

#[derive(Clone, Debug)]
pub struct TuneRecord {
    pub provenance: Provenance,
    pub freq_hz: f64,
    pub config: String,
    /// The `front` the image was built from, as (field, value) pairs in
    /// record order.
    pub front: Vec<(String, String)>,
    pub pll: Vec<(String, String)>,
    pub image_source: String,
    pub fixed_cell_violations: usize,
    pub registers: Vec<RegisterChoice>,
    pub write_errors: usize,
    /// R5..R15 that read back differently from what was written.
    pub readback_differs_at: Vec<u8>,
}

fn hex_byte(s: &str) -> Result<u8, String> {
    s.strip_prefix("0x").and_then(|h| u8::from_str_radix(h, 16).ok()).ok_or_else(|| format!("{s}: 0xNN expected"))
}

fn scalar_pairs(v: &Value) -> Vec<(String, String)> {
    match v {
        Value::Obj(fields) => fields
            .iter()
            .map(|(k, v)| {
                let s = match v {
                    Value::Null => "null".to_string(),
                    Value::Bool(b) => b.to_string(),
                    Value::Int(i) => i.to_string(),
                    Value::Num(x) => x.to_string(),
                    Value::Str(s) => s.clone(),
                    _ => "…".to_string(),
                };
                (k.clone(), s)
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub fn load_tune(dir: &Path, file: &str) -> Result<TuneRecord, String> {
    let (v, provenance) = load_json(dir, file)?;
    let options = v.field("options")?;
    let setup = v.field("setup")?;
    let image = setup.field("tuner_image")?;
    let applied = setup.field("tuner_applied")?;
    let bytes: Vec<u8> = applied.field_arr("image")?.iter().map(|b| b.as_str().ok_or("image byte: string expected".to_string()).and_then(hex_byte)).collect::<Result<_, _>>()?;
    let first_reg = 5u8;
    let mut registers = Vec::new();
    for r in image.field_arr("registers")? {
        let reg = r.field_i64("reg")? as u8;
        let value = hex_byte(r.field_str("value")?)?;
        let from_applied = *bytes.get((reg - first_reg) as usize).ok_or_else(|| format!("R{reg}: not in the applied image"))?;
        if from_applied != value {
            return Err(format!("{file}: R{reg} differs between tuner_image (0x{value:02X}) and tuner_applied (0x{from_applied:02X})"));
        }
        registers.push(RegisterChoice { reg, value, why: r.field_str("why")?.to_string() });
    }
    if registers.len() != bytes.len() {
        return Err(format!("{file}: {} notes for {} image bytes", registers.len(), bytes.len()));
    }
    let check = applied.field("readback_check")?;
    Ok(TuneRecord {
        provenance,
        freq_hz: options.field_f64("freq_hz")?,
        config: options.field_str("config")?.to_string(),
        front: scalar_pairs(setup.field("front")?),
        pll: scalar_pairs(setup.field("pll")?),
        image_source: image.field_str("source")?.to_string(),
        fixed_cell_violations: image.field_arr("fixed_cell_violations")?.len(),
        registers,
        write_errors: applied.field_arr("write_errors")?.len(),
        readback_differs_at: check.field_arr("r5_to_r15_differ_at")?.iter().filter_map(Value::as_i64).map(|r| r as u8).collect(),
    })
}
