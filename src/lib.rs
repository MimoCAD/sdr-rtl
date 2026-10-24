//! An RTL2832U + R820T-family receiver, written from the vendor
//! documents and the bench records under `derivation/` in this crate's
//! repository, and from nothing else.
//!
//! Every register value is cited where it is defined: a document table
//! and page ([R820T], [R820T2], [RTL2832U], as `derivation/documents.md`
//! names them), a bench step (`derivation/findings.md`), or a generated
//! table (`tables`, copied verbatim from `derivation/generated`). What
//! the bench has not established is refused with `Error::Unsupported`
//! and the reason, never guessed: rates outside the measured-good span,
//! the bias tee, and any LO no divider reaches.
//!
//! The raw-sample stream (`stream`) is the sequence findings step 10
//! confirmed and minimised on the bench; this code ran against the bench
//! dongle in findings step 11 (`examples/hwcheck.rs`). A receiver streams
//! by becoming a [`Stream`] ([`Rtl::into_stream`], `session`): it reads as
//! [`std::io::Read`] (the wire bytes) or as [`sdr_core::Iq`] samples,
//! changes only inside a pause that loses nothing, splits into a
//! [`ReadHalf`] and a [`Control`] for two threads, and its `finish` hands
//! the receiver back with the chip's count of every sample lost
//! (findings 16).
//!
//! Modules: `protocol` (the vendor requests' encodings and the register
//! addresses), `usb` (the request layer over nusb), `tables` (the
//! generated tables), `pll` (the synthesizer), `image` (the tuner's
//! register image), `demod` (the demodulator's bring-up, IF and rate),
//! `taps` (the demodulator's tap registers and our design), `gain`,
//! `band`, `filter`, `stream` (the raw stream), `session` (the
//! [`Stream`]), `eeprom` (the configuration EEPROM's layout, image and
//! access, findings 17).

//!
//! `no_std` at its core (with `alloc`): the register encodings, the
//! tables, the synthesizer, gain, filter, taps and the EEPROM's layout are
//! computation only. The default `std` feature adds the receiver itself —
//! the USB request layer over nusb, [`Rtl`], the raw stream and
//! [`Stream`].

#![no_std]
// The docs cite documents as [RTL2832U] / [R820T2] and register bits as
// R5[3:0]; rustdoc reads each as a link it cannot resolve and shows the
// text as written.
#![allow(rustdoc::broken_intra_doc_links)]

extern crate alloc;
#[cfg(feature = "std")]
#[macro_use]
extern crate std;

/// A nusb operation completed: awaited in a browser (WebUSB is async),
/// waited on natively (nusb's native operations block, and awaiting them
/// would need an async runtime). So the driver's async code suspends only
/// in a browser; natively it completes in place.
#[cfg(feature = "std")]
macro_rules! complete {
    ($op:expr) => {{
        #[cfg(target_arch = "wasm32")]
        {
            $op.await
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            nusb::MaybeFuture::wait($op)
        }
    }};
}
#[cfg(feature = "std")]
pub(crate) use complete;

pub mod band;
#[cfg(all(feature = "std", not(target_arch = "wasm32")))]
pub mod blocking;
pub mod clockwatch;
#[cfg(feature = "std")]
pub mod decim;
pub mod demod;
pub mod eeprom;
#[cfg(all(
    feature = "std",
    any(test, feature = "mock"),
    not(target_arch = "wasm32")
))]
pub mod fake;
pub mod filter;
pub mod gain;
pub mod image;
mod math;
pub mod pll;
pub mod protocol;
#[cfg(feature = "std")]
pub mod session;
#[cfg(feature = "std")]
pub mod stream;
pub mod taps;
// The tables are generated copies (`derivation/generated`, "do not
// edit"); the band grid's enum and array are as large as the record.
#[allow(clippy::large_enum_variant, clippy::large_const_arrays)]
pub mod tables;
#[cfg(feature = "std")]
pub mod usb;

use alloc::vec::Vec;

use sdr_core::gain::StageReading;

pub use eeprom::{Eeprom, EepromChanges, EepromConfig, EepromIdentity, EepromWriteReport};
pub use gain::{Codes as GainCodes, Stage};
pub use pll::{Pll, PllState};
#[cfg(feature = "std")]
pub use session::{Break, Control, Finished, ReadHalf, Stream, StreamError, StreamReport};
#[cfg(feature = "std")]
pub use stream::{DroppedBlocks, RtlStream};

// The receiver itself: std (the USB transport, threads, time).
#[cfg(feature = "std")]
use alloc::string::{String, ToString};
#[cfg(feature = "std")]
use alloc::sync::Arc;
#[cfg(feature = "std")]
use core::time::Duration;
#[cfg(feature = "std")]
use demod::ddc_if_hz;
#[cfg(feature = "std")]
use image::{Front, ImrPoint, R5, R6, R7, R8, R9, R10, R11, R12, R16, R20, R21, R22, R26, R27};
#[cfg(feature = "std")]
use nusb::DeviceInfo;
#[cfg(feature = "std")]
use sdr_core::gain::nearest_level;
#[cfg(feature = "std")]
use sdr_core::usb_stream::BulkIqStream;
#[cfg(feature = "std")]
use sdr_core::{Backend, DeviceDesc, Error, Result, TunePolicy, TuneRequest, TuneResult};
#[cfg(feature = "std")]
use tables::pll::XTAL_HZ;
#[cfg(feature = "std")]
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

/// The RF range tuned: 42 MHz, the bottom of the tuner's documented
/// 42-1002 MHz ([R820T] Table 1-2), to 1700 MHz, measured (findings 26).
///
/// Above 1002 MHz, both bench dongles every 10 MHz to 1720 MHz: the
/// synthesizer locks at /2, the tone lands where the words put it and
/// reads 51-60 dB over the median bin, and the band table's dominant path
/// (RFFILT 10, RFMUX 01, which `band::path_for` gives beyond the grid) is
/// the strongest of all 16 at every step, by 1.1-1.8 dB. ADS-B's 1090 MHz
/// reads 58.8 dB over the median.
///
/// The top: at /2 the signal ends in a cliff, full strength at one MHz and
/// nothing the next — dongle A at 1724 → 1725 MHz (LO 1728.6), dongle B
/// past 1735 and gone by 1740 — while R2 still reads in range, so the
/// driver cannot tell. The VCO is not the limit (forced to /4 at the same
/// VCO, 3.45-3.67 GHz, the tone was received). 1700 MHz is 24 MHz under
/// the lower cliff, a margin chosen against units not measured. Below 42
/// MHz is not measured (the bench source starts at 70 MHz).
pub const RF_RANGE_HZ: (f64, f64) = (42.0e6, 1700.0e6);

