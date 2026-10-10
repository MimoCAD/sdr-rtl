//! USB_EPA_FIFO_CFG[31:24], the chip's count of 188-byte blocks dropped
//! with the endpoint FIFO full ([RTL2832U] Table 34, p. 40: "Counts
//! blocks dropped (each is 188 bytes) due to full FIFO"; documents.md):
//! when does it count in the raw-sample stream, and what clears it?
//!
//!   dropcounter
//!
//! Built with `--features bench`. Reading the register (4 bytes at USB
//! block 0x160, little-endian, the counter the fourth byte) at each step:
//! 1. after a power cycle (TARGET A through the analyzer) and `Rtl::open`;
//! 2. while the stream runs at 2.4 Msps and is read without pause: at
//!    0.05, 0.2, 0.5, 1, 2, 4 and 8 s after the start;
//! 3. a 2 s stall (the stream left unread; the host queue holds ~55 ms);
//! 4. the stream stopped (page 0 0x06 bit 7 cleared, nothing can count),
//!    read twice 100 ms apart;
//! 5. the clears, each read back, stopping at the first that leaves 0:
//!    (a) one byte, 0, at 0x163 (the counter's own byte); (b) the four
//!    bytes as read with the counter byte 0; (c) USB_EPA_CTL (0x148) bit 9,
//!    "FIFO reset" (documents.md), set then cleared — the write the vendor
//!    driver makes at its USB set-up (findings 9b: 0x0210 then 0), here as
//!    the register read with bit 9 set, then as read;
//! 6. the stream started again (the start bit alone) and read at 0.05,
//!    0.5 and 2 s.
//!
//! USB block offset 0x1C is never touched (derivation/HAZARD-usb-0x1C.md):
//! every write here is at 0x148-0x149 or 0x160-0x163. One JSON line per
//! reading.

mod bench;

use std::time::{Duration, Instant};

use bench::*;
use sdr_core::{Iq, RxSource};
use sdr_rtl::protocol::{ACCESS_GET, ACCESS_SET, BLOCK_USB, windex, wvalue_usb};
use sdr_rtl::{Rtl, RtlStream};

const EPA_FIFO_CFG: u16 = 0x160;
const COUNTER_BYTE: usize = 3;
const EPA_CTL: u16 = 0x148;
/// USB_EPA_CTL bit 9, FIFO reset (documents.md, [RTL2832U] Table 32).
const EPA_CTL_FIFO_RESET: u16 = 1 << 9;
const RATE_SPS: f64 = 2.4e6;
const STALL: Duration = Duration::from_secs(2);
const MARKS_S: [f64; 7] = [0.05, 0.2, 0.5, 1.0, 2.0, 4.0, 8.0];
const RESTART_MARKS_S: [f64; 3] = [0.05, 0.5, 2.0];

fn usb_read(rtl: &Rtl, off: u16, len: u16) -> Result<Vec<u8>, String> {
    rtl.link().get(wvalue_usb(off), windex(BLOCK_USB, ACCESS_GET, 0), len, "USB block read").map_err(|e| e.to_string())
}

fn usb_write(rtl: &Rtl, off: u16, data: &[u8]) -> Result<(), String> {
    let end = off + data.len() as u16;
    assert!((off >= EPA_FIFO_CFG && end <= EPA_FIFO_CFG + 4) || (off >= EPA_CTL && end <= EPA_CTL + 2), "writes stay inside USB_EPA_FIFO_CFG and USB_EPA_CTL");
    rtl.link().set(wvalue_usb(off), windex(BLOCK_USB, ACCESS_SET, 0), data, "USB block write").map_err(|e| e.to_string())
}

fn counter(rtl: &Rtl) -> Result<(u8, Vec<u8>), String> {
    let v = usb_read(rtl, EPA_FIFO_CFG, 4)?;
    Ok((v[COUNTER_BYTE], v))
}

fn emit(step: &str, rtl: &Rtl, extra: &str) -> Result<u8, String> {
    let (c, v) = counter(rtl)?;
    println!("{{\"step\":\"{step}\",\"fifo_cfg\":{v:?},\"counter\":{c}{extra}}}");
    Ok(c)
}

/// Read the stream without pause, the counter at each mark.
fn marks(rtl: &Rtl, s: &mut RtlStream, buf: &mut [Iq], label: &str, at: &[f64]) -> Result<(), String> {
    let t = Instant::now();
    let (mut n, mut overflows) = (0usize, 0usize);
    for &m in at {
        while t.elapsed().as_secs_f64() < m {
            let info = s.recv_into(buf, RECV_TIMEOUT_S).map_err(|e| e.to_string())?;
            n += info.n_samples;
            overflows += info.overflow as usize;
        }
        emit(&format!("{label} {m} s"), rtl, &format!(",\"samples\":{n},\"overflows\":{overflows}"))?;
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let cycle = power_cycle()?;
    let mut rtl = Rtl::open("").map_err(|e| e.to_string())?;
    emit("after power cycle and open", &rtl, &format!(",\"cycle\":\"{cycle}\""))?;
    rtl.set_rate(RATE_SPS).map_err(|e| e.to_string())?;
    let mut s = rtl.rx_stream(TRANSFER_BYTES, QUEUE_DEPTH).map_err(|e| e.to_string())?;
    let mut buf = vec![Iq::default(); s.max_num_samps()];
    s.start().map_err(|e| e.to_string())?;
    marks(&rtl, &mut s, &mut buf, "streaming, read without pause, at", &MARKS_S)?;
    std::thread::sleep(STALL);
    emit("after a 2 s stall", &rtl, "")?;
    s.stop().map_err(|e| e.to_string())?;
    emit("stream stopped", &rtl, "")?;
    std::thread::sleep(Duration::from_millis(100));
    let frozen = emit("stream stopped, 100 ms later", &rtl, "")?;
    let mut cleared = frozen == 0;
    if !cleared {
        usb_write(&rtl, EPA_FIFO_CFG + COUNTER_BYTE as u16, &[0])?;
        cleared = emit("(a) after 0 written to the counter byte 0x163", &rtl, "")? == 0;
    }
    if !cleared {
        let (_, v) = counter(&rtl)?;
        usb_write(&rtl, EPA_FIFO_CFG, &[v[0], v[1], v[2], 0])?;
        cleared = emit("(b) after the register written back with the counter byte 0", &rtl, &format!(",\"wrote\":[{},{},{},0]", v[0], v[1], v[2]))? == 0;
    }
    if !cleared {
        let ctl = usb_read(&rtl, EPA_CTL, 2)?;
        let was = u16::from_le_bytes([ctl[0], ctl[1]]);
        usb_write(&rtl, EPA_CTL, &(was | EPA_CTL_FIFO_RESET).to_le_bytes())?;
        emit("(c) USB_EPA_CTL bit 9 set", &rtl, &format!(",\"epa_ctl_was\":{was}"))?;
        usb_write(&rtl, EPA_CTL, &was.to_le_bytes())?;
        let now = usb_read(&rtl, EPA_CTL, 2)?;
        emit("(c) USB_EPA_CTL restored", &rtl, &format!(",\"epa_ctl_now\":{}", u16::from_le_bytes([now[0], now[1]])))?;
    }
    s.start().map_err(|e| e.to_string())?;
    marks(&rtl, &mut s, &mut buf, "restarted, read without pause, at", &RESTART_MARKS_S)?;
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("dropcounter: {e}");
        std::process::exit(1);
    }
}
