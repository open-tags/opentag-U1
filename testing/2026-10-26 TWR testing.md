---
date: 2026-10-26
product: opentag U1
topic: TWR benchmark
status: in progress
---

# 2026-10-26 TWR testing

Two-way ranging (TWR) benchmark for opentag U1 PCB V2. The goal is a clean, repeatable data set and a one-page report. Work through the sections in order and check items off as you go.

## Goal and targets

Set targets before testing and do not change them after.

| Metric | Target |
|---|---|
| Bias, line of sight, ≤ 15 m | < 5 cm |
| Spread (std), line of sight, ≤ 15 m | < 5 cm |
| Availability (successful exchanges) | ≥ 99 % |
| Logging integrity (`seq` gaps) | 0 |

## Equipment

- [ ] 2 × U1 PCB V2 boards: **A** = initiator, **B** = responder (record serials below)
- [ ] 2 × tripods
- [ ] 2 × USB-C power banks
- [ ] ST-Link (SWDIO, SWCLK, GND to board B only)
- [ ] Laser distance meter (model and accuracy below)
- [ ] Tape, marker, phone camera

| Item | Value |
|---|---|
| Board A serial / UID | Board 9 (initiator), UID `003B0057-3632500F-2032394B`, `twr_init` (DEFMT_LOG=info) |
| Board B serial / UID | Board 10 (responder), UID `00170032-36325010-2032394B` |
| Laser model and accuracy | |
| Location | |
| Firmware git hash | |
| `CALIB` value | |

## Learnings from T0 (2026-10-26)

What the T0 runs and diagnostics (T0-1, T0-2, DIAG-1 to DIAG-3) taught us, now built into the baseline and T1:

