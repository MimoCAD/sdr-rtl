//! An RTL2832U + R820T-family receiver, written from the vendor
//! documents and the bench records under `../derivation` and from
//! nothing else.
//!
//! Every register value is cited where it is defined: a document table
//! and page ([R820T], [R820T2], [RTL2832U], as `derivation/documents.md`
//! names them), a bench step (`derivation/findings.md`), or a generated
//! table (`tables`, copied verbatim from `derivation/generated`). What
//! the bench has not established is refused with `Error::Unsupported`
//! and the reason, never guessed: rates outside the measured-good span,
//! the bias tee, and any LO no divider reaches.
//!
//! The raw-sample stream ([`Rtl::rx_stream`], `stream`) is the sequence
//! findings step 10 confirmed and minimised on the bench; this code
//! ran against the bench dongle in findings step 11
//! (`examples/hwcheck.rs`).
//!
//! Modules: `protocol` (the vendor requests' encodings and the register
//! addresses), `usb` (the request layer over nusb), `tables` (the
//! generated tables), `pll` (the synthesizer), `image` (the tuner's
//! register image), `demod` (the demodulator's bring-up, IF and rate),
//! `taps` (the demodulator's tap registers and our design), `gain`,
//! `band`, `filter`, `stream`, `eeprom` (the configuration EEPROM's
//! layout, image and access, findings 17).

pub mod band;
pub mod demod;
pub mod eeprom;
pub mod filter;
pub mod gain;
pub mod image;
pub mod pll;
pub mod protocol;
pub mod stream;
pub mod taps;
pub mod tier;
// The tables are generated copies (`derivation/generated`, "do not
// edit"); the band grid's enum and array are as large as the record.
#[allow(clippy::large_enum_variant, clippy::large_const_arrays)]
pub mod tables;
pub mod usb;

use std::time::Duration;

use nusb::transfer::{Bulk, In};
use nusb::{DeviceInfo, MaybeFuture};
use sdr_core::gain::{Stage, StageReading, nearest_level};
use sdr_core::usb_stream::BulkIqStream;
use sdr_core::{Backend, DeviceDesc, Error, Result, TunePolicy, TuneRequest, TuneResult, open_with_deadline};

pub use eeprom::{Eeprom, EepromConfig, EepromIdentity, EepromWriteReport, IdentityChanges};
pub use gain::Codes as GainCodes;
pub use pll::{Pll, PllState};
pub use stream::{DroppedBlocks, RtlStream};
pub use tier::{AccountedStream, Break, Finished, LosslessStream, RestrictedStream, StreamError, StreamReport, Tier, TieredStream};

use demod::ddc_if_hz;
use image::{Front, R5, R6, R7, R10, R11, R12, R16, R20, R21, R22, R26, R27};
use tables::pll::XTAL_HZ;
use usb::Link;

/// The backend name every error carries.
pub const BACKEND: &str = "rtl";

/// The USB identities a dongle of this kind enumerates with. 0bda:2832
/// is what the bench dongle enumerated as after every power cycle
/// (findings step 0); 0bda:2838 is the other dongle identity the harness
/// accepts (`examples/harness/usb.rs` ACCEPTED_IDS), listed there, not
/// observed by a bench step. The harness's third entry, ffff:ffff, is a
/// vendor-less identity any unprogrammed device may carry; its guard
/// requires exactly one attached device, a driver's listing does not,
/// so it is left out. The chip behind an identity is checked at open by
/// the tuner's read-only R0 = 0x96.
pub const KNOWN_IDS: [(u16, u16); 2] = [(0x0bda, 0x2832), (0x0bda, 0x2838)];

/// The tuner's documented frequency range, 42-1002 MHz ([R820T] Table
/// 1-2); the band table covers the same span (findings step 4f).
pub const RF_RANGE_HZ: (f64, f64) = (42.0e6, 1002.0e6);

