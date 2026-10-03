//! An RTL-SDR (RTL2832U + R820T2/R828D) driver over nusb, in pure Rust.
//!
//! No libusb, no librtlsdr: the stack is this module → nusb → the OS.
//! Register sequences re-derived from rtl-sdr-rs (MPL-2.0); see each
//! submodule's provenance note. The design goal is observability — every
//! quantity the hardware can report is read back (PLL lock, live gain
//! indices, resampler registers), and what it can't (silent FIFO drops)
//! is inferred and counted in the stream layer.

mod eeprom;
mod r820t2;
mod r82xx;
pub mod regs;
mod stream;
mod usb;

pub use eeprom::{EEPROM_SIZE, compose as eeprom_compose, Fields as EepromFields, RtlEeprom};
pub use r82xx::PllState;
pub use stream::RtlStream;

use nusb::MaybeFuture;

use sdr_core::{Error, Result};
use sdr_core::gain::{Stage, StageReading};
use crate::r82xx::{R82XX_IF_FREQ, R82xx, R82xxVariant, R828D_XTAL_FREQ};
use crate::regs::{DEFAULT_FIR, EP_BULK_IN, KNOWN_IDS, RSAMP_RATIO_FIELD, RSAMP_RATIO_FRACTION_BITS, RSAMP_RATIO_TOP_BIT, RTL_XTAL_HZ};
use crate::usb::{Rtl2832, RtlUsb};
use sdr_core::{TunePolicy, TuneRequest, TuneResult};

/// Which tuner was found, and how it's wired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RtlTuner {
    R820T,
    R828D,
    /// RTL-SDR Blog V4: an R828D fed 28.8 MHz, with an HF upconverter.
    R828DBlogV4,
}

/// Everything the tuner can tell us about where it actually is.
#[derive(Clone, Copy, Debug)]
pub struct RtlTunerStatus {
    /// Actual tuned frequency, derived from the exact fractional-N
    /// registers written (the RW registers are write-only).
    pub actual_freq_hz: f64,
    /// True hardware readback of the PLL lock bit.
    pub pll_locked: bool,
    /// Live per-stage gains in dB. LNA and mixer indices are hardware
    /// readback (valid under AGC too); VGA is the cached register.
    pub lna_gain_db: f64,
    pub mixer_gain_db: f64,
    pub vga_gain_db: f64,
    /// Current low-IF the demod is downconverting from.
    pub if_freq_hz: f64,
    /// Tuner PLL reference.
    pub xtal_hz: f64,
    pub tuner: RtlTuner,
    /// The exact fractional-N programming behind `actual_freq_hz`.
    pub pll: Option<r82xx::PllState>,
}

/// An open RTL-SDR dongle.
pub struct Rtl {
    usb: RtlUsb,
    tuner: R82xx,
    label: String,
    /// Actual sample rate the resampler settled on (0 until set).
    rate_hz: f64,
    /// Actual tuned frequency (derived; 0 until tuned).
    freq_hz: f64,
    /// Explicit tuner bandwidth; 0 tracks the sample rate.
    bw_hz: u32,
}

impl Rtl {
    /// Open by spec remainder: `""` (first found), an index (`"0"`), or
    /// `"serial=<sn>"`.
    ///
    /// Bounded: the whole enumerate/open/claim sequence runs under
    /// [`sdr_core::open_with_deadline`], because a wedged device
    /// can enumerate and then never answer — the absent-device case
    /// fails fast on its own, the wedged case only fails by deadline.
    pub fn open(rest: &str) -> Result<Rtl> {
        let rest = rest.to_string();
        sdr_core::open_with_deadline("rtl", move || Self::open_inner(&rest))
    }

    fn open_inner(rest: &str) -> Result<Rtl> {
        let want_serial = rest.strip_prefix("serial=");
        let want_index: Option<usize> = if want_serial.is_none() && !rest.is_empty() {
            Some(rest.parse().map_err(|_| Error::NotFound {
                spec: format!("rtl:{rest}"),
            })?)
        } else {
            None
        };

        let devices = nusb::list_devices()
            .wait()
            .map_err(|e| Error::Usb {
                backend: "rtl",
                context: "list_devices",
                message: e.to_string(),
            })?
            .filter(|d| {
                KNOWN_IDS
                    .iter()
                    .any(|(vid, pid, _)| d.vendor_id() == *vid && d.product_id() == *pid)
            });

        let mut index = 0usize;
        for info in devices {
            if let Some(sn) = want_serial {
                if info.serial_number() != Some(sn) {
                    continue;
                }
            } else if let Some(want) = want_index
                && index != want
            {
                index += 1;
                continue;
            }
            let label = format!(
                "{} {} (serial {})",
                info.manufacturer_string().unwrap_or("?"),
                info.product_string().unwrap_or("RTL2832U"),
                info.serial_number().unwrap_or("?"),
            );
            let device = info.open().wait().map_err(|e| Error::Usb {
                backend: "rtl",
                context: "open",
                message: e.to_string(),
            })?;
            return Rtl::init(RtlUsb::new(device)?, label);
        }
        Err(Error::NotFound {
            spec: format!("rtl:{rest}"),
        })
    }

