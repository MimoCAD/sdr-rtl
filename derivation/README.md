# How sdr-rtl's numbers were derived

Every value this driver writes to an RTL2832U or an R820T/R820T2 comes
from one of three places, and each is recorded here:

1. **A vendor document**, cited by table and page
   ([`documents.md`](documents.md)).
2. **A choice made from a documented meaning**, with the reason written
   beside it (e.g. "PW_LNA = 000: the document's maximum LNA current,
   chosen for noise figure").
3. **A measurement**, made by the harness in this repository against
   real hardware, with its raw data committed under `data/` and the
   table it produced generated into the source — never typed in by hand.

No value is taken from another driver's source code. Where an earlier
driver's behaviour is an interoperability fact (which register switches
the RTL2832U to raw IQ output, how its filter coefficients are packed),
it is established here by black-box probing that the harness can replay.

## The bench

- The receiver under test: an RTL2832U dongle with an R820T-family tuner
  and a 28.8 MHz crystal.
- The signal source: any transmitter the harness can command to "a tone
  at f Hz and L dBm". The source is CABLED to the dongle through fixed
  attenuation — never radiated. 40–60 dB keeps the dongle far below the
  R820T's +10 dBm maximum input (R820T datasheet, Table 1-2).
- Tests that need no source use the tuner's own noise.

## The records

Each measurement writes one file under `data/`: what was measured, how
(the sweep, the settings held fixed), the instrument chain, the raw
readings, and the tool's version. No device serials, no host names.
The generated tables cite the file they came from.
