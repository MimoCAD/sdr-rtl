# The RTL2832U configuration EEPROM — what the firmware reads, derived from the ROM

Clean-room derivation, 2026-10-10. Sources: the firmware image
(`~/mimosdr-captures/rtl2832u-rom/rtl2832u-rom.bin`, SHA-256 per
`derivation/findings.md` Step 5a), its listing by our own
disassembler (`examples/rom51 … --inline 0x120B:4 --xref`,
`listing.txt` beside the image), `derivation/documents.md` and
`findings.md` Step 5. No other driver, tool or web page was read. Every fact is
**code** (read from the disassembly) unless marked otherwise; nothing here was
measured on a bench, and no EEPROM was read.

Notation as in findings.md: plain hex addresses are ROM (code) addresses; `X:`
is the 8051's external-data space as the code addresses it (8051 view: each
32-bit register word stored most-significant byte first, so host register
`H` is `X:(H & ~3) | (3 − (H & 3))` — findings 5c, bench-confirmed). `E[n]`
is EEPROM byte `n`. The EEPROM is I²C slave 0xA0 (8-bit form), the only fixed
I²C address in the ROM (0x0FB8, 0x16F6, 0x1756); M24C02 per documents.md
(Fig. 9).

Recognition note: the layout below matches an RTL-SDR EEPROM layout I have
seen described elsewhere (a 0x28 0x32 header, VID/PID, an 0xA5 serial flag, a
configuration byte, string descriptors from offset 9). None of that memory was
used; each item is traced to the ROM addresses given.

---

## 1. The routines and helpers

| ROM | what it is | evidence |
|---|---|---|
| 0x1C13 | main: 0x1C98 timers, 0x1C59 I²C master, **0x0A7D EEPROM configuration**, 0x1895 USB initialisation (which first runs **0x16D6 patch list**), **0x1AF0 GPIO set-up**, then the request loop 0x1C22 | calls at 0x1C13–0x1C1F |
| 0x0FB6 / 0x0FBA | I²C read. 0x0FB6 sets slave = 0xA0 (0x0FB8) and register = idata 25h, then falls into 0x0FBA: R7 slave, R5 register, R3 length, destination = generic pointer at idata 34h–36h, idata 37h = "no register byte" flag. With the flag clear it loads the master FIFO (I2CMFR, X:3050) with slave & 0xFE, the register, slave \| 1 (0x1025–0x1053) and then reads R3 bytes out of X:3053 (0x10F1–0x111F). On the error status (X:304F bit 2, 0x10BF–0x10D6) it retries; 5 attempts (idata 3Ch = 5, 0x0FDA; 0x1123–0x112C); returns carry set on success | listing 0x0FB6–0x1130 |
| 0x127C | I²C write: R7 slave, R5 register, R3 length, source = generic pointer at idata 33h–35h, idata 36h = "no register byte" flag; sends slave & 0xFE, the register (unless flagged, 0x12D8), then the bytes (0x1308–0x1338); 5 attempts on the error status (idata 3Bh, 0x1290, 0x13B6–0x13C3), **no delay between attempts** | listing 0x127C–0x13C3 |
| 0x1131 / 0x114A | generic-pointer byte read (R3 = 0x01 → X:, below → idata, 0xFE → paged X:, else code), 0x114A at pointer + DPTR | listing |
| internal RAM | idata 22h–24h = generic pointer 01:40:90 = **X:4090**, the scratch buffer the EEPROM is read into (0x0A7D–0x0A83) | |

Constraints these impose on every EEPROM read: one I²C transaction moves at most
16 bytes (callers cap at 0x10: 0x0C5E–0x0C69, 0x0CF6–0x0D01; the patch list
caps at 15, 0x1730–0x173B); the register (EEPROM offset) is one byte.

---

## 2. Validity: how the firmware decides the EEPROM is used

At 0x0A7D the firmware first copies an 18-byte device descriptor from ROM
0x0DD0 into idata 09h–1Ah (loop 0x0A8D–0x0A9F) — the descriptor it will serve
(GET_DESCRIPTOR device returns idata 09h, length idata 09h, 0x009F–0x00BA).
Then:

1. **Read `E[0x00..0x0D]`** (14 bytes, R3 = 0x0E at 0x0AB0, register idata 25h
   = 0 at 0x0AA3) into X:4090–X:409D (0x0AB2). **I²C failure** (no EEPROM, 5
   NACKed attempts) → carry clear → **defaults** (0x0AB5 → 0x0D52).
2. **Test record check** (§6): `E[0] == 0x12` **or** `E[1] == 0x34`
   (0x0AC0–0x0AD1) → the test-record actions, then on to step 3.