/// The largest crystal correction [`Rtl::set_ppm`] takes, a choice: far
/// beyond a crystal's tolerance, and inside what keeps every rate plan's
/// ratio where the chip resamples as written (the nearest edge, 0.905
/// Msps's ratio under 2^25, is 5,560 ppm away; `demod::RSAMP_TOP_BIT`).
pub const PPM_LIMIT: f64 = 1000.0;

/// Bench only: an RF range in place of [`RF_RANGE_HZ`], to measure the
/// tuner outside it; its two bounds' f64 bits, 0 = none. The synthesizer's
/// reach (`pll`) and the lock check still apply.
#[cfg(feature = "bench")]
static BENCH_RF_RANGE_HZ: [core::sync::atomic::AtomicU64; 2] = [
    core::sync::atomic::AtomicU64::new(0),
    core::sync::atomic::AtomicU64::new(0),
];

/// Bench only: accept RFs in `range` (None: [`RF_RANGE_HZ`] again).
#[cfg(feature = "bench")]
pub fn set_bench_rf_range(range: Option<(f64, f64)>) {
    let (lo, hi) = range.map_or((0, 0), |(lo, hi)| (lo.to_bits(), hi.to_bits()));
    BENCH_RF_RANGE_HZ[0].store(lo, core::sync::atomic::Ordering::Relaxed);
    BENCH_RF_RANGE_HZ[1].store(hi, core::sync::atomic::Ordering::Relaxed);
}

/// The RF range accepted: [`RF_RANGE_HZ`] (or the bench's).
pub fn rf_range() -> (f64, f64) {
    #[cfg(feature = "bench")]
    {
        let lo = BENCH_RF_RANGE_HZ[0].load(core::sync::atomic::Ordering::Relaxed);
        let hi = BENCH_RF_RANGE_HZ[1].load(core::sync::atomic::Ordering::Relaxed);
        if hi != 0 {
            return (f64::from_bits(lo), f64::from_bits(hi));
        }
    }
    RF_RANGE_HZ
}

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
    /// The crystal as corrected: 28.8 MHz ([RTL2832U] p. 10;
    /// `tables::pll::XTAL_HZ`) × (1 + ppm / 10^6). `rf_hz` and `if_hz` are
    /// under it; `pll`'s LO and VCO are the words' at 28.8 MHz.
    pub xtal_hz: f64,
    /// The correction ([`Rtl::set_ppm`]).
    pub ppm: f64,
    /// R3 RF_INDICATOR[7:0] as last read (named, never described; it
    /// follows the tuner's own gain state in auto mode, findings step 4b).
    pub rf_indicator: u8,
    /// R1 and R4 as last read (unspecified).
    pub r1: u8,
    pub r4: u8,
    pub stages: Vec<StageReading<Stage>>,
    pub tuner_agc: bool,
    pub demod_agc: bool,
    /// The IF filter's upper −3 dB corner at the current codes.
    pub if_filter_upper_3db_hz: Option<f64>,
    /// The front-end path in use (R26 / R27).
    pub rffilt: u8,
    pub rfmux: u8,
    pub tf_nch: u8,
    pub tf_lp: u8,
    /// The image-rejection setting in use (R8 IMR_G, R9 IMR_P): the
    /// dongle's calibration at this RF, or 32/32.
    pub imr_g: u8,
    pub imr_p: u8,
}

/// An open dongle.
#[cfg(feature = "std")]
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
    /// The decimation the stream applies to the chip's samples (1: none;
    /// `demod::plan_for_output_rate`).
    decimation: u32,
    /// The output rate last asked for, so a correction change can set it
    /// again.
    rate_asked: f64,
    /// The RF last asked for (`set_freq`), so a correction change tunes it
    /// again rather than what the last words delivered.
    rf_asked: f64,
    /// The crystal correction, parts per million ([`Rtl::set_ppm`]).
    ppm: f64,
    /// The dongle's image-rejection calibration ([`Rtl::set_imr_table`]);
    /// empty: IMR_G = IMR_P = 32 everywhere.
    imr: Vec<ImrPoint>,
}

#[cfg(feature = "std")]
fn unsupported(what: String) -> Error {
    Error::Unsupported {
        backend: BACKEND,
        what,
    }
}

/// The attached dongles this process may open (in a browser, those the
/// page has been granted, `Rtl::request`).
#[cfg(feature = "std")]
async fn candidates() -> Result<Vec<DeviceInfo>> {
    match complete!(nusb::list_devices()) {
        Ok(list) => Ok(list
            .filter(|d| KNOWN_IDS.contains(&(d.vendor_id(), d.product_id())))
            .collect()),
        Err(e) => Err(Error::Usb {
            backend: BACKEND,
            context: "list devices",
            message: e.to_string(),
        }),
    }
}

/// How many of `found` report serial `sn`.
#[cfg(feature = "std")]
fn serial_count(found: &[DeviceInfo], sn: &str) -> usize {
    found
        .iter()
        .filter(|d| d.serial_number() == Some(sn))
        .count()
}

#[cfg(feature = "std")]
fn label_of(d: &DeviceInfo) -> String {
    let product = d.product_string().unwrap_or("RTL2832U");
    match d.serial_number() {
        Some(s) => format!("{product} serial {s}"),
        None => format!("{product} (no serial)"),
    }
}

