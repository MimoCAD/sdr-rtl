# Hazard: USB-block register 0x1C puts the dongle into a USB test mode

Found 2026-10-08 during the single-write block scan
(`data/step3-blockscan-usb.jsonl`, offset 28 = 0x1C). Twice the hub
branch carrying the analyzer, the dongle and a second radio dropped off
the bus and stayed off until the hub (an iVANKY FusionDockUltra dock)
was power-cycled.

## The exact transfer

One vendor control OUT to the RTL2832U, as every write in the scan:

| field | value | meaning |
|---|---|---|
| bmRequestType | 0x40 | host-to-device, vendor, device |
| bRequest | 0x00 | don't care per the datasheet's Table 27 |
| wValue | 0x201C | USB block base 0x2000 + offset 0x1C (Table 29's map lists 0x001C as reserved) |
| wIndex | 0x0110 | block 1 (USB) << 8 \| set (1) << 4 \| page 0 (Table 28) |
| wLength | 1 | |
| data | the value under test | |

Setup packet, little-endian on the wire:
`40 00 1C 20 10 01 01 00`, then one data byte.

The register's power-on value reads 0x00. Every write in the scan was
followed by a read-back, a 50 ms bulk-IN window on endpoint 0x81, a
restore to 0x00 and its read-back.

## What each value did (first 101 values, 0x00–0x64)

- **Stored:** the register keeps bits [2:0] only; the read-back is
  `value & 7` for every value that did not wedge (0x08 → 0, 0x0D → 5,
  0x0E → 6, 0x0F → 7, …). Bit 3 and above are ignored.
- **Lethal:** every value with `value & 7` in {1, 2, 3, 4} — 52 of 52
  such values — made the chip stop answering USB immediately: the write
  itself completed with the host's NotResponding error, the bulk pipe
  reported Disconnected, and the device needed VBUS removed and
  restored. `value & 7` in {0, 5, 6, 7} — 48 of 48 — was harmless.
- **The branch death:** on two of those wedges (values 0x54 and 0x64,
  i.e. field 4, immediately after a field-3 wedge and recovery), the
  VBUS power cycle through the analyzer's TARGET-A switch was followed
  by the analyzer, the dongle and the second radio all leaving the bus.
  Only a dock power cycle (once also a host restart) restored them. The
  analyzer's own gateware stayed up (its status LEDs stayed lit).

Earlier, on demodulator page 4, 320 consecutive wedges with the same
recovery did not harm the hub: this is not a cycle-count effect.

## What the field almost certainly is

The lethal set {1, 2, 3, 4} with 5 harmless is the USB 2.0
specification's test-mode selector table (USB 2.0 §7.1.20 and Table
9-7, SetFeature TEST_MODE):

| selector | mode | what the device drives |
|---|---|---|
| 1 | Test_J | a continuous J state |
| 2 | Test_K | a continuous K state |
| 3 | Test_SE0_NAK | SE0, NAK to everything |
| 4 | Test_Packet | the standard test packet, repeated |
| 5 | Test_Force_Enable | hubs only — meaningless to a device |

So writing 0x201C[2:0] = 1..4 appears to put the dongle's USB
transceiver into the corresponding electrical test mode directly,
without the host's SetFeature. In those modes a device stops
answering (that is the wedge), and per §7.1.20 the only exit is a
power cycle (that is why the restore write cannot help).

This is a hypothesis from the value pattern; the proof is in
"Confirming it" below.

## Spec position, both sides

- The transfer we send is a legal vendor request. Entering a test
  mode this way is the chip's own undocumented behaviour, but the
  result on the bus is the same as after SetFeature(TEST_MODE): a
  device driving Test_J / Test_K / SE0 or test packets at its upstream
  port. Test modes exist for compliance testing on a port the host has
  deliberately put under test; a device entering one spontaneously
  while attached to a normal hub is outside the specification's
  intended use. That part is on us.
- A hub whose entire downstream branch then stays dead through a
  VBUS cycle of the offending port, until the hub itself is power
  cycled, is not recovering from an illegal bus state the way a hub
  should (a port reset or disconnect is supposed to end it). That part
  is the dock's, and is what a vendor report would describe: "a
  downstream device driving USB 2.0 test-mode signalling (Test_J,
  Test_K, Test_SE0_NAK or Test_Packet) locks the branch until the dock
  is power cycled."

## Confirming it (needs someone present: the dock will need a restart)

1. Put the analyzer in capture (`cynthion-witness watch 20 --ep 1`),
   then run a single candidate: `harness blockscan --block usb --from
   28 0x04 --limit 1 --out /tmp/testmode.jsonl --no-cycle`.
   Test_Packet (selector 4) makes the device transmit the standard
   53-byte test packet of §7.1.20 continuously; the capture should show
   it (or the analyzer's own bus-state events for J/K/SE0 with 1–3).
2. Repeat with selector 1 or 3 to see the DC states.
3. If a different dock or a plain hub recovers on port reset, the
   defect is the dock's.

## The confirmation run (2026-10-08, with the operator present)

`data/step3-testmode-selector4-capture.log` (the analyzer watching the
dongle's port, all endpoints, orphan DATA packets dumped) and
`data/step3-testmode-selector4.jsonl` (one candidate: 0x1C ← 0x04,
no power cycle before it).

What the wire showed in the ~1 s between the write and the end of the
capture: the control transfer's own packets (zero-length DATA1 status
stages `4b 00 00`, a one-byte read-back `4b e8 40 f1` = 0xE8, the
DEMOD_CTL value), then nothing — **no standard test packet** (a
53-byte DATA0 beginning `00 × 9, aa × 8, …`, repeated) was seen. The
dongle stopped answering (the write completed with NotResponding),
and within the same second the analyzer's own control pipe stopped
answering too; the harness's recovery then found no dongle on the bus.

So the Test_Packet identification is **not confirmed**: selector 4
did not produce visible test packets before the analyzer itself went
down. What is confirmed is narrower and still useful for the dock
report: a single vendor write of a value with bits [2:0] in 1–4 to
0x201C makes the dongle silent on USB within a millisecond, and the
device sitting between it and the dock (the analyzer's passthrough)
loses its own control pipe in the same second, after which the dock's
whole branch is gone until the dock is power cycled. Whether the chip
drives a test-mode line state, a PHY power-down, or something else on
its upstream pair cannot be told from this capture; a scope on the
D+/D− pair at the moment of the write would.

## Scan handling

The remaining values of 0x1C (0x65–0xFF) are deferred, not cut: by
the stored-bits finding they can only repeat the eight field values
already seen, but they stay in the candidate space and run last, with
someone present. The block resumed at offset 0x1D.