3. **Signature**: `E[0] == 0x28` (0x0BAB–0x0BB2) **and** `E[1] == 0x32`
   (0x0BB5–0x0BBF); either mismatch → **defaults** (LJMP 0x0D52). A test
   record (E[0] = 0x12, or E[1] = 0x34) can never pass this, so a test-record
   EEPROM always ends in the defaults.
4. String length limits (§3): `L1 + L2 ≤ 70` (0x0C47–0x0C4E, an 8-bit sum —
   see open item 9) and, with a serial, `L3 ≤ 40` (0x0CE5–0x0CEA); otherwise
   **defaults** for all three strings. Any I²C failure during the string reads
   → defaults (0x0C2C, 0x0C6E, 0x0CD8, 0x0D06).

There is **no checksum** over the EEPROM's own contents, and nothing checks a
string descriptor's type byte or that its length is even.

The patch list (§5) has its own independent marker (0x5C at `E[0x4E]`) and does
**not** depend on the 0x28 0x32 signature: 0x16D6 never looks at `E[0..1]`.

---

## 3. The layout

| offset | size | meaning | read at | what the firmware does with it |
|---|---|---|---|---|
| 0x00 | 1 | signature, must be **0x28** | 0x0BAB | §2 step 3 |
| 0x01 | 1 | signature, must be **0x32** | 0x0BB5 | §2 step 3 |
| 0x02–0x03 | 2 | **idVendor**, little-endian (descriptor byte order) | loop 0x0BC2–0x0BE9 (from X:4092) | copied into idata 11h–12h = device-descriptor offsets 8–9 |
| 0x04–0x05 | 2 | **idProduct**, little-endian | same loop | idata 13h–14h = descriptor offsets 10–11 |
| 0x06 | 1 | **serial flag**: 0xA5 = a serial string follows | 0x0BEC–0x0BFE (X:4096) | 0xA5 → idata 19h (iSerialNumber) ← 3 and a "serial present" flag (idata 2Fh) ← 1; any other value → iSerialNumber ← 0, no serial read |
| 0x07 | 1 | **configuration bits** (table below) | 0x0C0C–0x0C10 (X:4097) | whole byte → idata 20h (bit-addressable 20h.0–20h.7) |
| 0x08 | 1 | not read in a valid record (used only by a test record, §6) | — | — |
| 0x09 | L1 | **manufacturer string descriptor**, stored whole: byte 0x09 is its bLength = L1, then bDescriptorType and the characters | L1 at 0x0C15–0x0C3C (first pass); bytes 0x0C51–0x0C84 | copied (with the product string) into X:4010; served verbatim as string index 1 (0x01AB–0x01C9), length = its first byte |
| 0x09+L1 | L2 | **product string descriptor**, same form | L2 at the second pass of 0x0C15; bytes with the above | follows the manufacturer at X:4010+L1; served as string index 2, located by X:4010's first byte (0x01CC–0x01FD) |
| 0x09+L1+L2 | L3 | **serial string descriptor**, same form, only if `E[6] == 0xA5`; L3 ≤ 40 | L3 at 0x0CC1–0x0CEA; bytes 0x0CEC–0x0D19 | copied to X:4060 (0x0D1B–0x0D50); served as string index 3 (0x0200–0x021E) |
| 0x4E | 1 | **patch-list marker**, 0x5C | 0x16F8–0x170B | anything else → no patch list |
| 0x4F | 1 | patch-list **length N** in bytes, capped at 178 | 0x1710–0x171F | §5 |
| 0x50… | N | patch records, 3 bytes each: X: address high, X: address low, value | 0x1726–0x17BB | each value written to that 8051 X: address (§5) |

All other bytes are never read by the firmware. Bytes 0x09–0x0D are read in
step 1's 14-byte block but used only from the later reads.

Encoding of the strings: the firmware serves the EEPROM's bytes as they are, so
the EEPROM must hold complete USB string descriptors (bLength,
bDescriptorType 0x03, then the string in the USB descriptor encoding, UTF-16LE
per the USB 2.0 specification). The ROM's own defaults (0x0E40, 0x0E50, 0x0E62)
are in exactly that form. The firmware itself checks neither the type nor the
encoding.

Overlap: strings start at 0x09 and may total 70 + 40 = 110 bytes (to 0x76), but
the patch list sits at 0x4E. A strings block longer than 0x4E − 0x09 = 69 bytes
reaches 0x4E; if 0x4E then happens to hold 0x5C, the following string bytes are
run as patch records. Keep the strings within 0x09–0x4D when a patch list is
wanted (or make sure 0x4E ≠ 0x5C).

