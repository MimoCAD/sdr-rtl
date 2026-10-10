//! sdr-rtl in a browser over WebUSB: the proof page's two actions.
//!
//! `stream_check`: ask for a dongle, read its EEPROM, stream 2.4 Msps for
//! four seconds — the reading half on this task reading the wire bytes,
//! the control on another task retuning three times — then reunite and
//! finish; the report is the chip's own count of the blocks it dropped.
//!
//! `eeprom_round_trip`: one free byte past the strings set to 0x55 and back
//! to what it was, each write read back whole, and the part compared with
//! what it held before (`examples/eepromcheck.rs`, in a browser).
//!
//! Both log each step through the page's `log(text)` and resolve to a
//! one-line JSON summary. Opened as `?auto=stream` (or `?auto=eeprom`) the
//! page runs one by itself — the dongle must already be granted, since
//! only a click may open the chooser — taking `transfer`, `depth`,
//! `seconds` and `retunes` from the query, and sends each line to the
//! server (`POST /log`).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use js_sys::Function;
use sdr_core::TuneRequest;
use sdr_core::time::{Instant, sleep};
use sdr_rtl::{Control, Eeprom, Rtl, Stream};
use wasm_bindgen::prelude::*;

const RATE_SPS: f64 = 2.4e6;
const START_HZ: f64 = 300e6;
const STEP_HZ: f64 = 100e3;
const READ_FOR: Duration = Duration::from_secs(4);
const READ_BYTES: usize = 16384;
const PROBE_VALUE: u8 = 0x55;

fn say(log: &Function, text: &str) {
    let _ = log.call1(&JsValue::NULL, &JsValue::from_str(text));
}

fn js(e: impl ToString) -> JsValue {
    JsValue::from_str(&e.to_string())
}

/// The dongle the page was granted, or the chooser (needs the click that
/// called us).
async fn dongle(log: &Function) -> Result<Rtl, JsValue> {
    let rtl = match Rtl::open("").await {
        Ok(r) => r,
        Err(_) => {
            say(log, "asking for a dongle (WebUSB chooser)…");
            Rtl::request().await.map_err(js)?
        }
    };
    say(log, &format!("opened: {}", rtl.label()));
    Ok(rtl)
}