/// Every attached dongle, as `rtl:serial=<sn>` — or `rtl:<index>` when
/// the device reports no serial, or shares its serial with another
/// attached dongle (many leave the factory with one serial; the label then
/// says so). The index is the OS's enumeration order, which differs
/// between platforms and can change across replugs: a distinct serial
/// (`eeprom`) is the stable way to name a dongle. Empty when the devices
/// cannot be listed.
#[cfg(feature = "std")]
pub async fn list() -> Vec<DeviceDesc> {
    let found = candidates().await.unwrap_or_default();
    found
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let shared = d
                .serial_number()
                .is_some_and(|s| serial_count(&found, s) > 1);
            DeviceDesc {
                spec: match d.serial_number() {
                    Some(s) if !shared => format!("rtl:serial={s}"),
                    _ => format!("rtl:{i}"),
                },
                label: if shared {
                    format!(
                        "{} (serial shared with another attached dongle)",
                        label_of(d)
                    )
                } else {
                    label_of(d)
                },
                backend: Backend::Rtl,
            }
        })
        .collect()
}

/// A spec's options ([`parse_spec_options`]).
#[cfg(feature = "std")]
#[derive(Debug, Default, PartialEq)]
struct SpecOptions {
    ppm: Option<f64>,
    imr: Option<Vec<ImrPoint>>,
}

/// A spec remainder split into the device part and its options: the
/// device is everything before the first `,` (or empty when the first
/// part is an option), the options `key=value` after it: `ppm=<p>` and
/// `imr=MHz:G:P+…` (`image::parse_imr_table`). Any other key is refused,
/// by name.
#[cfg(feature = "std")]
fn parse_spec_options(rest: &str) -> core::result::Result<(&str, SpecOptions), Error> {
    let bad = |what: String| Error::Unsupported {
        backend: BACKEND,
        what: format!("spec \"rtl:{rest}\": {what}"),
    };
    let is_option = |s: &str| s.starts_with("ppm=") || s.starts_with("imr=");
    let mut parts = rest.split(',');
    let first = parts.next().unwrap_or("");
    let (device, mut options): (&str, Vec<&str>) = if is_option(first) {
        ("", vec![first])
    } else {
        (first, Vec::new())
    };
    options.extend(parts);
    let mut out = SpecOptions::default();
    for o in options {
        match o.split_once('=') {
            Some(("ppm", v)) => {
                out.ppm = Some(
                    v.trim()
                        .parse::<f64>()
                        .map_err(|_| bad(format!("ppm={v} is not a number")))?,
                )
            }
            Some(("imr", v)) => out.imr = Some(image::parse_imr_table(v).map_err(bad)?),
            _ => {
                return Err(bad(format!(
                    "unknown option \"{o}\" (the options are ppm=<parts per million> and imr=MHz:G:P+…)"
                )));
            }
        }
    }
    Ok((device, out))
}

/// The device a spec remainder names: `""` the first found, `"<index>"`
/// the nth, `"serial=<sn>"` by serial — refused when more than one
/// attached dongle reports that serial.
#[cfg(feature = "std")]
async fn select(rest: &str) -> Result<DeviceInfo> {
    let not_found = || Error::NotFound {
        spec: format!("rtl:{rest}"),
    };
    let mut found = candidates().await?;
    if rest.is_empty() {
        return if found.is_empty() {
            Err(not_found())
        } else {
            Ok(found.remove(0))
        };
    }
    if let Some(sn) = rest.strip_prefix("serial=") {
        let n = serial_count(&found, sn);
        if n > 1 {
            return Err(unsupported(format!(
                "spec \"rtl:{rest}\": {n} attached dongles report serial {sn}; open one by index (rtl:<index>, see list) or give each a distinct serial in its EEPROM"
            )));
        }
        return found
            .into_iter()
            .find(|d| d.serial_number() == Some(sn))
            .ok_or_else(not_found);
    }
    let index: usize = rest.parse().map_err(|_| {
        unsupported(format!(
            "spec \"rtl:{rest}\": expected \"\", \"<index>\" or \"serial=<sn>\""
        ))
    })?;
    if index < found.len() {
        Ok(found.remove(index))
    } else {
        Err(not_found())
    }
}

#[cfg(feature = "std")]
impl Rtl {
    /// Open a dongle by spec remainder and bring it to the bench's
    /// measured state: the demodulator's bring-up (`demod::setup`), the
    /// rate at 2.048 Msps (`demod::DEFAULT_OUTPUT_RATE_SPS`), the tuner
    /// image (`image`) with the bring-up front end and the PLL at the
    /// 300 MHz tune every table and the stream's tone tests were taken
    /// against (findings 4e, 10.2b), every write read back where the chip
    /// allows. In a browser the dongles are those the page was granted
    /// ([`Rtl::request`]). (`blocking::Rtl::open` bounds a native open
    /// with `sdr_core::open_with_deadline`.)
    ///
    /// The spec may carry options after the device, comma-separated:
    /// `ppm=<p>` applies [`Rtl::set_ppm`] at the open (`serial=A1,ppm=0.97`,
    /// `0,ppm=-12`, `ppm=3` for the first dongle); `imr=MHz:G:P+…` the
    /// dongle's image-rejection calibration ([`Rtl::set_imr_table`];
    /// `serial=A1,ppm=0.97,imr=805:7:33+420:33:0`).
    pub async fn open(rest: &str) -> Result<Rtl> {
        let (device, options) = parse_spec_options(rest)?;
        let mut rtl = Rtl::open_info(select(device).await?).await?;
        if let Some(table) = options.imr {
            rtl.set_imr_table(table).await?;
        }
        if let Some(ppm) = options.ppm {
            rtl.set_ppm(ppm).await?;
        }
        Ok(rtl)
    }

    /// In a browser: ask the user for a dongle (the WebUSB device chooser,
    /// filtered to [`KNOWN_IDS`]; it must follow a user gesture, such as a
    /// click), then open it as [`Rtl::open`] does. The page keeps the
    /// grant, so later [`Rtl::open`] calls find the dongle without asking.
    #[cfg(target_arch = "wasm32")]
    pub async fn request() -> Result<Rtl> {
        let selectors: Vec<nusb::DeviceSelector> = KNOWN_IDS
            .iter()
            .map(|&(vendor, product)| nusb::DeviceSelector::all().with_vid_pid(vendor, product))
            .collect();
        let info = nusb::request_device(&selectors)
            .await
            .map_err(|e| Error::Usb {
                backend: BACKEND,
                context: "request device",
                message: e.to_string(),
            })?
            .ok_or_else(|| Error::NotFound {
                spec: "rtl: (no dongle chosen)".into(),
            })?;
        Rtl::open_info(info).await
    }