### Configuration byte `E[7]` (idata 20h)

| bit | read at | effect |
|---|---|---|
| 0 | 0x014F, 0x033B, 0x05A1 | configuration and other-speed-configuration descriptors' bmAttributes ← 0xA0 (bus powered + **remote wake-up**) instead of the ROM's 0x80 (writes X:4097 of the descriptor copy). SET_FEATURE(DEVICE_REMOTE_WAKEUP) is accepted (USB 0x2010 bit 2 set) only with this bit; without it → STALL (0x05A1–0x05BE). GET_STATUS reports remote wake-up from 0x2010 bit 2 (0x04FE–0x0511) |
| 1 | 0x006C, 0x0148, 0x0334 | configuration (and other-speed) descriptor: wTotalLength 34 and bNumInterfaces ← 2 (X:4094 ← 2) — adds the ROM table's **second interface** (interface 1, alternate 0, **no endpoints**, class/subclass/protocol 0xFF, iInterface 5); without the bit, wTotalLength 25 and one interface |
| 2 | 0x1AF7 | master switch of the GPIO set-up (0x1AF0). Set: USB 0x2010 bit 5 set (0x1B0A–0x1B1E); **GPIO3 made an output and driven high** — GPD (0x3004, X:3007) bit 3 cleared, GPOE (0x3003, X:3000) bit 3 set, GPO (0x3001, X:3002) bit 3 set (0x1B21–0x1B35). Clear: 0x2010 bit 5 cleared, and the write also zeroes the rest of that word above bit 7 (R4–R6 = 0, 0x1B37–0x1B48); GPIO3 made an input (GPD bit 3 set, GPOE bit 3 cleared, 0x1B4B–0x1B58). Bits 3 and 4 act only when bit 2 is set |
| 3 | 0x1AFA | (with bit 2) **DEMOD_CTL_1** (0x300B, X:3008) ← 0x01 (0x1AFD–0x1B02) |
| 4 | 0x1B03 | (with bits 2 and 3) DEMOD_CTL_1 read back and bit 1 cleared (0x1B06–0x1B09) |
| 5–7 | — | never referenced (the bit-reference cross-check: only 20h.0–20h.4 occur) |

documents.md: GPIO3 "doubles as tuner power ('default 1 = off')"; DEMOD_CTL_1
"Enable IR remote wakeup & low current XTL mode when suspended", no bit table.
GP_CFG0 (0x3007, X:3004) bit 7 is set unconditionally first (0x1AF0–0x1AF6).

---

## 4. What the device descriptor can and cannot carry from the EEPROM

From the EEPROM: idVendor, idProduct (offsets 8–11) and iSerialNumber (3 or
0). Fixed by ROM 0x0DD0 and **not changeable from the EEPROM**: bcdUSB 2.00,
class/subclass/protocol 0, bMaxPacketSize0 64, **bcdDevice 1.00**,
iManufacturer 1, iProduct 2, bNumConfigurations 1. iConfiguration (4),
iInterface (5), bMaxPower (500 mA, 0xFA) and the bulk endpoint 0x81 come from
the ROM configuration tables (0x0DE2 high speed, 0x0E1A full speed; other-speed
0x0F0C / 0x0F44, chosen by X:2017 bit 0 at 0x00D6 / 0x02C2). String index 0
(language ID table) is ROM 0x0E04 (LANGID 0x0409); indices 4–6 are ROM
strings (0x0E86, 0x0EA6, 0x0ECC); index 7 and up STALL (0x0165–0x016A).

---

## 5. The patch list (0x16D6–0x17BB)

Run at every USB initialisation: at boot (0x1895, called from 0x1C1C) **before**
the SIE reset and connect, and again whenever the host issues the block 7
re-enumeration request (0x0A6A LJMP 0x1895). 0x0A7D (descriptors) and 0x1AF0
(GPIO) are **not** re-run by block 7, so a changed VID/PID/string/config byte
takes effect only after a power cycle.