/// `transfer_bytes` and `depth` 0 for the driver's defaults; `seconds` 0
/// for the page's 4 s; `retunes` evenly spaced (0: none).
#[wasm_bindgen]
pub async fn stream_check(log: Function, transfer_bytes: usize, depth: usize, seconds: f64, retunes: u32) -> Result<JsValue, JsValue> {
    let read_for = if seconds > 0.0 { Duration::from_secs_f64(seconds) } else { READ_FOR };
    let mut rtl = dongle(&log).await?;
    let id = rtl.read_eeprom().await.map_err(js)?.identity().map_err(js)?;
    say(&log, &format!("EEPROM: {:04x}:{:04x} · {:?} / {:?} / serial of {} characters", id.vid, id.pid, id.manufacturer, id.product, id.serial.chars().count()));
    rtl.set_rate(RATE_SPS).await.map_err(js)?;
    rtl.set_freq(&TuneRequest::auto(START_HZ)).await.map_err(js)?;
    let (mut reader, control) = rtl.into_stream(transfer_bytes, depth).await.map_err(|e| js(e.error))?.split();
    say(&log, &format!("streaming {:.1} Msps at {:.1} MHz for {:.1} s (transfers {transfer_bytes} B × {depth}, 0 = default); retuning {retunes} times from another task", RATE_SPS / 1e6, START_HZ / 1e6, read_for.as_secs_f64()));

    // The control on its own task, retunes evenly spaced.
    let back: Rc<RefCell<Option<Result<Control, String>>>> = Rc::new(RefCell::new(None));
    let slot = Rc::clone(&back);
    let tasks_log = log.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let mut r = Ok(());
        for k in 0..retunes {
            sleep(read_for / (retunes + 1)).await;
            let hz = START_HZ + STEP_HZ * ((k + 1) % 2) as f64;
            match control.retune(&TuneRequest::auto(hz)).await {
                Ok((t, b)) => say(&tasks_log, &format!("retune {k}: {:.6} MHz from sample {} ({} block(s) before)", t.actual_rf_freq / 1e6, b.settled_at, b.dropped_before.blocks)),
                Err(e) => {
                    r = Err(e.to_string());
                    break;
                }
            }
        }
        *slot.borrow_mut() = Some(r.map(|()| control));
    });

    // The reading half here, timed: reads, the longest wait for one, and
    // the bytes each second.
    let t = Instant::now();
    let mut buf = vec![0u8; READ_BYTES];
    let mut bytes = 0u64;
    let (mut reads, mut longest_ms) = (0u64, 0.0f64);
    let mut per_second: Vec<u64> = Vec::new();
    // Reads by size: ≤ 188 (a block), ≤ 512 (a packet), ≤ 4096, < 16384, = 16384.
    let mut sizes = [0u64; 5];
    while t.elapsed() < read_for {
        let r0 = Instant::now();
        let n = reader.read(&mut buf).await.map_err(js)? as u64;
        longest_ms = longest_ms.max(r0.elapsed().as_secs_f64() * 1e3);
        sizes[match n {
            0..=188 => 0,
            189..=512 => 1,
            513..=4096 => 2,
            4097..=16383 => 3,
            _ => 4,
        }] += 1;
        reads += 1;
        bytes += n;
        let s = t.elapsed().as_secs() as usize;
        if per_second.len() <= s {
            per_second.resize(s + 1, 0);
        }
        per_second[s] += n;
    }
    say(&log, &format!("read: {reads} reads, the longest {longest_ms:.1} ms; bytes per second {per_second:?} (2.4 Msps = 4,800,000); reads by size ≤188 / ≤512 / ≤4096 / <16384 / 16384: {sizes:?}"));
    let control = loop {
        if let Some(r) = back.borrow_mut().take() {
            break r.map_err(js)?;
        }
        sleep(Duration::from_millis(10)).await;
    };
    let seconds = t.elapsed().as_secs_f64();
    let breaks = control.breaks();
    let stream = Stream::reunite(reader, control).map_err(|_| js("the halves did not reunite"))?;
    let fin = stream.finish().await.map_err(js)?;
    let r = &fin.report;
    let summary = format!(
        "{{\"rate\":{RATE_SPS},\"transfer_bytes\":{transfer_bytes},\"depth\":{depth},\"seconds\":{seconds:.3},\"bytes_read\":{bytes},\"reads\":{reads},\"longest_read_ms\":{longest_ms:.1},\"samples\":{},\"dropped_blocks\":{},\"saturated\":{},\"stall_retries\":{},\"breaks\":{},\"break_dropped\":{:?}}}",
        r.samples,
        r.dropped_blocks,
        r.saturated,
        r.stall_retries,
        breaks.len(),
        breaks.iter().map(|b| b.dropped_before.blocks).collect::<Vec<_>>()
    );
    say(&log, &format!("finished: {} samples, {} block(s) dropped by the chip", r.samples, r.dropped_blocks));
    Ok(JsValue::from_str(&summary))
}

#[wasm_bindgen]
pub async fn eeprom_round_trip(log: Function) -> Result<JsValue, JsValue> {
    let mut rtl = dongle(&log).await?;
    let before = rtl.read_eeprom().await.map_err(js)?;
    let end = before.strings_end().map_err(js)?;
    let Some(addr) = (end..before.size()).find(|&i| before.bytes()[i] == 0) else {
        return Err(js("no free zero byte past the strings to test with"));
    };
    say(&log, &format!("{}-byte part; strings end at {end:#04x}; probing {addr:#04x}", before.size()));
    let mut probe = before.bytes().to_vec();
    probe[addr] = PROBE_VALUE;
    let wrote = rtl.write_eeprom(&Eeprom::from_bytes(probe).map_err(js)?).await.map_err(js)?;
    let mid = rtl.read_eeprom().await.map_err(js)?.bytes()[addr];
    say(&log, &format!("wrote {PROBE_VALUE:#04x} at {addr:#04x} ({} page(s)); reads {mid:#04x}", wrote.chunks.len()));
    rtl.write_eeprom(&before).await.map_err(js)?;
    let after = rtl.read_eeprom().await.map_err(js)?;
    let same = after.bytes() == before.bytes();
    say(&log, &format!("restored; the whole part as before: {same}"));
    Ok(JsValue::from_str(&format!("{{\"probe\":{addr},\"read_back\":{mid},\"whole_part_as_before\":{same}}}")))
}
