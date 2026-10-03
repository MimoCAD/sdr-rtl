# Bench findings

Replay: `cargo run --release -p sdr-rtl --example harness -- <step>`

Status: recovery proven; 1A/1B ran read-only; 1C onward not yet run.

## Step 0 — recovery: PROVEN — `data/step0-recovery.json` — `harness recovery`

3/3 power cycles: the dongle is gone within ~60 ms of power-off and
enumerates again within ~530 ms of power-on (now as 0bda:2832).

## Step 1A — system/USB: wValue = base+offset, little-endian — `data/step1-readonly-sys-usb-encoding.json` — `harness protocol-readonly`

Candidates: base+offset, offset, offset<<8, offset<<8|base>>8,
swap(base+offset). base+offset matches DEMOD_CTL 0x20, GPO 0x18, GPOE
0x19, GP_CFG0 0x96, GP_CFG1 0x55 (5/6); the other forms score 0.
I2CMSTR reads `10 09 93 00` = 0x00930910 little-endian; byte reads
inside the 32-bit registers work.

Mismatches: GPD 0x06 (doc 0x0E), I2CCR 0x17 (0x13), I2CMCR 0x28
(0x013A0000), USB_SYSCTL 0x09 (0x01), EPA_CFG 0xD1 (0x40), EPA_CTL 0x0210
(0), EPA_MAXPKT 0x0200 (0x40), EPA_FIFO_CFG 0x14 (0xA). The dongle had
been opened and streamed by another driver earlier in the same power
session, so these are that driver's leftovers, not reset values; a power
cycle is needed before reading reset values.

Descriptor: high speed; interface 0 bulk IN 0x81, max packet 512;
interface 1 has no endpoints.

## Step 1B — demodulator: no answer as found (DEMOD_CTL 0x20) — `data/step1-readonly-demod-encoding.json`

512 forms (offset|k<<8, offset<<8|k for k in 0..=255) × 101 documented
reset bytes (59 non-zero): every read stalls except wValue low byte
0xA0/0xA1, which reads the configuration EEPROM (M24C02, Fig. 9;
contents never recorded, marked RR). Best score 3/59.

Block 0 with wValue (offset<<8)|i2c_address is a byte-addressed I²C read.

Next: `harness protocol` (power cycle, documented PLL enable DEMOD_CTL
bit 7, repeat the search, verify writes).

## Not yet run

1C/1D (`protocol`), 2 (`tuner`), 3 (`baseline`, `scan`, `confirm`).