0. **Test upstairs, not at the downstairs bench.** *(Found by WEB-3.)* Downstairs, every board, firmware, radio config and power setup lost ~25 % of exchanges to ~10–25 undecodable channel-9 detections per second, even with no opentag transmitting (LISTEN-1). Upstairs, the same boards and firmware gave 100 % availability with essentially zero receive errors.
1. **Misses come from a background source of undecodable frames, not the radio config.** *(Corrected after T1-1.)* Board 10 sees either ~10–15 receive errors per second (PHY header / RS sync loss) or ~0.3–1.4 per second, and the misses follow that rate. Config A was in the high state (DIAG-1) and so was config B (T1-1); DIAG-2 and DIAG-3 happened to fall in a quiet window (15:43–15:58). The high state also appeared with board 9 switched off (T0-1), so the source is not our initiator. Config B stays the baseline (Qorvo default), but it is not the fix.
2. **The setup changes the readings a lot.** With config B, DIAG-2 (board 10 just powered on, USB cable on board 10, people moving) had a 168 mm spread with readings out to 2.1 m. DIAG-3 (both warmed up, power banks, SWD only, nobody near) had **9.6 mm**. T1 tests one power factor (board 9's supplemental USB supply); the others stay fixed at baseline.
3. **Power banks can switch off.** Board 9's bank shut off under low load during T0-1. Use always-on/low-current mode and check board 9's LED before every run.
4. **Board 10 has no power bank yet.** In every run it was powered through the ST-Link and/or USB, not a power bank. Its supply is a suspect for the background receive errors, which board 10 is the one counting.
4. **Start logging from boot.** `probe-rs attach` only shows the last ~1 KB of the log buffer. Record with `probe-rs run` (resets the responder) so a run is captured from the start.
5. **Moving the ST-Link while plugged in can wedge it** ("interfaces are claimed"). Unplug it at the Mac end to recover.
6. **Calibration depends on the radio config.** `CALIB` must be measured with config B (T2); the config-A default gives about −133 mm.
7. **Still open:** about 8 % misses with config B (requests not heard, bad final ID, no final). Read board 9's counters after T1.

## Baseline condition

Every test changes **one** thing from this baseline:

- **Location:** upstairs, near a wall (since 2026-10-28; downstairs has channel-9 interference)
- **Boards:** 9 = initiator, 8 = responder (board 8 replaced board 10 on 2026-10-26)
- **Firmware:** the website images `twr_init.bin` (`twr_init-5`) and `twr_resp.bin` (`twr_resp-5`), radio config A (850 kbps, 128-symbol preamble, 802.15.4a SFD, channel 9), read over USB serial like the web console. `CALIB` is set at runtime with the `CALIB <n>` command
- **Power:** both boards on Mac USB (data cables); no ST-Link attached
- **Warm-up:** both boards powered for at least 5 minutes before the first run
- **Distance:** 5.000 m (reference mark to reference mark); today's diagnostics and T1 use 1.546 m
- **Line of sight**, nothing within about 1 m of the path
- **Height:** both modules at 1.2 m. **Deviation 2026-10-28:** all upstairs runs (T0-up1, T2, CABLE, T3, T5, T4) were at **0.338 m** (measured; first estimated as 18 in / 0.46 m) on tripods, so floor reflections are stronger than at the planned height
- **Orientation:** yaw 0°, tilt 0°. Tape arrows on both boards point at each other; both boards in the same plane. **Deviation:** all runs through 2026-10-28 had the boards lying flat, not upright (see T5 mounting finding)
- **People:** nobody within about 2 m of the boards or the line between them
- **Cables:** taped straight down the back of each tripod, the same every run
- **Laptop:** fixed spot off to the side, never between the boards; same power state every run (on battery or on charger)

## Firmware and tooling prerequisites

- [x] Bench firmware for the responder (`twr_bench_resp`): logs over defmt/RTT only (no USB, CAN or web console)
	- [x] Startup banner: git hash, board UID, `CALIB`, radio config, system clock
	- [x] Per measurement: `D seq dist_mm exchange_us rssi los_pct` (LOS confidence from the radio instead of first-path power)
	- [x] Per failure: `MISS seq reason`
	- [x] Stats every 10 s, plus receive errors by type (`RXERR idle` / `RXERR final`)
	- [x] Board-clock timestamp on every line; built with `DEFMT_LOG=info`
	- [x] `CALIB` (`TWR_CALIB`) and radio config (`TWR_PHY`) set at build time
- [x] Bench initiator (`twr_bench_init`): same protocol, logs misses, stats and `RXERR resp` over RTT
- [ ] Recorder tool. For now: `probe-rs run` (resets board 10, logs from boot) for 60 s into `raw.log` + `run.json`, then the summary script prints n / mean / std / misses / gaps / Hz
- [ ] Run data saved under an ignored `bench-runs/` folder, never committed

## Session start (once per session)

1. [ ] Set both tripods to 1.2 m; tape a reference mark on each module and an orientation arrow on each board
2. [ ] Flash board A, move it to its power bank, confirm it is running
3. [ ] Flash board B; check the startup banner; fill in the table above
4. [ ] Photograph both ends and an overview of the room
5. [ ] Warm up both boards for 5 minutes

## Per-run procedure

1. Position the boards; re-aim the arrows at each other
2. Laser-measure 3 times; write all three in the run table
3. Start the recorder (60 s unless the test says otherwise)
4. Hands off; note anyone walking through
5. Check the quick summary: redo the run if there are `seq` gaps, low *n* or a disturbance
6. Move to the next point

## Tests

Result columns: **n** = samples, **mean** and **std** in mm, **miss %** = failed exchanges, **gaps** = `seq` gaps with no `MISS` line.

### T0 · Smoke test

Baseline, 2 minutes. Confirms the firmware boots, runs at about 40 Hz and logs without gaps.

- [x] T0 done on config A (logging and rate pass; availability fails the 99 % target). DIAG-3 is the config-B equivalent (60 s, 9.6 mm spread, 91.9 % availability).

| Run | Laser (mm) ×3 | n | mean | std | miss % | gaps | Hz | Notes |
|---|---|---|---|---|---|---|---|---|
| T0-1 | 1546 | 0 | – | – | – | – | – | Invalid: board 9's power bank shut off before recording started |
| T0-2 | 1546 | 3749 | 1389.8 | 15.4 | 26.0 | 0 RTT loss | 42.2 polls/s, 31.2 ranges/s | Uncalibrated (`CALIB` 12241): bias −156 mm. Misses: 684 no-final, 254 bad-final-id, 372 polls not heard. Log matches firmware counters exactly. LOS 100 %, RSSI −76.8 dBm |

| T0-up1 (2026-10-28, upstairs) | 918 | 4763 | 1064.7 | 26.2 | 0.2 | 3 unheard (no loss on USB) | 39.7 | **Pass.** Boards 9 → 8, website firmware over USB, uncalibrated (`CALIB` 12241): bias +147 mm. Last 2 min of a 6 min recording; warm-up drift only +4 mm over 6 min. Responder: 17 receive errors in 6 min |

Raw run data from these sessions is published with the [performance results](https://open-tags.com/validation/).

### T1 · Setup controls

Config B, 1.546 m, 60 s each, back to back. One factor only: **board 9 with vs without a supplemental USB power supply**. Everything else stays at baseline. **Pass:** mean within about 3 mm and spread within about 5 mm between the two runs.

- [ ] T1 done

| Run | Board 9 power | n | mean | std | min–max | avail % | Notes |
|---|---|---|---|---|---|---|---|
| T1-1 | Without supplemental USB supply | 2209 | 1427.2 | 34.5 | 1368–1623 | 82.0 | Did **not** reproduce DIAG-3: background errors back at 11.2/s (DIAG-3: 1.4/s). Ended 16:04. Rerun once the background source is understood |
| T1-2 | With supplemental USB supply (record the supply) | | | | | | |

### T2 · Calibration

3.000 m, 2 minutes. Compute the antenna-delay constant, rebuild with it, then confirm. **Excluded from scoring.**

- [x] T2-1 recorded, `CALIB` computed (+38 ticks at 4.69 mm/tick)
- [x] New `CALIB` applied (sent over USB instead of rebuilding)
- [x] T2-2 confirms the calibration-distance reading matches the laser within 1 cm (−1.1 mm)

| Run | Laser (mm) ×3 | n | mean | std | `CALIB` | Notes |
|---|---|---|---|---|---|---|
| T2-1 | 3257 | 4374 | 3434.8 | 18.7 | 12241 | before: bias +177.8 mm (2026-10-28, upstairs, boards 9 → 8) |
| T2-2 | 3257 | 2185 | 3255.9 | 16.0 | 12279 | after: bias −1.1 mm, 100 % availability (old cables, superseded) |
| T2-3 | 3257 | 2186 | 3255.6 | 25.2 | **12282** | **current**: recalibrated for the new USB-C cables (from CABLE-2, +41 ticks). Bias **−1.4 mm**, 99.9 % availability |

Current value: **`CALIB 12282`** (new cables). It was sent to board 8 at runtime over USB (`CALIB <n>` command); it is **lost if board 8 resets or loses power**, so resend it at the start of every session (check `INFO calib=`). Note: before calibration the bias was +147 mm at 0.918 m but +178 mm at 3.257 m, so expect about −31 mm at short range after calibration. T3 will map this distance-dependent bias.

### T3 · Distance sweep

Line of sight, 60 s per distance. The headline result.

- [x] T3 done (2026-10-28, upstairs, 0.57–5.66 m; 7.5–20 m deferred)

| Run  | Nominal | Laser (mm) ×3 | n    | mean   | std  | bias       | miss % | Notes                                                                                                |
| ---- | ------- | ------------- | ---- | ------ | ---- | ---------- | ------ | ---------------------------------------------------------------------------------------------------- |
| T3-1 | 0.5 m   | 568           | 2186 | 414.1  | 17.6 | **−153.9** | 0.0    | 2026-10-28, `CALIB 12282`, new cables. RSSI −58.9 dBm (3.257 m: −70.8). Large short-range under-read |
| T3-2 | 1 m     | 1266          | 2184 | 1195.5 | 18.2 | −70.5      | 0.0    | RSSI −61.1 dBm                                                                                       |
| T3-3 | 1.5 m   | 1798          | 2185 | 1755.6 | 24.9 | −42.4      | 0.0    | RSSI −62.8 dBm                                                                                       |
| T3-4 | 2 m     | 2249          | 2191 | 2185.1 | 20.3 | −63.9      | 0.0    | RSSI −68.1 dBm. Breaks the shrinking trend (−42 at 1.8 m)                                            |
| T3-5 | 2.5 m   | 2698          | 2183 | 2661.5 | 16.2 | −36.5      | 0.1    | RSSI −69.7 dBm                                                                                       |
| T3-6 | 4 m     | 4226          | 2184 | 4217.1 | 25.4 | −8.9       | 0.0    | RSSI −71.4 dBm                                                                                       |
| T3-7 | 5 m     | 5658          | 2186 | 5605.9 | 48.6 | −52.1      | 0.0    | RSSI **−84.6 dBm** (13 dB weaker than at 4.2 m): weak signal, doubled spread                         |

Upstairs allows about 5 m of clear line of sight, so 7.5–20 m are deferred to a longer space. 3.257 m is the calibration distance and is not scored.

### T4 · Repeat sweep

Same distances in **reverse order**, ideally on another day. Checks repeatability and drift.

- [x] T4 done (2026-10-28: same session as T3, reverse order, board 9 at 0°; skips 3.257 m)

| Run | Nominal | Laser (mm) ×3 | n | mean | std | bias | miss % | Notes |
|---|---|---|---|---|---|---|---|---|
| T4-1 | 5.658 m | 5658 | 2189 | 5722.0 | 47.5 | +64.0 | 0.3 | RSSI −80.7. Matches T5-5 (+60); T3-7 read −51 at the same spot |
| T4-2 | 4.2 m | 4290 | 2190 | 4298.6 | 21.6 | +8.6 | 0.2 | RSSI −72.3. T3-6 at 4.226 m: −8.9 |
| T4-3 | 2.5 m | 2457 | 2191 | 2398.4 | 21.5 | −58.6 | 0.1 | RSSI −68.4. Between T3's 2.249 m (−64) and 2.698 m (−37) |
| T4-4 | 2.2 m | 2173 | 2186 | 2177.9 | 26.5 | **+4.9** | 0.0 | RSSI −64.8. T3 at 2.249 m read −64: a 7 cm move changed the error by ~7 cm |
| T4-5 | 1.8 m | 1774 | 2186 | 1649.7 | 19.4 | **−124.3** | 0.0 | RSSI −69.0. T3 at 1.798 m read −42: 2.4 cm apart, 8 cm different |
| T4-6 | 1.3 m | 1259 | 2190 | 1218.5 | 20.2 | −40.5 | 0.2 | RSSI −58.3. T3 at 1.266 m: −70.5 |
| T4-7 | 0.6 m | 657 | 2191 | 592.5 | 19.4 | −64.5 | 0.1 | RSSI −57.4. T3 at 0.568 m: −154 |

### T5 · Orientation

Board 9 (initiator) stays at baseline; rotate **board 8 (responder)** only, about its reference mark so the distance doesn't change. 0° = arrow pointing at board 9; positive yaw = clockwise seen from above. Tilt 90° = board stood on its edge, arrow still pointing at board 9. Upstairs (2026-10-28) T5 runs at the T3-7 spot, **5.658 m** (baseline error −51 mm, weak signal ~−85 dBm), so compare each angle against T5-1 rather than against zero; low-signal angles may lose packets.

- [ ] T5 done

| Run | Board B yaw / tilt | n | mean | std | bias | miss % | rssi | Notes |
|---|---|---|---|---|---|---|---|---|
| T5-1 | 0° / 0° | 2193 | 5606.5 | 67.3 | −51.5 | 0.0 | −84.6 | 5.658 m; matches T3-7 (−52 mm). Spread 67 mm |
| T5-2 | 45° / 0° | | | | | | | |
| T5-3 | 90° / 0° | | | | | | | |
| T5-4 | 135° / 0° | | | | | | | |
| T5-5 | 180° / 0° | | | | | | | |
| T5-6 | 225° / 0° | | | | | | | |
| T5-7 | 270° / 0° | | | | | | | |
| T5-8 | 315° / 0° | | | | | | | |
| T5-9 | 0° / 90° | | | | | | | |

**Board 9 (initiator) rotations** (board 8 at 0°):

| Run  | Board 9 yaw / tilt | n    | mean   | std  | bias       | miss % | rssi  | Notes                                                                      |
| ---- | ------------------ | ---- | ------ | ---- | ---------- | ------ | ----- | -------------------------------------------------------------------------- |
| T5-2 | 90° / 0°           | 2191 | 5862.4 | 30.4 | **+204.4** | 0.1    | −79.3 | **+256 mm vs T5-1** from a 90° turn of board 9; signal 5 dB *stronger*     |
| T5-3 | 180° / 0°          | 2192 | 5865.9 | 21.7 | **+207.9** | 0.0    | −77.6 | Same as 90° (+3.5 mm); signal stronger still                               |
| T5-4 | 270° / 0°          | 2187 | 5638.7 | 35.2 | −19.3      | 0.0    | −78.7 | Back near 0°'s reading despite a strong signal                             |
| T5-5 | 0° / 0° (repeat)   | 2192 | 5718.4 | 52.5 | +60.4      | 0.0    | −80.7 | **Doesn't repeat T5-1** (−51.5 mm): 112 mm apart at the same nominal angle |

**Conclusion at 5.658 m:** readings jump by 10–25 cm with orientation and don't repeat when returning to 0°. This spot (weak signal, near the room's edge) is dominated by multipath, so small placement changes flip which path the radio locks onto. Repeat T5 at a strong-signal spot before drawing conclusions about the antenna itself.

**T5b: board 9 rotations at 4.262 m** (strong signal; board 8 at 0°; 0.338 m height):

| Run | Board 9 yaw | n | mean | std | bias | miss % | rssi | Notes |
|---|---|---|---|---|---|---|---|---|
| T5b-1 | 0° | 2191 | 4327.9 | 18.3 | +65.9 | 0.0 | −70.1 | T3 (4.226 m) −9, T4 (4.290 m) +9: this placement reads 6–7 cm longer |
| T5b-2 | 90° | 2192 | 4314.3 | 13.1 | +52.3 | 0.0 | −72.7 | −13.6 mm vs 0° |
| T5b-3 | 180° | 2191 | 4511.3 | 20.7 | **+249.3** | 0.0 | −70.4 | **+183 mm vs 0°**, signal just as strong. 180° also read +208 at 5.658 m |
| T5b-4 | 270° | 2191 | 4229.3 | 11.7 | −32.7 | 0.0 | −71.8 | −98.6 mm vs 0°; tightest spread of the set |
| T5b-5 | 0° (repeat) | 2186 | 4235.8 | 11.8 | −26.2 | 0.0 | −70.0 | **Doesn't repeat T5b-1** (+65.9): 92 mm apart at the same angle |

**T5b conclusion:** the repeat check failed again (92 mm), so per-angle offsets are not reliable yet. The one result that held at both spots: **180° (board 9 facing away) reads ~+18–21 cm long** with a strong signal. Likely confounder: board 9's **USB cable moves and twists with every rotation**, changing what sits near the antenna (the cable swap alone moved readings 12 mm). For clean orientation data, run board 9 on a power bank with no cable attached.

**Decision (2026-10-28):** no orientation correction for U1. The data is kept as a reference. U2 will carry an onboard IMU that gives absolute orientation, which could support an orientation-aware correction later if needed.

**Mounting finding (2026-10-29):** every TWR run had both boards **lying flat** on the tripods. The DWM3000 antenna is vertically polarised and meant to be mounted upright, where it is omnidirectional in the horizontal plane; when antennas are not aligned, the pattern has nulls at some angles that "can limit range and introduce location inaccuracy" (DWM3000 data sheet, section 5). Flat boards make the antenna horizontal, so rotating a board changes polarisation alignment. That likely explains the large, non-repeating orientation effects in T5/T5b (+18–21 cm at 180°). **Retest T5 with both boards upright** (module long axis vertical, antenna end up; the module sits at 45° on the U1 PCB, so the board stands on a corner) before relying on any orientation conclusion.

### T6 · Obstructions

5 m, baseline orientation. One obstruction at a time.

- [x] T6 done (door only, 2026-10-28; person, body, cardboard and metal skipped)

Upstairs 2026-10-28 placement: 4.262 m, board 9 at 0°, 0.338 m height. No-obstruction references: T5b-5 −26.2 mm (RSSI −70.0), T6-0b (door reopened) **−35.7 mm** (std 12.3, RSSI −70.9). Compare obstruction runs against these (~−31 mm).

| Run | Obstruction | Position | n | mean | std | bias | miss % | Notes |
|---|---|---|---|---|---|---|---|---|
| T6-1 | Person standing | Midway | | | | | | |
| T6-2 | Board B held against a body | At board B | | | | | | |
| T6-3 | Cardboard box | Midway | | | | | | |
| T6-4 | Door or wall | Closed interior door between the boards | 2192 | 4229.4 | 14.1 | −32.6 | 0.0 | 2026-10-28, 4.262 m, board 9 at 0°. vs no-obstruction T5b-5 (−26.2, RSSI −70.0): **−6.4 mm**, RSSI −73.3 (−3.3 dB) |
| T6-4b | Door or wall | Same door closed again (repeat) | 2192 | 4227.7 | 13.1 | −34.3 | 0.0 | vs reopened baseline T6-0b (−35.7): **+1.4 mm**, RSSI −73.2 (−2.3 dB). Door effect repeats: ~0 mm, ~3 dB |
| T6-5 | Metal sheet or laptop lid | Midway | | | | | | |

### T7· Board swap

5 m. Boards A and B swap roles (reflash), each keeping its orientation toward the other.

- [x] T7 done (2026-10-28)

| Run | Initiator / responder | n | mean | std | bias | miss % | Notes |
|---|---|---|---|---|---|---|---|
| T7-1 | 8 / 9 | 2192 | 4403.4 | 19.4 | +141.4 (`CALIB 12241`) | 0.0 | 2026-10-28, 4.262 m (assumed, boards not moved), RSSI −68.7. Expected +156.7 if swapping roles changed nothing (T6-0b −35.7 at `CALIB 12282`, +41 ticks): **role-swap effect −15 mm (~3 ticks)**, within placement noise. Laser remeasured afterwards: **4.281 m**, so bias +122.4 mm at `CALIB 12241`. Board 9 calibrated at this spot: **`CALIB 12267`** (zero error at 4.281 m; the 9 → 8 pair was zeroed at 3.257 m, where 4.2 m read within ±1 cm) |

### T9 · Stability

Baseline, 60 minutes. Shows warm-up drift and checks logging integrity over a long run.

- [x] T9 done

| Run | Duration | n | mean | std | drift (first vs last 5 min) | gaps | Notes |
|---|---|---|---|---|---|---|---|
| T9-1 | 60 min | 143011 | 4278.3 | 20.8 | −0.7 mm (−2.1 → −2.8) | 0 (longest 0.25 s) | 2026-10-28 17:07–18:07, boards 8 init / 9 resp, 4.281 m, `CALIB 12267`, website firmware, Mac USB. Bias −2.7 mm, p95 \|e\| 38 mm, max \|e\| 77 mm. 39.7 Hz, availability 99.98 % (33 misses, all single). Every 1-min mean within −4.4…−1.5 mm: **no warm-up drift**. 1-min std stepped from ~19.5 to ~22 mm around minute 29 (likely a change in the room, not the boards; RSSI flat at −68.5). Plot: `output/u1-twr-bench/2026-10-28/plots/t9_stability.png` |

### H · Height

Same spot as T9-1 (4.281 m, boards 8 init / 9 resp, `CALIB 12267`, same cables); only the tripod height changes.

| Run | Height | n | mean | std | bias | miss % | Notes |
|---|---|---|---|---|---|---|---|
| T9-1 | 0.338 m | 143011 | 4278.3 | 20.8 | −2.7 | 0.02 | Reference (60 min). RSSI −68.5 |
| H-1 | 1.008 m | 2184 | 4361.7 | 13.3 | **+146.7** | 0.05 | 2026-10-28, 60 s. RSSI −70.1. Laser re-measured at this height: **4.215 m** (66 mm shorter; raising the tripods moved the boards). **Spread −36 %** vs 0.338 m, but bias **+149 mm** (≈ +32 ticks): `CALIB 12267` from 0.338 m does not hold at 1.008 m. Calibration is geometry-dependent; recalibrate at the final height |
| H-2 | 1.008 m | 2191 | 4216.2 | 13.4 | **+1.2** (`CALIB 12298`) | 0.05 | 2026-10-28, 60 s, same spot as H-1. RSSI −70.1, p95 \|e\| 27 mm. **Calibration for 1.008 m: board 9 `CALIB 12298`** (+31 ticks vs 0.338 m). Runtime only, lost on reset |

## Schedule

- **Day 1 (about 3 h):** T0, T1, T2, T3, T5, T6
- **Day 2 (about 2.5 h):** T4, T7, T9

## Rules

- Change one thing at a time from the baseline.
- Never score at the calibration distance (3 m).
- Re-aim both arrows after every move; measure the truth after setting up, not before.
- Hands off during recording; note anyone walking through.
- Don't change firmware or `CALIB` mid-session without recording it.
- Keep the laptop's position and power state the same for every run.

## Results summary

Fill in after the session; the generated report replaces this.

| Metric | Target | Result | Pass |
|---|---|---|---|
| Bias, line of sight, ≤ 15 m | < 5 cm | | |
| Spread, line of sight, ≤ 15 m | < 5 cm | | |
| Availability | ≥ 99 % | | |
| `seq` gaps | 0 | | |
| Worst orientation bias (T5) | report | | |
| Worst obstruction bias (T6) | report | | |

## Notes

### Experiment log 2026-10-28 (upstairs)

- **14:37** Boards 9 (initiator) and 8 (responder) upstairs on Mac USB, website firmware. T0-up1 at 0.918 m: pass (99.8 %, 39.7 Hz).
- **~14:50** T2 calibration at 3.257 m: `CALIB` 12241 → **12279**, bias +177.8 → −1.1 mm.
- **~15:03 Cable swap.** Both boards moved to **new USB-C cables**, same positions (3.257 m). Replugging power-cycled both boards, so board 8 reverted to `CALIB 12241`.
- **15:04 CABLE-1** (3 min, `CALIB 12241`, compare with T2-1):

| | T2-1 (old cables) | CABLE-1 (new cables) | Change |
|---|---|---|---|
| Mean | 3434.8 mm | 3446.4 mm | **+11.6 mm** |
| Spread (std) | 18.7 mm | 26.5 mm | +7.8 mm |
| Availability | 99.9 % | 100.0 % | — |
| Receive errors (responder / initiator) | — | 1 / 0 in 3 min | — |

  Per minute the mean held at 3445–3448 mm, so it is not a warm-up drift. The shift exceeds the 3 mm T1 pass threshold, but the cable swap also **power-cycled both boards**, so the cause could be the cables, the power cycle (radio re-initialization) or a small bump while replugging. Next: power-cycle again on the new cables without moving anything, to separate cable from power cycle.
- **15:09 CABLE-2** (3 min): both boards unplugged 10 s and replugged on the same new cables, not moved, `CALIB 12241`. Mean **3449.5 mm**, std 25.4 mm, 99.9 % availability. Within ~3 mm of CABLE-1, so **a power cycle alone barely moves the reading**; the +12–15 mm shift and the wider spread arrived with the cable swap (the new cables, or a small nudge while swapping them). Plot: `output/u1-twr-bench/2026-10-28/plots/cable_swap.png`.
- **15:2x Decision: keep the new cables.** Recalibrated at 3.257 m: `CALIB 12282` (T2-3: bias −1.4 mm, std 25.2 mm). T3 runs on the new cables.
- **T7 board swap (reflash):** board 9 → `twr_resp.bin` (responder), board 8 → `twr_init.bin` (initiator), both website images, flashed over the ST-Link and verified. Boards stayed in the same spots. Board 9 starts at the default `CALIB 12241`.

### Diagnosing the T0 misses

| Run | Radio config | Setup | Availability | PHY errors in FINAL window | mean | std | Notes |
|---|---|---|---|---|---|---|---|
| DIAG-1 | A: 850 kbps, 128, 4a SFD, PAC 16 (as `twr_resp`) | 10 on power bank + SWD | 74.2 % | 209 / 40 s | 1407 | 37.8 | Misses are almost all corrupted PHY headers (plus some RS sync loss), not timeouts |
| DIAG-2 | B: 6.8 Mbps, 128, 4z SFD, PAC 8 (Qorvo default) | 10 on USB + SWD, just powered on | 93.7 % | 4 / 40 s | 1560 | 168.0 | Decode errors gone. New problem: long positive tail (1.33–2.11 m) and mean drifting down over the first 20 s |

| DIAG-3 | B | Both warmed up; 10 on power bank + SWD only; nobody near | 91.9 % | 27 / 60 s | 1413 | **9.6** | Clean setup removes the DIAG-2 tail: range 1383–1457 mm. Bias −133 mm (uncalibrated). Remaining misses: 113 requests not heard, 65 bad-final-id, 41 no-final |

Initiator with no responder running (config B) still logged ~12 PHY/RS errors per second.

Plot: `output/u1-twr-bench/2026-10-26/plots/config_a_vs_b.png` (DIAG-1 vs DIAG-2).

**Background receive-error rate per run (board 10):**

| Run | Ended | Config | Board 9 | Receive errors / s | No-final |
|---|---|---|---|---|---|
| T0-1 | 15:30 | A | Off for most of the run | 14.5 | 172 |
| DIAG-1 | 15:40 | A | On | 10.6 | 243 / 40 s |
| DIAG-2 | 15:53 | B | On | 0.3 | 5 / 40 s |
| DIAG-3 | 15:58 | B | On | 1.4 | 36 / 60 s |
| T1-1 | 16:04 | B | On | 11.2 | 294 / 60 s |

**WEB-1 (website firmware, no debugger):** boards 9 and 10 flashed with the exact `open-tags.github.io` bins (`twr_init-5` / `twr_resp-5`, config A, same ranging code as today's source), both on Mac USB, ST-Link unplugged, read over USB serial like the web console. Result: **74.1 % availability**, 31.5 ranges/s, ~11 receive errors/s at board 10 but only ~2.9/s at board 9; initiator saw 11.6 % no-response. Same as the bench firmware (DIAG-1: 74.2 %). **So the bench build, SWD/RTT logging and ST-Link power are not the cause; it's boards 9/10 or the room.** Board 10 sees ~4× more receive errors than board 9.

**WEB-2 (board 8 replaces board 10):** same website firmware, same cable, port and position; board 8 UID `003B004E-3632500F-2032394B`. Result: **76.4 % availability**, ~9.8 receive errors/s at the responder, 31.9 ranges/s. Distances read ~1.87 m (board 8 uncalibrated). **Board 10 is not the cause.** Board 9 (the initiator in every run) or the room remain. The initiator's USB log was empty this run.

**LISTEN-1 (board 8 alone, board 9 unplugged):** with no opentag transmitting, board 8's receive-error counter rose by 794 in ~61 s, **~13 errors/s**, and it heard no requests. **The background is in the environment, not our boards:** boards 8, 9 and 10 all see it, with or without the other board transmitting.

**WEB-3 (boards 8 + 9 moved upstairs, near a wall, away from other equipment):** same website firmware, both on Mac USB. Result: **100.0 % availability**, 39.9 ranges/s, 0 misses, 0 unheard requests, **1 receive error at the responder and 0 at the initiator in 60 s** (vs ~10–13/s downstairs). Distance not measured (mean 1190 mm, std 21 mm, uncalibrated, just replugged).

**Root cause: the downstairs bench location.** Something there produces ~10–25 channel-9 detections per second that our radios can't decode, and every one that lands in a receive window costs an exchange. Boards, firmware, radio config and power were all ruled out. **Run the remaining tests upstairs.** Finding the downstairs source is optional (likely a UWB device such as a phone, watch, AirTag or smart-home hub), but real deployments will meet the same thing, so the firmware should get more robust to it (keep listening after a bad frame instead of abandoning the exchange).

**Takeaways so far:** the misses track a background error source that switches between ~11–15/s and ~1/s, independent of radio config and present even with board 9 off. My earlier conclusion that config B fixed the misses was wrong: DIAG-2/3 ran during a quiet window. Spread also tracks it (9.6 mm quiet vs 34.5 mm noisy). Next: identify the source.