/// Which tuner the dongle carries. The bench proved one thing about
/// it: the read-only R0 reads 0x96, the check byte both the R820T and
/// the R820T2 documents print ([R820T] p. 17; [R820T2] Table 1-2), and
/// neither document gives a way to tell the two apart. So one variant,
/// honestly named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RtlTuner {
    /// An R820T or R820T2: R0 = 0x96.
    R820TFamily,
}

/// The tuner as a reader sees it after the last PLL write.
#[derive(Clone, Debug, PartialEq)]
pub struct RtlTunerStatus {
    pub tuner: RtlTuner,
    /// R0 read 0x96.
    pub chip_id_ok: bool,
    pub pll: PllState,
    /// The RF the baseband is centred on: LO − IF (the wanted sideband is
    /// below the LO, findings step 4h).
    pub rf_hz: f64,
    /// The IF the LO sits above the RF by: the DDC's 3,569,993.6 Hz
    /// (`demod::ddc_if_hz`, the stream's pset_iffreq 0x381112).
    pub if_hz: f64,
    /// 28.8 MHz ([RTL2832U] p. 10; `tables::pll::XTAL_HZ`).
    pub xtal_hz: f64,
    /// R3 RF_INDICATOR[7:0] as last read (named, never described; it
    /// follows the tuner's own gain state in auto mode, findings step 4b).
    pub rf_indicator: u8,
    /// R1 and R4 as last read (unspecified).
    pub r1: u8,
    pub r4: u8,
    pub stages: Vec<StageReading>,
    pub tuner_agc: bool,
    pub demod_agc: bool,
    /// The IF filter's upper −3 dB corner at the current codes.
    pub if_filter_upper_3db_hz: Option<f64>,
    /// The front-end path in use (R26 / R27).
    pub rffilt: u8,
    pub rfmux: u8,
    pub tf_nch: u8,
    pub tf_lp: u8,
}

/// An open dongle.
pub struct Rtl {
    link: Link,
    label: String,
    front: Front,
    pll: Pll,
    /// R0-R4 as last read after a PLL write or a status request.
    status: [u8; 5],
    /// The resampler ratio written last (`set_rate`); a stream starts
    /// with it.
    ratio: u32,
}

fn unsupported(what: String) -> Error {
    Error::Unsupported { backend: BACKEND, what }
}

fn candidates() -> Vec<DeviceInfo> {
    match nusb::list_devices().wait() {
        Ok(list) => list.filter(|d| KNOWN_IDS.contains(&(d.vendor_id(), d.product_id()))).collect(),
        Err(_) => Vec::new(),
    }
}

fn label_of(d: &DeviceInfo) -> String {
    let product = d.product_string().unwrap_or("RTL2832U");
    match d.serial_number() {
        Some(s) => format!("{product} serial {s}"),
        None => format!("{product} (no serial)"),
    }
}

/// Every attached dongle, as `rtl:serial=<sn>` (or `rtl:<index>` when
/// the device reports no serial).
pub fn list() -> Vec<DeviceDesc> {
    candidates()
        .iter()
        .enumerate()
        .map(|(i, d)| DeviceDesc {
            spec: match d.serial_number() {
                Some(s) => format!("rtl:serial={s}"),
                None => format!("rtl:{i}"),
            },
            label: label_of(d),
            backend: Backend::Rtl,
        })
        .collect()
}

/// The device a spec remainder names: `""` the first found, `"<index>"`
/// the nth, `"serial=<sn>"` by serial.
fn select(rest: &str) -> Result<DeviceInfo> {
    let not_found = || Error::NotFound { spec: format!("rtl:{rest}") };
    let mut found = candidates();
    if rest.is_empty() {
        return if found.is_empty() { Err(not_found()) } else { Ok(found.remove(0)) };
    }
    if let Some(sn) = rest.strip_prefix("serial=") {
        return found.into_iter().find(|d| d.serial_number() == Some(sn)).ok_or_else(not_found);
    }
    let index: usize = rest.parse().map_err(|_| unsupported(format!("spec \"rtl:{rest}\": expected \"\", \"<index>\" or \"serial=<sn>\"")))?;
    if index < found.len() { Ok(found.remove(index)) } else { Err(not_found()) }
}