    async fn open_info(info: DeviceInfo) -> Result<Rtl> {
        let label = label_of(&info);
        let dev = complete!(info.open()).map_err(|e| Error::Usb {
            backend: BACKEND,
            context: "open",
            message: e.to_string(),
        })?;
        Rtl::open_transport(Arc::new(usb::NusbTransport::claim(dev).await?), label).await
    }

    /// Bring a dongle up over any [`usb::Transport`]: a device opened some
    /// other way, or a simulated one in tests. `label` is what
    /// [`Rtl::label`] reports. The bring-up is [`Rtl::open`]'s.
    pub async fn open_transport(transport: Arc<dyn usb::Transport>, label: String) -> Result<Rtl> {
        let link = Link::new(transport);
        demod::setup(&link).await?;
        let ratio = demod::ratio_for_output_rate(demod::DEFAULT_OUTPUT_RATE_SPS)?;
        demod::write_rsamp(&link, ratio).await?;
        // The image the bench's records were taken with: the bring-up
        // front end at the 300 MHz tune (findings step 4e; the recorded
        // image in `tables::tuner_image`), the VGA under its code; the LO
        // above it by the stream's IF, as the step-10 tone tests placed it.
        let front = Front::BRING_UP;
        let pll = Pll::program(OPEN_RF_HZ + ddc_if_hz())?;
        let status = link
            .with_tuner(async |l| {
                l.tuner_write_image(&front.image(&pll)).await?;
                sdr_core::time::sleep(demod::PLL_SETTLE).await;
                l.tuner_status().await
            })
            .await?;
        // The dongle must still answer after the image (findings step 4a:
        // the one write that stops it is refused by the request layer).
        link.sys_read8(protocol::DEMOD_CTL).await?;
        Ok(Rtl {
            link,
            label,
            front,
            pll,
            status,
            ratio,
            decimation: 1,
            rate_asked: demod::DEFAULT_OUTPUT_RATE_SPS,
            rf_asked: OPEN_RF_HZ,
            ppm: 0.0,
            imr: Vec::new(),
        })
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn tuner_kind(&self) -> RtlTuner {
        RtlTuner::R820TFamily
    }

    /// The output sample rate: rsamp_ratio = floor(f_xtal / (4 · rate) ·
    /// 2^22) ([RTL2832U] 9.4 with findings 10.2a's rule, output = f_symbol
    /// / 4), written now and again by the stream's start sequence. Every
    /// rate from 0.24 to 2.88 Msps is accepted
    /// (`demod::OUTPUT_RATE_SPAN_SPS`). The chip resamples at the rate
    /// itself in the spans it is used at directly, 0.905-2.88 and
    /// 0.24-0.30 Msps (`demod::OUTPUT_RATE_GOOD_SPS`,
    /// `demod::OUTPUT_RATE_LOW_SPS`; findings 10.2a, 25). Between them,
    /// where the chip resamples at another rate (findings 25: the
    /// register's top bit adds 2^26 to the ratio, `demod::RSAMP_TOP_BIT`;
    /// the low span is reached through it), it runs at 2, 3 or 4 times the
    /// rate and the stream decimates
    /// (`demod::plan_for_output_rate`, the `decim` module):
    /// [`Stream::recv_into`] hands out the rate asked for, and a stream read
    /// with [`Stream::read`] hands out the chip's wire bytes at the chip's
    /// rate ([`Rtl::chip_rate`]) instead — the first read decides. Refused with
    /// their record (`demod::ratio_for_output_rate`): under 0.24 Msps, 3.2
    /// Msps (half the rate in 2 runs of 8) and f_symbol ≥ 14.4 MHz
    /// (broken).
    ///
    /// No rate in the spans puts phase steps in the samples: with the USRP
    /// as the tone source, none at any of nine rates 1.024-2.88 Msps, at
    /// the start or after a change (findings 19), nor at the twelve
    /// measured from 0.24 to 1.0 Msps (findings 25). The steps findings
    /// 16.5 reported (all through at 2.88 Msps, after a start below 1.44)
    /// were the HackRF source's.
    ///
    /// Refused while a stream exists: changing the ratio of a running
    /// stream did not dependably take (findings 10.2a), so the rate is
    /// fixed from `rx_stream` until the stream is dropped. A stream made
    /// after a drop and a rate change runs at the new rate (findings
    /// step 11: 1.024, 2.4, 2.88 and 2.048 Msps in turn).
    pub async fn set_rate(&mut self, sps: f64) -> Result<()> {
        if self.link.stream_open() {
            return Err(unsupported(format!(
                "rate {:.6} Msps: a stream exists; drop it first (a live rsamp_ratio change did not dependably take, findings 10.2a)",
                sps / 1e6
            )));
        }
        let plan = demod::plan_for_output_rate(sps)?.corrected(sps, self.scale());
        demod::write_rsamp(&self.link, plan.ratio).await?;
        self.ratio = plan.ratio;
        self.decimation = plan.decimation;
        self.rate_asked = sps;
        Ok(())
    }

    /// The output rate as the resampler register holds it now: its
    /// f_symbol / 4, over the stream's decimation.
    pub async fn rate(&self) -> Result<f64> {
        Ok(demod::read_f_symbol(&self.link).await? * self.scale()
            / demod::OUTPUT_DECIMATION
            / self.decimation as f64)
    }

    /// The output rate last set (no request).
    pub fn rate_set(&self) -> f64 {
        self.chip_rate() / self.decimation as f64
    }

    /// The decimation the stream applies to the chip's samples for the rate
    /// set (1: none).
    pub fn decimation(&self) -> u32 {
        self.decimation
    }

    /// The rate the chip delivers for the rate set (no request): the rate
    /// set × [`Rtl::decimation`]. The raw samples (wire bytes) come at it.
    pub fn chip_rate(&self) -> f64 {
        demod::output_rate_of_ratio(self.ratio) * self.scale()
    }

    /// The crystal's frequency over its nominal 28.8 MHz, as corrected.
    fn scale(&self) -> f64 {
        1.0 + self.ppm * 1e-6
    }

    /// The crystal correction in use, parts per million.
    pub fn ppm(&self) -> f64 {
        self.ppm
    }

    /// The dongle's image-rejection calibration: IMR_G / IMR_P per RF
    /// (`image::ImrPoint`), the nearest point used at every tuning. Empty:
    /// 32/32 everywhere, as before. Findings 28: the best settings differ
    /// from unit to unit (a setting one dongle wants costs another up to 18
    /// dB) and move smoothly with frequency, so each dongle's are measured
    /// on the bench (`examples/imrsearch.rs`) and carried with it — usually
    /// in its spec, `imr=MHz:G:P+…`. Takes effect at once: the RF last asked
    /// for is tuned again (allowed while streaming, inside a pause).
    pub async fn set_imr_table(&mut self, table: Vec<ImrPoint>) -> Result<()> {
        if let Some(bad) = table
            .iter()
            .find(|p| p.g > image::IMR_FIELD || p.p > image::IMR_FIELD || !p.rf_hz.is_finite())
        {
            return Err(unsupported(format!(
                "imr point {:.3} MHz {}:{}: G and P are 0..=63",
                bad.rf_hz / 1e6,
                bad.g,
                bad.p
            )));
        }
        let old = core::mem::replace(&mut self.imr, table);
        if let Err(e) = self.set_freq(&TuneRequest::auto(self.rf_asked)).await {
            self.imr = old;
            return Err(e);
        }
        Ok(())
    }

    /// The image-rejection calibration in use.
    pub fn imr_table(&self) -> &[ImrPoint] {
        &self.imr
    }

    /// Correct for the dongle's crystal: it runs `ppm` parts per million
    /// above 28.8 MHz (below, negative). One crystal clocks the tuner's
    /// synthesizer, the DDC and the resampler (findings 26: with the
    /// synthesizer's own offset taken off, a tone's error from 70 to 1720
    /// MHz was a straight line through zero in its frequency, −0.90 ppm on
    /// one dongle, and with that correction set it was within 0.5 Hz at
    /// every point), so every
    /// frequency the dongle makes is off by the same proportion and the
    /// correction is one scale: the synthesizer is programmed for RF /
    /// scale, the resampler for rate / scale, and the RF and the rates the
    /// receiver reports are the words' × scale. The sign: uncorrected, a
    /// crystal running +1 ppm puts the LO 10^-6 × f high, so a tone at f
    /// lands 10^-6 × f below where the words put it — a tone read low by
    /// 0.90 ppm wants `set_ppm(0.90)`.
    ///
    /// Takes effect at once: the RF last asked for is tuned again under the new
    /// correction, and the rate set again — unless a stream exists, where
    /// the rate keeps the ratio it started with (a live ratio change did
    /// not dependably take, findings 10.2a) and is reported under the new
    /// correction, the stream's true rate if the correction is the
    /// dongle's. Refused beyond ±[`PPM_LIMIT`].
    pub async fn set_ppm(&mut self, ppm: f64) -> Result<()> {
        if !ppm.is_finite() || ppm.abs() > PPM_LIMIT {
            return Err(unsupported(format!(
                "a crystal correction of {ppm} ppm: the limit is ±{PPM_LIMIT} ppm"
            )));
        }
        let rf = self.rf_asked;
        let old = self.ppm;
        self.ppm = ppm;
        let retuned = async {
            if !self.link.stream_open() {
                self.set_rate(self.rate_asked).await?;
            }
            self.set_freq(&TuneRequest::auto(rf)).await
        }
        .await;
        if let Err(e) = retuned {
            self.ppm = old;
            if !self.link.stream_open() {
                self.set_rate(self.rate_asked).await?;
            }
            return Err(e);
        }
        Ok(())
    }

    /// Tune: the LO above the RF by the DDC's IF (high side, findings 4h;
    /// `demod::ddc_if_hz`, 3.570 MHz, findings 10.2b), the synthesizer by
    /// the documented formula with the measured divider rule (`pll`,
    /// /16 included, findings 10.5), the front-end path from the band
    /// table (`band`), the actual RF from the words written, the
    /// synthesizer's own offset from them (`pll::VCO_OFFSET_HZ`) and the
    /// crystal correction ([`Rtl::set_ppm`]) included. Anything outside
    /// [`RF_RANGE_HZ`] (42-1700 MHz) is refused; inside it every RF is
    /// reachable.
    /// Allowed while streaming: the bench programmed the tuner after the
    /// start in every minimisation run (findings 10.4).
    pub async fn set_freq(&mut self, req: &TuneRequest) -> Result<TuneResult> {
        let rf = match req.rf {
            TunePolicy::Manual(f) => f,
            TunePolicy::Auto | TunePolicy::Default => req.target_freq,
        };
        if let TunePolicy::Manual(dsp) = req.dsp
            && dsp != 0.0
        {
            return Err(unsupported(format!(
                "no DSP tuning stage: a DSP offset of {dsp} Hz cannot be applied in the device"
            )));
        }
        let range = rf_range();
        if !rf.is_finite() || rf < range.0 || rf > range.1 {
            return Err(unsupported(format!(
                "RF {:.6} MHz is outside the tuning range {}-{} MHz (the bottom documented, [R820T] Table 1-2; the top measured, findings 26)",
                rf / 1e6,
                range.0 / 1e6,
                range.1 / 1e6
            )));
        }
        let candidates = Pll::candidates_delivering(rf / self.scale() + ddc_if_hz())?;
        let path =
            band::path_for(rf).ok_or_else(|| unsupported("the band table is empty".into()))?;
        let mut front = self.front;
        front.rffilt = path.rffilt;
        front.rfmux = path.rfmux;
        front.tf_nch = path.tf_nch;
        front.tf_lp = path.tf_lp;
        // The dongle's own image-rejection setting for this RF (its
        // calibration's nearest point; 32/32 without one, findings 28).
        (front.imr_g, front.imr_p) = image::imr_for(&self.imr, rf);
        let imr_changed = (front.imr_g, front.imr_p) != (self.front.imr_g, self.front.imr_p);
        // Review A5: R2 judged, not only read. Each divider that keeps the
        // VCO in its documented range is tried, smallest first, until R2
        // says locked: where a unit's VCO really starts varies (findings
        // 20: one dongle locked at /2 down to 1.709 GHz over 1,884 points;
        // findings 21: two others not below 1.72 GHz, so 851.5-857.75 MHz
        // took /4 on them). If none locks the tuning is refused before it
        // is taken as the receiver's state, so a stream's change writes the
        // last good tuning back.
        let mut tried: Vec<String> = Vec::new();
        let mut locked = None;
        for (i, pll) in candidates.iter().enumerate() {
            // R26, R27 and the PLL words are above R15: written unverified
            // (findings step 4a: the read wraps after 16 registers).
            let status = self
                .link
                .with_tuner(async |l| {
                    if i == 0 {
                        l.tuner_write(R26, front.r26()).await?;
                        l.tuner_write(R27, front.r27()).await?;
                        // R8 and R9 read back (R5-R15 do, findings step 4a).
                        if imr_changed {
                            l.tuner_write(R8, front.r8()).await?;
                            l.tuner_write(R9, front.r9()).await?;
                            l.tuner_verify(&front.image(pll)).await?;
                        }
                    }
                    for (reg, v) in [
                        (R16, pll.r16()),
                        (R20, pll.r20()),
                        (R21, pll.r21()),
                        (R22, pll.r22()),
                    ] {
                        l.tuner_write(reg, v).await?;
                    }
                    sdr_core::time::sleep(demod::PLL_SETTLE).await;
                    l.tuner_status().await
                })
                .await?;
            if pll::r2_says_locked(status[2]) {
                locked = Some((*pll, status));
                break;
            }
            tried.push(format!(
                "R2 = {} with the VCO at {:.3} GHz (/{})",
                status[2] & pll::VCO_INDICATOR_MASK,
                pll.vco_hz() / 1e9,
                pll.divider
            ));
        }
        let Some((pll, status)) = locked else {
            let (lo, hi) = pll::r2_locked_span();
            return Err(Error::Device {
                backend: BACKEND,
                context: "tune",
                message: format!(
                    "RF {:.6} MHz: the synthesizer reports no lock (locked {lo}-{hi}): {}",
                    rf / 1e6,
                    tried.join("; ")
                ),
            });
        };
        self.front = front;
        self.pll = pll;
        self.rf_asked = rf;
        self.status = status;
        let actual_rf = (pll.lo_delivered_hz() - ddc_if_hz()) * self.scale();
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
        Ok((self.pll.lo_delivered_hz() - ddc_if_hz()) * self.scale())
    }

    /// The receiver's last good tuner state written whole again (the image
    /// of `front` at `pll`, as `open` writes it): after a change that
    /// failed part way, the hardware is made what this `Rtl` says it is.
    /// The state fields are set only when a change's writes all succeed,
    /// so they always hold the last good state.
    pub(crate) async fn reassert(&mut self) -> Result<()> {
        let img = self.front.image(&self.pll);
        self.status = self
            .link
            .with_tuner(async |l| {
                l.tuner_write_image(&img).await?;
                sdr_core::time::sleep(demod::PLL_SETTLE).await;
                l.tuner_status().await
            })
            .await?;
        Ok(())
    }

    fn codes(&self) -> GainCodes {
        GainCodes {
            lna: self.front.lna_gain,
            mix: self.front.mix_gain,
            vga: self.front.vga_code,
        }
    }

    /// R5, R7 and R12 for the codes, each verified by read-back (R5-R15
    /// read back exactly, findings step 4a).
    async fn write_codes(&mut self, codes: GainCodes) -> Result<()> {
        let mut front = self.front;
        front.lna_gain = codes.lna.min(15);
        front.mix_gain = codes.mix.min(15);
        front.vga_code = codes.vga.min(15);
        self.write_front_regs(front, &[R5, R7, R12]).await
    }

    /// Write the given registers of a front end and verify the image's
    /// R5-R15 against the register file.
    async fn write_front_regs(&mut self, front: Front, regs: &[u8]) -> Result<()> {
        let img = front.image(&self.pll);
        self.link
            .with_tuner(async |l| {
                for &reg in regs {
                    l.tuner_write(reg, img[reg as usize]).await?;
                }
                l.tuner_verify(&img).await
            })
            .await?;
        self.front = front;
        Ok(())
    }

    /// One number over the three stages, front first (`gain`): the
    /// nearest rung of the ladder.
    pub async fn set_gain(&mut self, db: f64) -> Result<()> {
        self.write_codes(gain::codes_for_db(db)).await
    }

    /// The sum of the three stages' dB above code 0 (`gain`).
    pub fn gain(&self) -> f64 {
        self.codes().total_db()
    }

    /// (LNA, mixer, VGA) in dB above each stage's code 0.
    pub fn gain_stages(&self) -> (f64, f64, f64) {
        let c = self.codes();
        (
            gain::code_db(Stage::Lna, c.lna),
            gain::code_db(Stage::Mixer, c.mix),
            gain::code_db(Stage::Vga, c.vga),
        )
    }

    /// Each stage to the code whose measured dB is nearest the request.
    pub async fn set_gain_stages(&mut self, lna_db: f64, mixer_db: f64, vga_db: f64) -> Result<()> {
        let level = |stage: Stage, db: f64| nearest_level(&gain::levels_db(stage), db) as u8;
        self.write_codes(GainCodes {
            lna: level(Stage::Lna, lna_db),
            mix: level(Stage::Mixer, mixer_db),
            vga: level(Stage::Vga, vga_db),
        })
        .await
    }

    /// The dB of every level of a stage (`tables::gain`).
    pub fn stage_levels_db(&self, stage: Stage) -> Vec<f64> {
        gain::levels_db(stage).to_vec()
    }

    pub fn stage_readings(&self) -> Vec<StageReading<Stage>> {
        self.codes().readings()
    }

    pub async fn set_stage_level(&mut self, stage: Stage, level: usize) -> Result<()> {
        if level >= gain::CODES {
            return Err(unsupported(format!(
                "{stage:?} level {level}: the stage has {} levels",
                gain::CODES
            )));
        }
        self.write_codes(self.codes().with_level(stage, level as u8))
            .await
    }

    /// `tuner_agc`: the LNA and mixer gains from the tuner's own power
    /// detectors (LNA_GAIN_MODE = 0, MIXGAIN_MODE = 1, PDET1 and PDET3 on;
    /// [R820T2] Table 1-3). `demod_agc`: the VGA's gain from the VAGC
    /// pin (VGA_MODE = 1), which the demodulator's IF AGC loop drives
    /// through its AGC_IF pin on this board (findings step 4b); the loop
    /// is enabled with it and disabled without it ([RTL2832U] Table 2: the
    /// open leaves every AGC off). Both off unless asked for: the default
    /// is fixed gain. R5, R6, R7 and R12, verified.
    pub async fn set_agc(&mut self, tuner_agc: bool, demod_agc: bool) -> Result<()> {
        let mut front = self.front;
        front.lna_manual = !tuner_agc;
        front.mix_manual = !tuner_agc;
        front.pdet1_on = tuner_agc;
        front.pdet3_on = tuner_agc;
        front.vga_from_pin = demod_agc;
        self.write_front_regs(front, &[R5, R6, R7, R12]).await?;
        self.link
            .demod_rmw8(
                protocol::AAGC_PAGE,
                protocol::AAGC_OFF,
                protocol::EN_IF_AGC,
                if demod_agc { protocol::EN_IF_AGC } else { 0 },
            )
            .await?;
        Ok(())
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
    pub async fn set_bandwidth(&mut self, hz: f64) -> Result<()> {
        if !hz.is_finite() || hz <= 0.0 {
            return Err(unsupported(format!(
                "bandwidth {hz} Hz: a positive width is needed"
            )));
        }
        let s = filter::setting_for_bandwidth(hz);
        let mut front = self.front;
        front.filt_code = s.filt_code;
        front.filt_bw = s.filt_bw;
        self.write_front_regs(front, &[R10, R11]).await
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
    pub async fn tuner_status(&mut self) -> Result<RtlTunerStatus> {
        self.status = self
            .link
            .with_tuner(async |l| l.tuner_status().await)
            .await?;
        let s = self.status;
        Ok(RtlTunerStatus {
            tuner: self.tuner_kind(),
            chip_id_ok: s[0] == protocol::TUNER_R0_CHECK,
            pll: PllState::new(&self.pll, s[2]),
            rf_hz: (self.pll.lo_delivered_hz() - ddc_if_hz()) * self.scale(),
            if_hz: ddc_if_hz() * self.scale(),
            xtal_hz: XTAL_HZ * self.scale(),
            ppm: self.ppm,
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
            imr_g: self.front.imr_g,
            imr_p: self.front.imr_p,
        })
    }

    /// The receiver as a [`Stream`] at the rate it holds ([`session`]). The
    /// stream owns the receiver until `finish` hands it back, so nothing
    /// reaches the control pipe while samples flow except what the stream
    /// offers. `transfer_bytes` is rounded down to whole packets (sdr-core's
    /// engine; at least one; 0 is `stream::DEFAULT_TRANSFER_BYTES`, 16 KiB);
    /// at least [`session::MIN_QUEUE_DEPTH`] transfers are kept in flight
    /// (fewer let a pause or a request use the queue up, findings 16.7).
    /// Nothing is written until the first read (or `start`).
    pub async fn into_stream(
        self,
        transfer_bytes: usize,
        queue_depth: usize,
    ) -> std::result::Result<Stream, StreamError> {
        Stream::new(self, transfer_bytes, queue_depth).await
    }

    pub(crate) fn link_ref(&self) -> &Link {
        &self.link
    }

    /// The raw stream under every kind, with our taps.
    pub(crate) async fn open_stream(
        &mut self,
        transfer_bytes: usize,
        queue_depth: usize,
    ) -> Result<RtlStream> {
        self.open_stream_with_taps(transfer_bytes, queue_depth, taps::design_registers())
            .await
    }

    /// The raw stream, for bench programs (`--features bench`): every
    /// method, the lossy ones included. Over the bulk IN endpoint 0x81 of
    /// interface 0 (`stream::BULK_IN_ENDPOINT`), 8-bit unsigned I/Q, even
    /// byte I (`stream::WIRE_FORMAT`, findings 10.2b), our own taps
    /// (`taps::design_registers`). One stream at a time. Always the chip's
    /// samples at the chip's rate ([`RtlStream::rate`]): for a rate the
    /// [`Stream`] makes by decimation, that is [`Rtl::chip_rate`], k times
    /// the rate set ([`Rtl::decimation`]).
    #[cfg(feature = "bench")]
    pub async fn rx_stream(
        &mut self,
        transfer_bytes: usize,
        queue_depth: usize,
    ) -> Result<RtlStream> {
        self.open_stream(transfer_bytes, queue_depth).await
    }

    /// [`Rtl::rx_stream`] with a given image of the 20 tap registers
    /// (page 1 0x1C-0x2F, `taps::pack`'s layout) in place of our design.
    #[cfg(feature = "bench")]
    pub async fn rx_stream_with_taps(
        &mut self,
        transfer_bytes: usize,
        queue_depth: usize,
        tap_regs: [u8; taps::TAP_REGS],
    ) -> Result<RtlStream> {
        self.open_stream_with_taps(transfer_bytes, queue_depth, tap_regs)
            .await
    }

    /// The interface is claimed by the transport ([`usb::Transport::
    /// open_bulk_in`]; nusb's detaches a kernel driver bound to it first).
    async fn open_stream_with_taps(
        &mut self,
        transfer_bytes: usize,
        queue_depth: usize,
        tap_regs: [u8; taps::TAP_REGS],
    ) -> Result<RtlStream> {
        if !self.link.claim_stream() {
            return Err(unsupported(
                "a stream of this receiver already exists; one at a time".into(),
            ));
        }
        let endpoint = match self.link.open_bulk_in().await {
            Ok(ep) => ep,
            Err(e) => {
                self.link.release_stream();
                return Err(e);
            }
        };
        let rate = self.chip_rate();
        let transfer_bytes = if transfer_bytes == 0 {
            stream::DEFAULT_TRANSFER_BYTES
        } else {
            transfer_bytes
        };
        let queue_depth = if queue_depth == 0 {
            stream::DEFAULT_QUEUE_DEPTH
        } else {
            queue_depth
        };
        let inner = BulkIqStream::with_endpoint(
            BACKEND,
            endpoint,
            stream::WIRE_FORMAT,
            transfer_bytes,
            queue_depth,
            rate,
        );
        Ok(RtlStream::new(
            inner,
            self.link.clone(),
            self.ratio,
            tap_regs,
        ))
    }

    /// The 20 tap registers as they read now (page 1 0x1C-0x2F; after a
    /// power cycle, before any stream, the chip's power-on values).
    pub async fn taps(&self) -> Result<[u8; taps::TAP_REGS]> {
        let v = self
            .link
            .demod_read(
                protocol::TAP_PAGE,
                protocol::TAP_FIRST,
                taps::TAP_REGS as u16,
            )
            .await?;
        let mut r = [0u8; taps::TAP_REGS];
        r.copy_from_slice(&v[..taps::TAP_REGS]);
        Ok(r)
    }

    /// The configuration EEPROM, the whole part: all 256 offsets in
    /// 16-byte block 3 reads (findings 17), cut to 128 bytes when the
    /// upper half repeats the lower (`eeprom::part_size`). Sixteen control
    /// transfers: during a stream each costs samples (findings 16).
    pub async fn read_eeprom(&self) -> Result<Eeprom> {
        Eeprom::from_read(&eeprom::read_raw(&self.link).await?)
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
    pub async fn write_eeprom(&mut self, new: &Eeprom) -> Result<EepromWriteReport> {
        if self.link.stream_open() {
            return Err(unsupported(
                "EEPROM write: a stream exists; drop it first".into(),
            ));
        }
        eeprom::write(&self.link, new).await
    }

    /// The request layer, for bench programs (`--features bench`).
    #[cfg(feature = "bench")]
    pub fn link(&self) -> &Link {
        &self.link
    }
}

/// How long the request layer waits on one control transfer.
#[cfg(feature = "std")]
pub const CONTROL_TIMEOUT: Duration = usb::CONTROL_TIMEOUT;

/// The RF the receiver opens tuned to: 300 MHz, where findings 4e
/// received the tone and the step-10 tone tests centred (findings 10.2b,
/// `TONE_RF_HZ` of `examples/harness/stream_confirm.rs`).
pub const OPEN_RF_HZ: f64 = 300.0e6;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demod::ddc_if_hz;

    /// The spec's options: the device part as before, `ppm=` after it or
    /// alone; anything else refused by name.
    #[test]
    fn spec_options_carry_the_crystal_correction() {
        let ppm = |p: Option<f64>| SpecOptions { ppm: p, imr: None };
        assert_eq!(parse_spec_options("").unwrap(), ("", ppm(None)));
        assert_eq!(
            parse_spec_options("serial=A1").unwrap(),
            ("serial=A1", ppm(None))
        );
        assert_eq!(
            parse_spec_options("serial=A1,ppm=0.97").unwrap(),
            ("serial=A1", ppm(Some(0.97)))
        );
        assert_eq!(
            parse_spec_options("1,ppm=-12").unwrap(),
            ("1", ppm(Some(-12.0)))
        );
        assert_eq!(parse_spec_options("ppm=3").unwrap(), ("", ppm(Some(3.0))));
        assert!(
            parse_spec_options("serial=A1,ppm=x")
                .unwrap_err()
                .to_string()
                .contains("not a number")
        );
        assert!(
            parse_spec_options("serial=A1,gain=3")
                .unwrap_err()
                .to_string()
                .contains("unknown option")
        );
    }

    /// The image-rejection calibration in the spec: `MHz:G:P` points joined
    /// by `+`, beside `ppm` or alone; malformed points refused by name.
    #[test]
    fn spec_options_carry_the_imr_calibration() {
        let (dev, o) = parse_spec_options("serial=A1,ppm=0.9,imr=805:7:33+420:33:0").unwrap();
        assert_eq!(dev, "serial=A1");
        assert_eq!(o.ppm, Some(0.9));
        assert_eq!(
            o.imr,
            Some(vec![
                ImrPoint {
                    rf_hz: 805e6,
                    g: 7,
                    p: 33
                },
                ImrPoint {
                    rf_hz: 420e6,
                    g: 33,
                    p: 0
                }
            ])
        );
        assert_eq!(
            parse_spec_options("imr=1090:9:33").unwrap().1.imr,
            Some(vec![ImrPoint {
                rf_hz: 1090e6,
                g: 9,
                p: 33
            }])
        );
        for bad in [
            "serial=A1,imr=805:64:0",
            "serial=A1,imr=805:7",
            "serial=A1,imr=x:1:1",
            "serial=A1,imr=805:7:33+",
        ] {
            assert!(parse_spec_options(bad).is_err(), "{bad}");
        }
    }

    /// The nearest point's setting; 32/32 with no table; a tie goes to the
    /// lower frequency.
    #[test]
    fn the_imr_setting_is_the_nearest_points() {
        let t = [
            ImrPoint {
                rf_hz: 420e6,
                g: 33,
                p: 0,
            },
            ImrPoint {
                rf_hz: 805e6,
                g: 7,
                p: 33,
            },
            ImrPoint {
                rf_hz: 1090e6,
                g: 9,
                p: 33,
            },
        ];
        assert_eq!(image::imr_for(&[], 805e6), (32, 32));
        assert_eq!(image::imr_for(&t, 154e6), (33, 0));
        assert_eq!(image::imr_for(&t, 851e6), (7, 33));
        assert_eq!(image::imr_for(&t, 1000e6), (9, 33));
        assert_eq!(
            image::imr_for(&t, 612.5e6),
            (33, 0),
            "the midpoint ties to the lower"
        );
    }

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
