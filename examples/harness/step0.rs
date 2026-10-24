//! Step 0: prove the way back. The dongle must disappear from the bus when
//! the analyzer cuts TARGET-A power and reappear when it restores it, or no
//! register is written.

use crate::json::J;
use crate::usb;

pub const CYCLES: usize = 3;

pub fn run() -> Result<(), String> {
    let mut rec = crate::header(
        "0",
        "power-cycle recovery: device count before, after vbus off, after vbus on",
    );
    rec.set("candidate_space", "none (a procedure check)");
    rec.set("cycles", CYCLES);
    // Refuse unless exactly one accepted device is attached.
    let info = usb::guard()?;
    rec.set(
        "device_before",
        J::obj()
            .with("vid", crate::json::hex16(info.vendor_id()))
            .with("pid", crate::json::hex16(info.product_id())),
    );
    let mut cycles = J::Arr(Vec::new());
    let mut ok = true;
    for n in 0..CYCLES {
        match usb::power_cycle() {
            Ok(r) => cycles.push(r.with("cycle", n)),
            Err(e) => {
                cycles.push(J::obj().with("cycle", n).with("error", e));
                ok = false;
                break;
            }
        }
    }
    rec.set("results", cycles);
    // After the last cycle the device must open and answer a standard request.
    let reopened = usb::Rtl::open().map(|r| r.descriptor_record());
    match reopened {
        Ok(d) => {
            rec.set("descriptor_after", d);
        }
        Err(e) => {
            rec.set("reopen_error", e);
            ok = false;
        }
    }
    rec.set("recovery_works", ok);
    crate::write_record("step0-recovery.json", &rec);
    if ok {
        Ok(())
    } else {
        Err("recovery did not work; stop".into())
    }
}
