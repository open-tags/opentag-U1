# 40 Hz precision TWR hardware-in-the-loop test

This test validates the role-locked `twr_init-5` and `twr_resp-5` images on two
OpenTag v2.2 boards before making lower-level radio changes.

## Hardware assumptions

- MCU: STM32G474CC with an 8 MHz HSE on PF0/PF1.
- DWM3000 reset: PA3 through R16.
- DWM3000 SPI1: PA4 CS, PA5 SCK, PA6 MISO, PA7 MOSI.
- DWM3000 IRQ: PB0 with a 100 kohm pulldown. This baseline intentionally polls
  the radio; IRQ-driven receive is a later optimization after timing is proven.
- UWB channel 9, 850 kbps, 128-symbol preamble, IEEE-short SFD, and 4 MHz SPI.

This HIL pass restores the original `twr-2` precision PHY and keeps the 6 ms
of additional inter-range guard introduced in `twr-4`. It targets 40 Hz without
changing reply timing, bitrate, SPI, or calibration.

## Setup

1. Mark two boards A and B and place their antenna reference points at a measured
   1.00 m separation with clear line of sight.
2. Flash A with `twr_init-2` and B with `twr_resp-2` from the firmware console.
3. Reconnect both serial devices and request INFO. Record each 24-character ID.
4. Confirm A reports mode I, B reports mode R, both report target 40 Hz, and both
   report zero output drops. Confirm both report `br=850 pre=128 sfd=ieee ch=9`.
5. Let the boards warm up and remain still for one minute.

## Baseline run

Run a five-minute static test in the browser and download the CSV. The first gate
passes when all of these are true:

- Actual rate is between 39 Hz and 41 Hz.
- Packet success is at least 99 percent.
- Missing sequences are no more than 1 percent of expected samples.
- Both INFO responses still report zero output drops after the run.
- P95 exchange time is below 20 ms.
- The CSV contains RSSI values and diagnostic deltas for both roles.
- No resets, USB disconnects, or sustained `MISS` bursts occur.

Rate and reliability are the gate for this build. Do not recalibrate from a run
that fails the gate.

## Accuracy sweep

After the baseline passes, run five minutes at 1 m, 3 m, and 5 m in clear line of
sight. Keep board height and antenna orientation fixed. Calibrate at 1 m only,
then repeat all three distances without changing calibration. Save every CSV;
the files include firmware versions, device IDs, calibration, measured rate,
missing sequences, packet success, and exchange timing.

## Browser isolation check

Repeat one minute with the live chart paused, then one minute with it running.
The measured rate and missing-sequence count should be statistically unchanged.
If they differ, capture both CSVs before changing firmware.

At 40 Hz the responder produces roughly one short USB CDC line per exchange,
far below USB bandwidth. The console bounds logs to 200 lines, keeps 200 chart
samples, calculates running test statistics, and schedules at most one chart
render per animation frame, so browser work should not control radio cadence.

## Next optimization gate

Only after this baseline passes should the next build test PB0 IRQ-driven receive,
a faster SPI clock, or a higher-rate PHY. Change one layer at a time and compare
against the saved baseline CSVs.