impl Rtl {
    /// Open a dongle by spec remainder and bring it to the bench's
    /// measured state: the demodulator's bring-up (`demod::setup`), the
    /// rate at 2.048 Msps (`demod::DEFAULT_OUTPUT_RATE_SPS`), the tuner
    /// image (`image`) with the bring-up front end and the PLL at the
    /// 300 MHz tune every table and the stream's tone tests were taken
    /// against (findings 4e, 10.2b), every write read back where the chip
    /// allows. Bounded by `sdr_core::open_with_deadline`.
    pub fn open(rest: &str) -> Result<Rtl> {
        let rest = rest.to_string();
        open_with_deadline(BACKEND, move || Rtl::open_now(&rest))
    }

    fn open_now(rest: &str) -> Result<Rtl> {
        let info = select(rest)?;
        let label = label_of(&info);
        let dev = info.open().wait().map_err(|e| Error::Usb { backend: BACKEND, context: "open", message: e.to_string() })?;
        let link = Link::new(dev);
        demod::setup(&link)?;
        let ratio = demod::ratio_for_output_rate(demod::DEFAULT_OUTPUT_RATE_SPS)?;
        demod::write_rsamp(&link, ratio)?;
        // The image the bench's records were taken with: the bring-up
        // front end at the 300 MHz tune (findings step 4e; the recorded
        // image in `tables::tuner_image`), the VGA under its code; the LO
        // above it by the stream's IF, as the step-10 tone tests placed it.
        let front = Front::BRING_UP;
        let pll = Pll::program(OPEN_RF_HZ + ddc_if_hz())?;
        let status = link.with_tuner(|l| {
            l.tuner_write_image(&front.image(&pll))?;
            std::thread::sleep(demod::PLL_SETTLE);
            l.tuner_status()
        })?;
        // The dongle must still answer after the image (findings step 4a:
        // the one write that stops it is refused by the request layer).
        link.sys_read8(protocol::DEMOD_CTL)?;
        Ok(Rtl { link, label, front, pll, status, ratio })
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn tuner_kind(&self) -> RtlTuner {
        RtlTuner::R820TFamily
    }

    /// The output sample rate: rsamp_ratio = floor(f_xtal / (4 · rate) ·
    /// 2^22) ([RTL2832U] 9.4 with findings 10.2a's rule, output = f_symbol
    /// / 4), written now and again by the stream's start sequence. Only
    /// 1.024-2.88 Msps is accepted, the span every fresh bench run
    /// delivered; 3.2 Msps (half the rate in 2 runs of 8) and f_symbol ≥
    /// 14.4 MHz (broken) are refused (`demod::ratio_for_output_rate`).
    ///
    /// **2.88 Msps corrupts the signal**: the samples carry a ~46° phase
    /// step every ~11,000-14,000 samples (~200 a second) for as long as
    /// the stream runs — every run measured, no sample lost (findings
    /// 16.5). A tone's phase is broken at each step; a decoder sees a
    /// symbol-timing and carrier glitch. 1.024-2.4 Msps show none after
    /// the first ~10 ms. Below 1.44 Msps the first ~5-10 ms after a start
    /// carry ~37° steps every 192-256 samples.
    ///
    /// Refused while a stream exists: changing the ratio of a running
    /// stream did not dependably take (findings 10.2a), so the rate is
    /// fixed from `rx_stream` until the stream is dropped. A stream made
    /// after a drop and a rate change runs at the new rate (findings
    /// step 11: 1.024, 2.4, 2.88 and 2.048 Msps in turn).
    pub fn set_rate(&mut self, sps: f64) -> Result<()> {
        if self.link.stream_open() {
            return Err(unsupported(format!(
                "rate {:.6} Msps: a stream exists; drop it first (a live rsamp_ratio change did not dependably take, findings 10.2a)",
                sps / 1e6
            )));
        }
        let ratio = demod::ratio_for_output_rate(sps)?;
        demod::write_rsamp(&self.link, ratio)?;
        self.ratio = ratio;
        Ok(())
    }

    /// The output rate the resampler register holds now: its f_symbol / 4.
    pub fn rate(&self) -> Result<f64> {
        Ok(demod::read_f_symbol(&self.link)? / demod::OUTPUT_DECIMATION)
    }

    /// Tune: the LO above the RF by the DDC's IF (high side, findings 4h;
    /// `demod::ddc_if_hz`, 3.570 MHz, findings 10.2b), the synthesizer by
    /// the documented formula with the measured divider rule (`pll`,
    /// /16 included, findings 10.5), the front-end path from the band
    /// table (`band`), the actual RF from the words written. Anything
    /// outside 42-1002 MHz is refused; inside it every RF is reachable.
    /// Allowed while streaming: the bench programmed the tuner after the
    /// start in every minimisation run (findings 10.4).
    pub fn set_freq(&mut self, req: &TuneRequest) -> Result<TuneResult> {
        let rf = match req.rf {
            TunePolicy::Manual(f) => f,
            TunePolicy::Auto | TunePolicy::Default => req.target_freq,
        };
        if let TunePolicy::Manual(dsp) = req.dsp
            && dsp != 0.0
        {
            return Err(unsupported(format!("no DSP tuning stage: a DSP offset of {dsp} Hz cannot be applied in the device")));
        }
        if !rf.is_finite() || rf < RF_RANGE_HZ.0 || rf > RF_RANGE_HZ.1 {
            return Err(unsupported(format!(
                "RF {:.6} MHz is outside the tuner's documented {}-{} MHz ([R820T] Table 1-2)",
                rf / 1e6,
                RF_RANGE_HZ.0 / 1e6,
                RF_RANGE_HZ.1 / 1e6
            )));
        }
        let pll = Pll::program(rf + ddc_if_hz())?;
        let path = band::path_for(rf).ok_or_else(|| unsupported("the band table is empty".into()))?;
        let mut front = self.front;
        front.rffilt = path.rffilt;
        front.rfmux = path.rfmux;
        front.tf_nch = path.tf_nch;
        front.tf_lp = path.tf_lp;
        // R26, R27 and the PLL words are above R15: written unverified
        // (findings step 4a: the read wraps after 16 registers).
        let status = self.link.with_tuner(|l| {
            l.tuner_write(R26, front.r26())?;
            l.tuner_write(R27, front.r27())?;
            for (reg, v) in [(R16, pll.r16()), (R20, pll.r20()), (R21, pll.r21()), (R22, pll.r22())] {
                l.tuner_write(reg, v)?;
            }
            std::thread::sleep(demod::PLL_SETTLE);
            l.tuner_status()
        })?;
        self.front = front;
        self.pll = pll;
        self.status = status;
        let actual_rf = pll.lo_hz() - ddc_if_hz();
        Ok(TuneResult {
            clipped_rf_freq: rf,
            target_rf_freq: rf,
            actual_rf_freq: actual_rf,
            target_dsp_freq: req.target_freq - actual_rf,
            actual_dsp_freq: 0.0,
        })
    }

    /// The RF the baseband is centred on, from the PLL words written.
    pub fn freq(&self) -> Result<f64> {
        Ok(self.pll.lo_hz() - ddc_if_hz())
    }

    /// The receiver's last good tuner state written whole again (the image
    /// of `front` at `pll`, as `open` writes it): after a change that
    /// failed part way, the hardware is made what this `Rtl` says it is.
    /// The state fields are set only when a change's writes all succeed,
    /// so they always hold the last good state.
    pub(crate) fn reassert(&mut self) -> Result<()> {
        let img = self.front.image(&self.pll);
        self.status = self.link.with_tuner(|l| {
            l.tuner_write_image(&img)?;
            std::thread::sleep(demod::PLL_SETTLE);
            l.tuner_status()
        })?;
        Ok(())
    }

    fn codes(&self) -> GainCodes {
        GainCodes { lna: self.front.lna_gain, mix: self.front.mix_gain, vga: self.front.vga_code }
    }

    /// R5, R7 and R12 for the codes, each verified by read-back (R5-R15
    /// read back exactly, findings step 4a).
    fn write_codes(&mut self, codes: GainCodes) -> Result<()> {
        let mut front = self.front;
        front.lna_gain = codes.lna.min(15);
        front.mix_gain = codes.mix.min(15);
        front.vga_code = codes.vga.min(15);
        self.write_front_regs(front, &[R5, R7, R12])
    }

    /// Write the given registers of a front end and verify the image's
    /// R5-R15 against the register file.
    fn write_front_regs(&mut self, front: Front, regs: &[u8]) -> Result<()> {
        let img = front.image(&self.pll);
        self.link.with_tuner(|l| {
            for &reg in regs {
                l.tuner_write(reg, img[reg as usize])?;
            }
            l.tuner_verify(&img)
        })?;
        self.front = front;
        Ok(())
    }

    /// One number over the three stages, front first (`gain`): the
    /// nearest rung of the ladder.
    pub fn set_gain(&mut self, db: f64) -> Result<()> {
        self.write_codes(gain::codes_for_db(db))
    }

    /// The sum of the three stages' dB above code 0 (`gain`).
    pub fn gain(&self) -> f64 {
        self.codes().total_db()
    }

    /// (LNA, mixer, VGA) in dB above each stage's code 0.
    pub fn gain_stages(&self) -> (f64, f64, f64) {
        let c = self.codes();
        (gain::code_db(Stage::Lna, c.lna), gain::code_db(Stage::Mixer, c.mix), gain::code_db(Stage::Vga, c.vga))
    }

    /// Each stage to the code whose measured dB is nearest the request.
    pub fn set_gain_stages(&mut self, lna_db: f64, mixer_db: f64, vga_db: f64) -> Result<()> {
        let level = |stage: Stage, db: f64| nearest_level(&gain::levels_db(stage).unwrap_or([0.0; gain::CODES]), db) as u8;
        self.write_codes(GainCodes { lna: level(Stage::Lna, lna_db), mix: level(Stage::Mixer, mixer_db), vga: level(Stage::Vga, vga_db) })
    }

    /// The dB of every level of a stage (`tables::gain`), None for a
    /// stage this tuner lacks.
    pub fn stage_levels_db(&self, stage: Stage) -> Option<Vec<f64>> {
        gain::levels_db(stage).map(|l| l.to_vec())
    }

    pub fn stage_readings(&self) -> Vec<StageReading> {
        self.codes().readings()
    }

    pub fn set_stage_level(&mut self, stage: Stage, level: usize) -> Result<()> {
        if gain::levels_db(stage).is_none() {
            return Err(unsupported(format!("no {} stage on this tuner (LNA, mixer and VGA only)", stage.label())));
        }
        if level >= gain::CODES {
            return Err(unsupported(format!("{} level {level}: the stage has {} levels", stage.label(), gain::CODES)));
        }
        self.write_codes(self.codes().with_level(stage, level as u8))
    }

    /// `tuner_agc`: the LNA and mixer gains from the tuner's own power
    /// detectors (LNA_GAIN_MODE = 0, MIXGAIN_MODE = 1, PDET1 and PDET3 on;
    /// [R820T2] Table 1-3). `demod_agc`: the VGA's gain from the VAGC
    /// pin (VGA_MODE = 1), which the demodulator's IF AGC loop drives
    /// through its AGC_IF pin on this board (findings step 4b); the loop
    /// itself stays enabled as at reset ([RTL2832U] Table 2), its
    /// output reaching the VGA only through VGA_MODE. R5, R6, R7 and R12,
    /// verified.
    pub fn set_agc(&mut self, tuner_agc: bool, demod_agc: bool) -> Result<()> {
        let mut front = self.front;
        front.lna_manual = !tuner_agc;
        front.mix_manual = !tuner_agc;
        front.pdet1_on = tuner_agc;
        front.pdet3_on = tuner_agc;
        front.vga_from_pin = demod_agc;
        self.write_front_regs(front, &[R5, R6, R7, R12])
    }

    pub fn tuner_agc(&self) -> bool {
        !self.front.lna_manual
    }

    pub fn demod_agc(&self) -> bool {
        self.front.vga_from_pin
    }

    /// The IF filter to the narrowest measured setting that passes the
    /// bandwidth about the IF (`filter`); wider than any corner is
    /// clamped to the widest. R10 and R11, verified.
    pub fn set_bandwidth(&mut self, hz: f64) -> Result<()> {
        if !hz.is_finite() || hz <= 0.0 {
            return Err(unsupported(format!("bandwidth {hz} Hz: a positive width is needed")));
        }
        let s = filter::setting_for_bandwidth(hz);
        let mut front = self.front;
        front.filt_code = s.filt_code;
        front.filt_bw = s.filt_bw;
        self.write_front_regs(front, &[R10, R11])
    }

    /// The IF filter's upper −3 dB corner at the current codes.
    pub fn bandwidth(&self) -> Option<f64> {
        filter::corner_of(self.front.filt_code, self.front.filt_bw).map(filter::bandwidth_of)
    }

    /// Refused. [RTL2832U] names GPIO4 as antenna power with "default 0 =
    /// off" (Tables 16-21), but the printed reset of GPO (0x18) has that
    /// bit at 1 — and the bench read GPO 0x18 at power-on (findings step
    /// 1A) — so the pin's polarity is a contradiction the documents do
    /// not resolve, and whether this board wires GPIO4 to the antenna
    /// port is unmeasured. A bench step with the pin metered decides it.
    pub fn set_bias_tee(&mut self, on: bool) -> Result<()> {
        let _ = on;
        Err(unsupported(format!(
            "bias tee: GPIO4 (GPO bit {}) is documented as antenna power with \"default 0 = off\", yet GPO's printed and \
             measured reset is 0x18 with the bit set (findings step 1A); the polarity and the board's wiring are \
             unmeasured",
            protocol::GPIO_ANTENNA_POWER.trailing_zeros()
        )))
    }

    /// R0-R4 read now, with the synthesizer's words and the front end.
    pub fn tuner_status(&mut self) -> Result<RtlTunerStatus> {
        self.status = self.link.with_tuner(Link::tuner_status)?;
        let s = self.status;
        Ok(RtlTunerStatus {
            tuner: self.tuner_kind(),
            chip_id_ok: s[0] == protocol::TUNER_R0_CHECK,
            pll: PllState::new(&self.pll, s[2]),
            rf_hz: self.pll.lo_hz() - ddc_if_hz(),
            if_hz: ddc_if_hz(),
            xtal_hz: XTAL_HZ,
            rf_indicator: s[3],
            r1: s[1],
            r4: s[4],
            stages: self.stage_readings(),
            tuner_agc: self.tuner_agc(),
            demod_agc: self.demod_agc(),
            if_filter_upper_3db_hz: filter::corner_of(self.front.filt_code, self.front.filt_bw),
            rffilt: self.front.rffilt,
            rfmux: self.front.rfmux,
            tf_nch: self.front.tf_nch,
            tf_lp: self.front.tf_lp,
        })
    }

    /// The stream the rate allows ([`tier::Tier::for_rate`]): a
    /// [`LosslessStream`] to 1.2 Msps, an [`AccountedStream`] to 1.8, a
    /// [`RestrictedStream`] above. The stream owns the receiver until
    /// `finish` hands it back, so nothing can reach the control pipe
    /// while samples flow except what the stream itself offers.
    /// `transfer_bytes` is rounded down to whole packets (sdr-core's engine;
    /// at least one); `queue_depth` transfers are kept in flight (at least
    /// two). Nothing is written until the first `recv_into` (or `start`).
    pub fn into_stream(self, transfer_bytes: usize, queue_depth: usize) -> std::result::Result<TieredStream, StreamError> {
        Ok(match Tier::for_rate(self.rate_set()) {
            Tier::Lossless => TieredStream::Lossless(LosslessStream::new(self, transfer_bytes, queue_depth)?),
            Tier::Accounted => TieredStream::Accounted(AccountedStream::new(self, transfer_bytes, queue_depth)?),
            Tier::Restricted => TieredStream::Restricted(RestrictedStream::new(self, transfer_bytes, queue_depth)?),
        })
    }

    /// A [`LosslessStream`]; refused above [`tier::LOSSLESS_MAX_SPS`].
    pub fn into_lossless(self, transfer_bytes: usize, queue_depth: usize) -> std::result::Result<LosslessStream, StreamError> {
        LosslessStream::new(self, transfer_bytes, queue_depth)
    }

    /// An [`AccountedStream`]; refused above [`tier::ACCOUNTED_MAX_SPS`].
    pub fn into_accounted(self, transfer_bytes: usize, queue_depth: usize) -> std::result::Result<AccountedStream, StreamError> {
        AccountedStream::new(self, transfer_bytes, queue_depth)
    }

    /// A [`RestrictedStream`], at any rate.
    pub fn into_restricted(self, transfer_bytes: usize, queue_depth: usize) -> std::result::Result<RestrictedStream, StreamError> {
        RestrictedStream::new(self, transfer_bytes, queue_depth)
    }

    /// The output rate the resampler ratio written last gives (no request).
    pub(crate) fn link_ref(&self) -> &Link {
        &self.link
    }

    pub(crate) fn rate_set(&self) -> f64 {
        demod::output_rate_of_ratio(self.ratio)
    }

    /// The raw stream under every kind, with our taps.
    pub(crate) fn open_stream(&mut self, transfer_bytes: usize, queue_depth: usize) -> Result<RtlStream> {
        self.open_stream_with_taps(transfer_bytes, queue_depth, taps::design_registers())
    }

    /// The raw stream, for bench programs (`--features bench`): every
    /// method, the lossy ones included. Over the bulk IN endpoint 0x81 of
    /// interface 0 (`stream::BULK_IN_ENDPOINT`), 8-bit unsigned I/Q, even
    /// byte I (`stream::WIRE_FORMAT`, findings 10.2b), at the rate set now,
    /// our own taps (`taps::design_registers`). One stream at a time.
    #[cfg(feature = "bench")]
    pub fn rx_stream(&mut self, transfer_bytes: usize, queue_depth: usize) -> Result<RtlStream> {
        self.open_stream(transfer_bytes, queue_depth)
    }

    /// [`Rtl::rx_stream`] with a given image of the 20 tap registers
    /// (page 1 0x1C-0x2F, `taps::pack`'s layout) in place of our design.
    #[cfg(feature = "bench")]
    pub fn rx_stream_with_taps(&mut self, transfer_bytes: usize, queue_depth: usize, tap_regs: [u8; taps::TAP_REGS]) -> Result<RtlStream> {
        self.open_stream_with_taps(transfer_bytes, queue_depth, tap_regs)
    }

    /// The interface is claimed with nusb's `detach_and_claim_interface`,
    /// which on Linux first detaches a kernel driver bound to it and
    /// elsewhere is a plain claim.
    fn open_stream_with_taps(&mut self, transfer_bytes: usize, queue_depth: usize, tap_regs: [u8; taps::TAP_REGS]) -> Result<RtlStream> {
        if !self.link.claim_stream() {
            return Err(unsupported("a stream of this receiver already exists; one at a time".into()));
        }
        let usb = |context: &'static str, e: nusb::Error| Error::Usb { backend: BACKEND, context, message: e.to_string() };
        let endpoint = self
            .link
            .device()
            .detach_and_claim_interface(stream::BULK_INTERFACE)
            .wait()
            .map_err(|e| usb("claim interface", e))
            .and_then(|iface| iface.endpoint::<Bulk, In>(stream::BULK_IN_ENDPOINT).map_err(|e| usb("open bulk endpoint", e)));
        let endpoint = match endpoint {
            Ok(ep) => ep,
            Err(e) => {
                self.link.release_stream();
                return Err(e);
            }
        };
        let rate = demod::output_rate_of_ratio(self.ratio);
        let inner = BulkIqStream::new(BACKEND, endpoint, stream::WIRE_FORMAT, transfer_bytes, queue_depth, rate);
        Ok(RtlStream::new(inner, self.link.clone(), self.ratio, tap_regs))
    }

    /// The 20 tap registers as they read now (page 1 0x1C-0x2F; after a
    /// power cycle, before any stream, the chip's power-on values).
    pub fn taps(&self) -> Result<[u8; taps::TAP_REGS]> {
        let v = self.link.demod_read(protocol::TAP_PAGE, protocol::TAP_FIRST, taps::TAP_REGS as u16)?;
        let mut r = [0u8; taps::TAP_REGS];
        r.copy_from_slice(&v[..taps::TAP_REGS]);
        Ok(r)
    }

    /// The configuration EEPROM, the whole part: all 256 offsets in
    /// 16-byte block 3 reads (findings 17), cut to 128 bytes when the
    /// upper half repeats the lower (`eeprom::part_size`). Sixteen control
    /// transfers: during a stream each costs samples (findings 16).
    pub fn read_eeprom(&self) -> Result<Eeprom> {
        Eeprom::from_read(&eeprom::read_raw(&self.link)?)
    }

    /// Write an image to the configuration EEPROM (findings 17; eeprom.md
    /// §8). Refused while a stream exists, and for an image
    /// `Eeprom::check_writable` refuses (a test record; a patch-list
    /// marker not explicitly allowed). The part is read once first and
    /// must be the image's size; only the 8-byte pages that
    /// differ are written, one block 3 Set each to slave 0xA0, each
    /// followed by `eeprom::EEPROM_WRITE_CYCLE` (20 ms, unmeasured: the
    /// part's write-cycle time is not in the documents); then the whole
    /// part is read back, and any byte that differs is an error. The new
    /// identity is served after the next power cycle: the firmware reads
    /// the EEPROM only at boot (eeprom.md §5).
    pub fn write_eeprom(&mut self, new: &Eeprom) -> Result<EepromWriteReport> {
        if self.link.stream_open() {
            return Err(unsupported("EEPROM write: a stream exists; drop it first".into()));
        }
        eeprom::write(&self.link, new)
    }

    /// The request layer, for bench programs (`--features bench`).
    #[cfg(feature = "bench")]
    pub fn link(&self) -> &Link {
        &self.link
    }
}

/// How long the request layer waits on one control transfer.
pub const CONTROL_TIMEOUT: Duration = usb::CONTROL_TIMEOUT;

/// The RF the receiver opens tuned to: 300 MHz, where findings 4e
/// received the tone and the step-10 tone tests centred (findings 10.2b,
/// `TONE_RF_HZ` of `examples/harness/stream_confirm.rs`).
pub const OPEN_RF_HZ: f64 = 300.0e6;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_whole_rf_range_programs() {
        // The documented range's ends program, and the former gap does
        // with /16 (findings 10.5).
        assert!(Pll::program(RF_RANGE_HZ.0 + ddc_if_hz()).is_ok());
        assert!(Pll::program(RF_RANGE_HZ.1 + ddc_if_hz()).is_ok());
        assert_eq!(Pll::program(150e6 + ddc_if_hz()).unwrap().divider, 16);
    }

    /// The opening tune is the step-10 tone tests': 300 MHz, LO at
    /// 300 MHz + the stream's IF under /8 (findings 10.2b, `lo_for_rf`).
    #[test]
    fn the_opening_tune_is_the_benched_one() {
        let pll = Pll::program(OPEN_RF_HZ + ddc_if_hz()).unwrap();
        assert_eq!(pll.divider, 8);
        assert!((pll.lo_hz() - ddc_if_hz() - OPEN_RF_HZ).abs() <= pll::sdm_step_hz(8) / 2.0);
    }

    #[test]
    fn known_ids_are_the_dongles() {
        assert!(KNOWN_IDS.contains(&(0x0bda, 0x2832)));
        assert!(!KNOWN_IDS.contains(&(0xffff, 0xffff)));
    }
}