    fn init(usb: RtlUsb, label: String) -> Result<Rtl> {
        usb.init_baseband(&DEFAULT_FIR)?;
        usb.set_i2c_repeater(true)?;

        // The Blog V4 shares the generic VID/PID; it identifies itself by
        // its USB strings, and feeds the tuner 28.8 MHz instead of the
        // R828D's usual 16 MHz crystal.
        let (manufacturer, product) = usb.usb_strings();
        let is_blog_v4 =
            manufacturer.as_deref() == Some("RTLSDRBlog") && product.as_deref() == Some("Blog V4");

        let Some(i2c_addr) = R82xx::probe(&usb) else {
            let _ = usb.set_i2c_repeater(false);
            return Err(Error::Device {
                backend: "rtl",
                context: "tuner probe",
                message: "no R820T/R828D found at 0x34/0x74 (other tuners unsupported)".into(),
            });
        };
        let (variant, xtal) = if i2c_addr == r82xx::R828D_I2C_ADDR {
            let v = R82xxVariant::R828D {
                blog_v4: is_blog_v4,
            };
            (
                v,
                if is_blog_v4 {
                    RTL_XTAL_HZ
                } else {
                    R828D_XTAL_FREQ
                },
            )
        } else {
            (R82xxVariant::R820T, RTL_XTAL_HZ)
        };
        let mut tuner = R82xx::new(variant, xtal);

        // R82xx SDR plan: low-IF with spectrum inversion, I-ADC only.
        usb.configure_low_if_input(R82XX_IF_FREQ)?;

        tuner.init(&usb)?;
        usb.set_i2c_repeater(false)?;

        Ok(Rtl {
            usb,
            tuner,
            label,
            rate_hz: 0.0,
            freq_hz: 0.0,
            bw_hz: 0,
        })
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn tuner_kind(&self) -> RtlTuner {
        match self.tuner.variant {
            R82xxVariant::R820T => RtlTuner::R820T,
            R82xxVariant::R828D { blog_v4: false } => RtlTuner::R828D,
            R82xxVariant::R828D { blog_v4: true } => RtlTuner::R828DBlogV4,
        }
    }

    /// Set the sample rate. The RTL2832 resamples from its 28.8 MHz clock
    /// by a 22.22-bit fixed-point ratio; the exact achieved rate is stored
    /// and returned by [`Rtl::rate`]. Valid: 226–300 k and 900 k–3.2 Msps.
    pub fn set_rate(&mut self, rate_sps: f64) -> Result<()> {
        let rate = rate_sps as u32;
        if rate <= 225_000 || rate > 3_200_000 || (300_000 < rate && rate <= 900_000) {
            return Err(Error::Unsupported {
                backend: "rtl",
                what: format!("sample rate {rate} Hz (valid: 226k–300k, 900k–3.2M)"),
            });
        }

        let rsamp_ratio = ((RTL_XTAL_HZ as u128 * (1u128 << RSAMP_RATIO_FRACTION_BITS)) / rate as u128) & RSAMP_RATIO_FIELD as u128;
        let real_ratio = rsamp_ratio | ((rsamp_ratio & RSAMP_RATIO_TOP_BIT as u128) << 1);
        self.rate_hz = (RTL_XTAL_HZ as u128 * (1u128 << RSAMP_RATIO_FRACTION_BITS)) as f64 / real_ratio as f64;

        // Tuner IF filter tracks the rate unless an explicit bandwidth is
        // set; the filter choice moves the low-IF, so reprogram it and
        // retune afterwards.
        self.usb.set_i2c_repeater(true)?;
        let bw = if self.bw_hz > 0 {
            self.bw_hz
        } else {
            self.rate_hz as u32
        };
        let if_freq = self.tuner.set_bandwidth(&self.usb, bw)?;
        self.usb.set_i2c_repeater(false)?;
        self.usb.set_if_freq(if_freq)?;
        if self.freq_hz > 0.0 {
            self.retune(self.freq_hz)?;
        }

        self.usb.set_resampler(rsamp_ratio as u32)?;
        Ok(())
    }

    /// The actual sample rate — re-derived from the resampler registers on
    /// the chip, not echoed from a cache.
    pub fn rate(&self) -> Result<f64> {
        let ratio = self.usb.resampler_ratio()? & RSAMP_RATIO_FIELD;
        if ratio == 0 {
            return Ok(0.0);
        }
        let real_ratio = ratio | ((ratio & RSAMP_RATIO_TOP_BIT) << 1);
        Ok((RTL_XTAL_HZ as u128 * (1u128 << RSAMP_RATIO_FRACTION_BITS)) as f64 / real_ratio as f64)
    }

    /// Read the raw 256-byte configuration EEPROM (VID/PID and the USB
    /// strings served at enumeration). Parse with [`RtlEeprom::parse`].
    pub fn read_eeprom(&self) -> Result<[u8; 256]> {
        eeprom::read(&self.usb)
    }

    /// Write a full EEPROM image: bytes already holding their target
    /// value are skipped (write endurance), the rest are paced through
    /// the 24C02's write cycle, and the whole image is readback-verified
    /// before this returns Ok. Descriptors are read at enumeration —
    /// the change takes effect on the next replug.
    pub fn write_eeprom(&mut self, image: &[u8; 256]) -> Result<()> {
        eeprom::write(&self.usb, image)
    }

    fn retune(&mut self, hz: f64) -> Result<f64> {
        self.usb.set_i2c_repeater(true)?;
        let actual = self.tuner.set_freq(&self.usb, hz as u32);
        self.usb.set_i2c_repeater(false)?;
        let actual = actual?;
        self.freq_hz = actual;
        Ok(actual)
    }

    /// Tune. The R82xx has no DSP stage after the LO, so `dsp` must be
    /// `Auto`/`Default`; the reported `target_dsp_freq` carries whatever
    /// residual the application's own mixer must absorb.
    pub fn set_freq(&mut self, req: &TuneRequest) -> Result<TuneResult> {
        let rf_target = match req.rf {
            TunePolicy::Manual(hz) => hz,
            TunePolicy::Auto | TunePolicy::Default => req.target_freq,
        };
        if let TunePolicy::Manual(x) = req.dsp
            && x != 0.0
        {
            return Err(Error::Unsupported {
                backend: "rtl",
                what: "manual DSP tune (the RTL has no post-LO DSP stage)".into(),
            });
        }
        let actual = self.retune(rf_target)?;
        Ok(TuneResult {
            clipped_rf_freq: rf_target,
            target_rf_freq: rf_target,
            actual_rf_freq: actual,
            target_dsp_freq: req.target_freq - actual,
            actual_dsp_freq: 0.0,
        })
    }

    /// The actual tuned frequency, derived from the PLL registers written
    /// (the tuner's RW registers cannot be read back — lock status can,
    /// see [`Rtl::tuner_status`]).
    pub fn freq(&self) -> Result<f64> {
        Ok(self.freq_hz)
    }

    /// Manual gain in dB on the TUNER scale (LNA + mixer), snapped to
    /// the nearest hardware step — the 0–~49.6 dB `rtl_sdr` convention.
    /// The VGA baseband stage is pinned at a fixed 16.3 dB and is NOT
    /// part of this number, so `set_gain(n)` and [`Rtl::gain`] report
    /// the same scale and round-trip. Use [`Rtl::set_gain_stages`] for
    /// direct three-stage control (VGA included) or [`Rtl::set_agc`]
    /// for hardware AGC.
    pub fn set_gain(&mut self, db: f64) -> Result<()> {
        self.usb.set_i2c_repeater(true)?;
        let r = self
            .tuner
            .set_gain_manual(&self.usb, (db * 10.0).round() as i32);
        self.usb.set_i2c_repeater(false)?;
        r.map(|_| ())
    }

    /// Live tuner gain in dB (LNA + mixer) from hardware readback of the
    /// active stage indices — the same scale [`Rtl::set_gain`] takes
    /// (valid under AGC too, where the hardware chose the indices). The
    /// fixed 16.3 dB VGA stage is deliberately excluded; read all three
    /// stages with [`Rtl::gain_stages`]. This used to add VGA in, so a
    /// `set_gain(31)` read back as ~47–61 — the "hardware answer at 61"
    /// that looked like the gain wasn't taking.
    pub fn gain(&self) -> Result<f64> {
        self.usb.set_i2c_repeater(true)?;
        let r = self.tuner.read_gains(&self.usb);
        self.usb.set_i2c_repeater(false)?;
        let (lna, mix, _vga) = r?;
        Ok((lna + mix) as f64 / 10.0)
    }

    /// Live per-stage gains (LNA, mixer, VGA) in dB — the diagnostic
    /// view [`Rtl::gain`] summarizes. VGA is the fixed baseband stage
    /// excluded from the manual-gain scale.
    pub fn gain_stages(&self) -> Result<(f64, f64, f64)> {
        self.usb.set_i2c_repeater(true)?;
        let r = self.tuner.read_gains(&self.usb);
        self.usb.set_i2c_repeater(false)?;
        let (lna, mix, vga) = r?;
        Ok((lna as f64 / 10.0, mix as f64 / 10.0, vga as f64 / 10.0))
    }

    /// Per-stage gain control by index (LNA 0–15, mixer 0–15, VGA 0–15 ≈
    /// 3.5 dB/step). Returns the snapped per-stage dB actually applied.
    pub fn set_gain_stages(&mut self, lna: u8, mixer: u8, vga: u8) -> Result<(f64, f64, f64)> {
        self.usb.set_i2c_repeater(true)?;
        let r = self.tuner.set_gain_stages(&self.usb, lna, mixer, vga);
        self.usb.set_i2c_repeater(false)?;
        let (l, m, v) = r?;
        Ok((l as f64 / 10.0, m as f64 / 10.0, v as f64 / 10.0))
    }

    /// The tuner's gain stages' levels in dB, index 0 first — LNA,
    /// mixer and VGA, 16 each. `None` for a stage the tuner lacks.
    pub fn stage_levels_db(stage: Stage) -> Option<Vec<f64>> {
        let [lna, mixer, vga] = R82xx::stage_levels_tenth_db();
        let table = match stage {
            Stage::Lna => lna,
            Stage::Mixer => mixer,
            Stage::Vga => vga,
            Stage::Amp | Stage::Pga => return None,
        };
        Some(table.iter().map(|&t| t as f64 / 10.0).collect())
    }

    /// Where each stage sits, read back from the tuner.
    pub fn stage_readings(&self) -> Result<Vec<StageReading>> {
        self.usb.set_i2c_repeater(true)?;
        let r = self.tuner.read_gain_indices(&self.usb);
        self.usb.set_i2c_repeater(false)?;
        let (lna, mixer, vga) = r?;
        Ok([(Stage::Lna, lna), (Stage::Mixer, mixer), (Stage::Vga, vga)]
            .into_iter()
            .map(|(stage, level)| {
                let levels = Self::stage_levels_db(stage).expect("an R82xx stage");
                StageReading { stage, level: level as usize, levels: levels.len(), db: levels[level as usize] }
            })
            .collect())
    }

    /// Set one stage to a level of [`Rtl::stage_levels_db`] (clamped),
    /// leaving the other two where the tuner has them. Manual mode: the
    /// tuner's LNA/mixer AGC is switched off by the write.
    pub fn set_stage_level(&mut self, stage: Stage, level: usize) -> Result<()> {
        let Some(levels) = Self::stage_levels_db(stage) else {
            return Err(Error::Device { backend: "rtl", context: "set_stage_level", message: format!("no {} stage", stage.key()) });
        };
        let level = level.min(levels.len() - 1) as u8;
        self.usb.set_i2c_repeater(true)?;
        let now = self.tuner.read_gain_indices(&self.usb);
        self.usb.set_i2c_repeater(false)?;
        let (mut lna, mut mixer, mut vga) = now?;
        match stage {
            Stage::Lna => lna = level,
            Stage::Mixer => mixer = level,
            Stage::Vga => vga = level,
            Stage::Amp | Stage::Pga => unreachable!("no levels, returned above"),
        }
        self.set_gain_stages(lna, mixer, vga).map(|_| ())
    }

    /// Tuner hardware AGC (LNA+mixer) and/or the demod's digital AGC.
    pub fn set_agc(&mut self, tuner_agc: bool, demod_agc: bool) -> Result<()> {
        if tuner_agc {
            self.usb.set_i2c_repeater(true)?;
            let r = self.tuner.set_gain_auto(&self.usb);
            self.usb.set_i2c_repeater(false)?;
            r?;
        }
        self.usb.set_demod_agc(demod_agc)?;
        Ok(())
    }

    /// Tuner IF filter bandwidth in Hz (0 = track the sample rate).
    pub fn set_bandwidth(&mut self, bw_hz: f64) -> Result<()> {
        self.bw_hz = bw_hz as u32;
        let bw = if self.bw_hz > 0 {
            self.bw_hz
        } else if self.rate_hz > 0.0 {
            self.rate_hz as u32
        } else {
            return Ok(()); // applied when the rate is set
        };
        self.usb.set_i2c_repeater(true)?;
        let if_freq = self.tuner.set_bandwidth(&self.usb, bw);
        self.usb.set_i2c_repeater(false)?;
        self.usb.set_if_freq(if_freq?)?;
        if self.freq_hz > 0.0 {
            self.retune(self.freq_hz)?;
        }
        Ok(())
    }

    /// The RTL2832's built-in test mode replaces ADC data with an 8-bit
    /// counter — a bit-exactness test for the whole USB path with no
    /// antenna involved.
    pub fn set_test_mode(&mut self, on: bool) -> Result<()> {
        self.usb.set_test_mode(on)
    }

    /// Bias tee on GPIO0 (powers LNAs over the coax; V3/V4 hardware).
    pub fn set_bias_tee(&mut self, on: bool) -> Result<()> {
        self.usb.set_gpio(0, on)
    }

    /// Everything the tuner can report about its actual state.
    pub fn tuner_status(&mut self) -> Result<RtlTunerStatus> {
        self.usb.set_i2c_repeater(true)?;
        let locked = self.tuner.pll_locked(&self.usb);
        let gains = self.tuner.read_gains(&self.usb);
        self.usb.set_i2c_repeater(false)?;
        let (lna, mix, vga) = gains?;
        Ok(RtlTunerStatus {
            actual_freq_hz: self.freq_hz,
            pll_locked: locked?,
            lna_gain_db: lna as f64 / 10.0,
            mixer_gain_db: mix as f64 / 10.0,
            vga_gain_db: vga as f64 / 10.0,
            if_freq_hz: self.tuner.if_freq_hz as f64,
            xtal_hz: self.tuner.xtal_hz as f64,
            tuner: self.tuner_kind(),
            pll: self.tuner.pll,
        })
    }

    /// Build the RX stream. The endpoint FIFO reset happens inside the
    /// stream's own `start()`, back to back with the queue priming —
    /// readiness is declared only when the whole path exists.
    pub fn rx_stream(&mut self, transfer_bytes: usize, queue_depth: usize) -> Result<RtlStream> {
        if self.rate_hz <= 0.0 {
            return Err(Error::Device {
                backend: "rtl",
                context: "rx_stream",
                message: "set the sample rate before streaming".into(),
            });
        }
        let endpoint = self
            .usb
            .interface
            .endpoint::<nusb::transfer::Bulk, nusb::transfer::In>(EP_BULK_IN)
            .map_err(|e| Error::Usb {
                backend: "rtl",
                context: "claim bulk endpoint",
                message: e.to_string(),
            })?;
        let transfer_bytes = if transfer_bytes > 0 {
            transfer_bytes
        } else {
            stream::DEFAULT_TRANSFER_BYTES
        };
        let queue_depth = if queue_depth > 0 {
            queue_depth
        } else {
            stream::DEFAULT_QUEUE_DEPTH
        };
        Ok(RtlStream::new(
            self.usb.device.clone(),
            endpoint,
            transfer_bytes,
            queue_depth,
            self.rate_hz,
        ))
    }
}

impl Drop for Rtl {
    fn drop(&mut self) {
        // Put the tuner in standby and power the demod down — the dongle
        // otherwise keeps drawing full RX current forever. Best-effort.
        let _ = self.usb.set_i2c_repeater(true);
        let _ = self.tuner.standby(&self.usb);
        let _ = self.usb.set_i2c_repeater(false);
        let _ = self.usb.power_down();
    }
}

/// Enumerate attached RTL dongles without opening any, one
/// [`sdr_core::DeviceDesc`] each (`rtl:serial=…`, or `rtl:<index>` for a
/// dongle with no serial).
pub fn list() -> Vec<sdr_core::DeviceDesc> {
    let Ok(devices) = nusb::list_devices().wait() else {
        return Vec::new();
    };
    devices
        .filter(|d| {
            KNOWN_IDS
                .iter()
                .any(|(vid, pid, _)| d.vendor_id() == *vid && d.product_id() == *pid)
        })
        .enumerate()
        .map(|(i, d)| sdr_core::DeviceDesc {
            spec: match d.serial_number() {
                Some(sn) => format!("rtl:serial={sn}"),
                None => format!("rtl:{i}"),
            },
            label: format!(
                "{} {} (serial {})",
                d.manufacturer_string().unwrap_or("?"),
                d.product_string().unwrap_or("RTL2832U"),
                d.serial_number().unwrap_or("?"),
            ),
            backend: sdr_core::Backend::Rtl,
        })
        .collect()
}