1. Read 2 bytes at `E[0x4E]` (0x16E4–0x16F8). I²C failure → return.
2. `E[0x4E] ≠ 0x5C` → return (0x1706–0x170D).
3. N = `E[0x4F]`; N > 178 → N = 178 (0x1710–0x171F). (178 = 256 − 0x4E: the
   cap counts from the marker, but data start at 0x50, so a list at the cap
   would read two bytes past offset 0xFF; what the EEPROM returns there is the
   M24C02's address roll-over, not in documents.md.)
4. From offset 0x50 (0x1722–0x1724): while N > 0, read c = min(N, 15) bytes
   (0x1730–0x1758; I²C failure → stop), N −= c, then for j = 0 … ⌊c/3⌋ − 1
   write byte [3j+2] to X: address ([3j] << 8) | [3j+1] (0x1766–0x17BB).
5. Leftover bytes of a chunk that is not a multiple of 3 are skipped (only the
   last chunk can be: every full chunk is 15 = 5 records). No terminator
   record: the length byte is the only end.

So a record is **(address high, address low, value)**, at most 59 complete
records (178 = 59 × 3 + 1), and the address is an **8051 X: address in the
8051's byte order** (for a host-documented register H in the USB/system blocks,
use `(H & ~3) | (3 − (H & 3))`; host DEMOD_CTL 0x3000 is X:3003). Any X:
address is accepted, including the data RAM X:4000–X:41FF that holds the
firmware's variables and descriptor copies. Registers that 0x1895 (USB block)
or 0x1AF0 (GPIO, run after it) write later overwrite a patch's value.

---

## 6. The test record (0x0AD4–0x0BA4)

Selected by `E[0] == 0x12` or `E[1] == 0x34` (0x0AC0–0x0AD1). It drives GPIO4
and GPIO0 and reports the ROM's CRC-16 (routine 0x1B5A over 0x0000–0x1FFF,
findings 5a):

- GPIO4 output, high: GPOE (0x3003) bit 4 set, GPD (0x3004) bit 4 cleared,
  GPO (0x3001) bit 4 set (0x0AD4–0x0AEE).
- Expected CRC: `E[4]` (high) : `E[5]` (low) if `E[2] == 0x56` and
  `E[3] == 0x78` (0x0AEF–0x0B1C); otherwise the ROM constant at 0x0B1E.
  CRC equal → GPIO4 low (GPO bit 4 cleared, 0x0B37–0x0B44).
- GPIO0 high (GPO bit 0, 0x0B45–0x0B4D).
- If `E[6] == 0x12` or `E[7] == 0x34`: an I²C write of the computed CRC, high
  byte then low byte, to **slave `E[8]`, register 0x09** (0x0B54–0x0B8D).
- Wait 2 timer-0 ticks (idata 40h:41h, decremented by 0x1CCE), GPIO0 low
  (0x0B90–0x0BA4); then the signature check, which fails → defaults.

A factory test hook, by its use. With `E[8] = 0xA0` it would write the CRC into
the EEPROM's own bytes 0x09–0x0A. Nothing here is useful to a driver; a driver
writing an EEPROM must keep `E[0] ≠ 0x12` and `E[1] ≠ 0x34`.

---

## 7. Blank or invalid EEPROM: the defaults (0x0D52–0x0DCF)

Reached on any failure in §2 (and with a test record):

- Serial: 36 bytes from ROM 0x0E62 → X:4060 (0x0D52–0x0D6D): a 17-character
  string, "77771111153705700".
- Manufacturer: ROM 0x0E40 (bLength 16) → X:4010 (0x0D70–0x0D97): "Generic".
- Product: ROM 0x0E50 (bLength 18) → X:4010+16 (0x0D99–0x0DC9): "RTL2832U".
- Configuration bits 0 and 2 **OR-ed** into idata 20h (0x0DCB–0x0DCD).
- Device descriptor: whatever idata 09h–1Ah holds — the ROM's (VID 0x0BDA,
  PID 0x2832, iSerialNumber 3) unless the failure came after §3's VID/PID and
  serial-flag copy.

So a **blank (erased, all 0xFF) or absent EEPROM** enumerates as 0x0BDA:0x2832,
"Generic" / "RTL2832U" / serial "77771111153705700", remote wake-up advertised,
one interface, GPIO3 driven high, USB 0x2010 bit 5 set, DEMOD_CTL_1 untouched
(bit 3 clear). idata 20h is 0 before this (reset clears idata 0x01–0x7F at
0x1A64–0x1A68, and the initialisation table at 0x1D0C sets idata 20h ← 0).

Partial failures mix the two: a valid header with an over-long string keeps the
EEPROM's VID/PID, serial flag and configuration byte (with bits 0 and 2 forced
on) but serves all three default strings (the defaults overwrite X:4010 and
X:4060 even if the manufacturer/product were already copied). A valid record
with `E[6] ≠ 0xA5` returns at 0x0CBE without touching X:4060 (iSerialNumber 0,
so the host has no reason to ask for index 3).

---

## 8. Host access to the EEPROM (vendor requests, findings 5c)

