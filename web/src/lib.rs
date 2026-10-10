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
//! one-line JSON summary.

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
const RETUNES: u32 = 3;
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

#[wasm_bindgen]
pub async fn stream_check(log: Function) -> Result<JsValue, JsValue> {
    let mut rtl = dongle(&log).await?;
    let id = rtl.read_eeprom().await.map_err(js)?.identity().map_err(js)?;
    say(&log, &format!("EEPROM: {:04x}:{:04x} · {:?} / {:?} / serial of {} characters", id.vid, id.pid, id.manufacturer, id.product, id.serial.chars().count()));
    rtl.set_rate(RATE_SPS).await.map_err(js)?;
    rtl.set_freq(&TuneRequest::auto(START_HZ)).await.map_err(js)?;
    let (mut reader, control) = rtl.into_stream(0, 0).await.map_err(|e| js(e.error))?.split();
    say(&log, &format!("streaming {:.1} Msps at {:.1} MHz; retuning {RETUNES} times from another task", RATE_SPS / 1e6, START_HZ / 1e6));

    // The control on its own task: a retune every second.
    let back: Rc<RefCell<Option<Result<Control, String>>>> = Rc::new(RefCell::new(None));
    let slot = Rc::clone(&back);
    let tasks_log = log.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let mut r = Ok(());
        for k in 0..RETUNES {
            sleep(READ_FOR / (RETUNES + 1)).await;
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

    // The reading half here.
    let t = Instant::now();
    let mut buf = vec![0u8; READ_BYTES];
    let mut bytes = 0u64;
    while t.elapsed() < READ_FOR {
        bytes += reader.read(&mut buf).await.map_err(js)? as u64;
    }
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
        "{{\"rate\":{RATE_SPS},\"seconds\":{seconds:.3},\"bytes_read\":{bytes},\"samples\":{},\"dropped_blocks\":{},\"saturated\":{},\"stall_retries\":{},\"breaks\":{},\"break_dropped\":{:?}}}",
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