The host reaches the EEPROM through the same I²C master and the **same read
routine 0x0FBA** the boot uses (block 3's read path calls it at 0x17FC), so it
is on the bus the host's I²C requests drive.

- **Read**: block 3, Get — bmRequestType 0xC0, wIndex 0x0300, wValue =
  `offset << 8 | 0xA0`, wLength n. 0x0788 → 0x17BE splits each ≤ 64-byte data
  packet into I²C random reads of ≤ 16 bytes (slave+W, offset, slave+R, bytes),
  advancing the offset (an 8-bit register, idata 2Ah/28h; 0x1802–0x1806,
  0x07F0–0x07FA). Any n works; 256 in one request would be four 64-byte
  packets (findings 5c: packets ≤ 64, endpoint-0 wait of 100 ticks per packet).
- **Write**: block 3, Set — bmRequestType 0x40, wIndex 0x0310, wValue as above,
  data = the bytes. 0x0814 → 0x1935 splits each OUT packet into I²C writes of
  ≤ 16 bytes (slave+W, offset, data), the offset advancing by 16, **with no
  delay between the writes** and 5 immediate attempts each (0x127C). An EEPROM
  that NACKs during its internal write cycle will therefore fail every write
  after the first in one request → STALL (0x085E). So: **one ≤ 16-byte write
  per control transfer**, not crossing an EEPROM write page, and wait out the
  write cycle before the next transfer (or poll by a read until it ACKs). The
  M24C02's page size and write-cycle time are **not** in documents.md. On the
  bench part the page is **8 bytes**, measured (findings 17): a 16-byte write
  at 0x30 wrapped, its second half landing back on 0x30–0x37.
- **Block 6** (standard I²C, wIndex 0x0600 / 0x0610): wLength ≤ 16 (0x076B),
  no register byte (X:4000 = 1, 0x077C–0x0787): a Get is a current-address
  read; a Set sends the data as-is, so data[0] is the word address and at most
  15 data bytes follow.
- **Never use block 0 for 0xA0.** Block 0 first writes the page number (wIndex
  & 0x0F) to **register 0x00 of the addressed slave** whenever it differs from
  the one cached page (idata 08h, shared by all slaves; 0x072A–0x0769), and the
  cache is invalidated at every USB initialisation (0x1917) and after a failed
  page write (0x075F). Through block 0 at slave 0xA0 that is a write to
  **`E[0]`, the 0x28 signature**.
- After writing, the descriptors change only at the next power-on; block 7
  re-runs only the patch list (§5) — and is itself a disconnect (findings 5c,
  hazard).

The firmware never writes the EEPROM on its own: 0x127C's callers are block 0's
page write (slave from the host), the test record (slave from `E[8]`) and the
host's block 3/6 write.

---

## 9. Open — not determinable from the ROM

1. Whether the bench dongle's EEPROM is programmed and what it holds (never
   read; METHODS.md §7 forbids recording its contents).
2. The M24C02's capacity, write-page size, write-cycle time and address
   roll-over: documents.md names the part (Fig. 9) but records none of these;
   they need the M24C02 datasheet added to the document set.
3. The meaning of USB 0x2010 bit 5 (config bit 2), GP_CFG0 bit 7, and
   DEMOD_CTL_1 bits 0 and 1 (config bits 3/4): documents.md gives no bit tables.
4. What interface 1 (config bit 1, no endpoints) is for; the firmware serves no
   IR request (block 5 always STALLs, findings 5c), so nothing in the ROM uses
   it.
5. The timer-0 tick in seconds (reload 0xC568 = 15,000 counts, 0x1C9E/0x1CD4;
   the 8051's clock is not in the documents). findings 5a's ~84 ms for the
   100-tick endpoint-0 bound suggests about 1 ms — an inference, not a
   measurement; so the test record's GPIO0 pulse is "2 ticks".
6. The I²C bus framing (repeated start vs stop/start between the register
   byte and the read) is the I²C master hardware's, not the code's.
7. The I²C master's error bit (X:304F bit 2 → host I2CMSR) is read as "NACK /
   error" by its use only.
8. The test record's slave at `E[8]` and the test fixture behind it.
9. Edge cases the code does not guard: L1 + L2 is summed in 8 bits, so lengths
   that wrap past 255 pass the ≤ 70 check (0x0C3A–0x0C4E) and GET_DESCRIPTOR
   then serves L1 bytes from X:4010 regardless; L1 = 0 makes the product string
   read from 0x09 again. A driver writing an EEPROM should keep every bLength
   ≥ 2, even, and the sums within the limits.
10. Bench confirmation of everything above (a host read of 0xA0 through block 3,
    a blank-EEPROM enumeration) — none done.
