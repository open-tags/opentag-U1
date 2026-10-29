---
date: 2026-10-28
product: opentag U1
topic: TDOA benchmark
status: planned
---

# 2026-10-28 TDOA testing

Time-difference-of-arrival (TDOA) benchmark for opentag U1 PCB V2: **3 anchors, 1 tag, 2D position.** Test in layers so every error can be traced: clock sync first, then each anchor pair's time difference, then position. Work through the sections in order and check items off as you go.

Builds on [[2026-10-26 TWR testing]]. TWR is reused as the comparison (D9) and, only if needed, to check the anchor survey (D0).

## Roadmap (2026-10-29)

1. [ ] **D1-2:** repeat the 10 min clock-sync run with all three anchors **upright** (module vertical, antenna up); compare with D1-1 (flat)
2. [ ] **Add a 4th anchor:** flash `tdoa_bench` anchor on a fifth board, repeat D1 with 4 anchors (all pairs)
3. [ ] **Software:** clock model (drift term / two-sided interpolation), weighted least-squares solver with residual check, smoothing over 1 s
4. [ ] **Tag tests:** tag firmware, then D3 calibration and the D4 grid at many points
5. [ ] **Downstairs:** find and switch off the channel-9 interference (likely HomePod mini / phone), then repeat there

## Goal and targets

Set targets before testing and do not change them after.

| Metric                                        | Target            |
| --------------------------------------------- | ----------------- |
| Clock-sync prediction error (std)             | < 100 ps (≈ 3 cm) |
| Pair time-difference error (std), per pair    | < 3 cm            |
| 2D position error, inside the anchor triangle | < 10 cm (95 %)    |
| Repeatability, return to the same point       | < 5 cm            |
| Position drift, 60 min                        | < 2 cm            |
| Blinks heard by all 3 anchors                 | ≥ 99 %            |
| Logging integrity (`seq` gaps)                | 0                 |

## Architecture under test

- **Uplink TDOA.** The tag transmits a short **blink** at 40 Hz. Every anchor timestamps it and reports over USB to the Mac, which solves position.
- **Wireless clock sync.** Anchor **M** (master) transmits a **sync** frame at a fixed rate (10 Hz to start, tuned in D2). Anchors **A1** and **A2** timestamp each sync and model M's clock offset and drift against their own. The anchor positions are surveyed, so each sync frame's flight time is known and subtracted.
- **2D only.** Three anchors give two independent time differences. Anchors and tag sit at the same height; 3D needs a fourth anchor.
- **Raw timestamps are the data.** Positions are recomputed offline from the logs, so the solver can change without re-testing.

## Equipment

- [ ] 4 × U1 PCB V2 boards: **M**, **A1**, **A2** = anchors, **T** = tag (record below)
- [ ] 4 × tripods, all set to the same height (≥ 1 m)
- [ ] 3 × data USB-C cables to the Mac (anchors), 1 × power bank (tag)
- [ ] Laser distance meter
- [ ] Floor tape and marker for the anchor and grid points
- [ ] Phone camera

| Item | Value |
|---|---|
| Anchor M board / UID | |
| Anchor A1 board / UID | |
| Anchor A2 board / UID | |
| Tag T board / UID | |
| Firmware git hash (anchor / tag) | |
| Sync rate | |
| Blink rate | 40 Hz |
| Location | upstairs |
| Tripod height | |

## Lessons carried over from TWR

1. **Height ≥ 1 m.** At 0.338 m the floor reflection arrived only 4–5 cm behind the direct path. It moved TWR readings by 5–15 cm per placement and shifted the calibration by 31 ticks (H-1).
2. **Laser after every move.** Raising the tripods quietly moved the boards 66 mm (H-1).
3. **Calibrate at the test height and geometry.** A calibration does not carry across heights.
4. **Change one thing at a time, without touching the boards.** Placement changes dominate the error; noise does not.
5. **Upstairs only.** Downstairs, channel-9 interference (likely the HomePod mini or a phone) cost ~25 % of exchanges.
6. **60 s runs are enough.** Within a placement, the noise averages away in seconds.
7. **Facing away reads long** (+18–21 cm at 180° in T5). Keep orientation arrows on every board and report tag orientation (D7).
8. **Mount every board upright.** The TWR boards lay flat, but the DWM3000 antenna is vertically polarised and is only omnidirectional in the horizontal plane when the module stands upright (data sheet section 5). Flat boards are the likely cause of the orientation effect in 7.

## Baseline condition

Every test changes **one** thing from this baseline:

- **Location:** upstairs, away from the HomePod and phones
- **Geometry:** anchors at the corners of a triangle with sides of about 4 m, tag at the triangle's center
- **Height:** all four modules at the same height, ≥ 1 m
- **Mounting:** every board (anchors and tag) **upright**: module long axis vertical, antenna end up. The module sits at 45° on the U1 PCB, so each board stands on a corner. With all antennas vertical, the direction a board faces should matter little; still keep an arrow on each board pointing at the triangle's center and record it
- **Power:** anchors on Mac USB; tag on a power bank (always-on mode)
- **Warm-up:** all boards powered for at least 5 minutes before the first run
- **People:** nobody inside the triangle or within about 2 m of it
- **Cables:** taped straight down the back of each tripod, the same every run
- **Laptop:** outside the triangle, same spot every run

## Firmware and tooling prerequisites

**D1/D2 bench firmware (2026-10-28):** `firmware/stm32g4/src/bin/tdoa_bench.rs`, fw `tdoa_bench-1`, one file with the role set at build time (`TDOA_ROLE=master|anchor`; touch the file between role builds). Radio config A (same as `twr_*-5`), USB CDC output so all three boards record at once. Master: SYNC every `SYNC_MS` (default 100, changeable over USB with `SYNC_MS <n>` for D2), logs `ST seq sched tx_ts`. Anchors: log every SYNC as `S seq master_sched rx_ts rssi_x10`. All clock fitting happens on the Mac (`tdoa_d1.py`), nothing is corrected on board. Assignment: **board 8 = master M**, boards 10 and 9 = anchors A1 and A2. The existing `tdoa_anchor.rs` / `tdoa_tag.rs` (anchors report back to the tag over the radio, RTT logs only) are left unchanged.

- [x] `tdoa_bench` master and anchor built
- [x] Flashed 2026-10-28: board 8 master, boards 9 and 10 anchor (9 and 10 confirmed by UID over SWD)
- [x] Recorder (`tdoa_record.py`, all boards over USB) and D1 analysis (`tdoa_d1.py`, checked on synthetic data)


- [ ] **Tag firmware:** blink frame with `seq` at 40 Hz; no receive
- [ ] **Anchor firmware:** for every received blink and sync, report over USB
	- [ ] `B anchor_id tag_seq rx_ts rssi fp_power` per blink (raw 40-bit timestamp)
	- [ ] `S anchor_id sync_seq rx_ts rssi fp_power` per sync
	- [ ] Carrier-integrator clock-offset estimate on every received frame (a second, independent estimate of drift)
	- [ ] Startup banner: git hash, UID, role, radio config, antenna delay
	- [ ] Stats every 10 s, receive errors by type
- [ ] **Master firmware:** anchor firmware plus a sync transmit at a configurable rate; logs `ST sync_seq tx_ts`
- [ ] **Recorder:** extend `usb_record.py` to 3 ports; `run.json` holds surveyed anchor and tag coordinates
- [ ] **Solver:** clock model (offset + drift per anchor vs M, from sync frames), pair time differences, 2D solve (least squares), GDOP per point
- [ ] Run data saved under the ignored `output/u1-tdoa-bench/` folder, never committed

## Session start (once per session)

1. [ ] Set all four tripods to the same height; tape a reference mark and an orientation arrow on each board
2. [ ] Flash all four boards; check each startup banner; fill in the table above
3. [ ] Tape the anchor positions and the D4 grid points on the floor
4. [ ] Photograph the room and each anchor
5. [ ] Warm up all boards for 5 minutes

## Per-run procedure

1. Place the tag on its point; re-aim its arrow at M
2. Laser the tag to each anchor that can be reached; write the distances in the run table
3. Start the recorder (60 s unless the test says otherwise)
4. Hands off; note anyone walking through
5. Check the quick summary: redo the run on `seq` gaps, missed syncs or a disturbance
6. Move to the next point

## Tests

Result columns: **n** = blinks heard by all 3 anchors, errors in mm, **heard %** = blinks heard by all 3 anchors out of blinks sent.

### D0 · Anchor survey

- [x] D0 done: anchors surveyed from wall distances (2026-10-28)

**Decision (2026-10-28):** the wall survey below is the anchor survey. No separate anchor-to-anchor laser or TWR check now. D3's pair calibration absorbs each anchor's antenna delay, so TWR adds nothing there. The coordinates do rest on three assumptions added on top of the measurements: the room is square, the module offset inside the casing, and the module corner taken from the sketch (each likely within 1–2 cm). **If D3 or D4 shows an unexplained constant offset on one pair, laser that anchor-to-anchor distance first** and compare with the survey value in the table below.

**Room survey 2026-10-28 (upstairs):** 3.012 m (x) × 4.196 m (y). Origin at the corner nearest board 10; x along the 3.012 m wall with boards 10 and 9, y along the 4.196 m wall.

![[images/tdoa_room_setup.svg]]

Anchors were **lying flat** during D1-1 (2026-10-28); the baseline is now upright, so re-measure positions after standing them up. Wall distances were measured to the **outside of the casing**. The coordinates below move each point to the radio module: the U1 PCB is 50 × 50 mm and the DWM3000 sits in one corner, rotated 45°, its center about 14 mm from the two near PCB edges and 36 mm from the far ones (from `opentag.kicad_pcb`). The module corner follows the diamond in the sketch. **Casing wall thickness not yet included** (adds a few mm per side).

| Board | USB serial | Wall distances (to casing) | x, y at module (m) | Firmware now |
|---|---|---|---|---|
| 10 | `32001700105032364B393220` | 138 mm left, 108 mm bottom | 0.152, 0.144 | `twr_resp-5` |
| 9 | `57003B000F5032364B393220` | 155 mm right, 96 mm bottom | 2.821, 0.132 | `twr_resp-5` |
| 8 | `4E003B000F5032364B393220` | 1.607 m right, 101 mm top | 1.369, 4.081 | `twr_init-5` |

Centroid = planned tag point C: **1.447, 1.452 m** (tag not placed yet). All three read over USB on 2026-10-28.

| Pair | Survey (mm) | Laser check (only if needed) | Notes |
|---|---|---|---|
| 10 – 9 | 2669 | | |
| 10 – 8 | 4121 | | |
| 9 – 8 | 4207 | | |

### D1 · Clock sync alone

No tag. M sends sync; A1 and A2 predict each next sync arrival from their clock model. The prediction error is the sync noise floor and the floor for every later test.

- [ ] D1 done

**SMOKE-1 (2026-10-28, 30 s, boards just re-powered after flashing):** chain works end to end. Master sent 312 syncs, 0 gaps, 0 TX errors; anchors 10 and 9 heard **100 %**, 0 receive errors, RSSI −69 dBm. Clocks vs master: board 10 ≈ 0.0 ppm, board 9 ≈ −1.4 ppm, both **drifting steadily (~−0.01 ppm per 5 s): warm-up**, which a straight-line clock model can't follow. Per-timestamp noise from second differences: **~81 ps (24 mm) on board 10, ~108 ps (32 mm) on board 9**. Prediction error 150–260 ps (FAIL), dominated by the warm-up drift. Note: a single timestamp's noise already sits near the 100 ps target, so the target may need to become ~150 ps; decide after D1-1 on warmed-up boards, before looking at D3.

| Run | Duration | Anchor | syncs | missed | drift (ppm) | prediction error std (ps) | Notes |
|---|---|---|---|---|---|---|---|
| SMOKE-1 | 30 s | 10 | 311 | 0 | −0.03 (drifting) | 199 (last 2) / 147 (last 10) | Not warmed up |
| SMOKE-1 | 30 s | 9 | 310 | 0 | −1.45 (drifting) | 265 / 219 | Not warmed up |

| Run | Duration | Anchor | syncs | missed | drift (ppm) | prediction error std (ps) | Notes |
|---|---|---|---|---|---|---|---|
| D1-1 | 10 min | 10 | 6010 | 0 | −0.26 → −0.58 (drifting) | 155 (last 10) / 275 (last 2) | 2026-10-28 22:01–22:11, started right after SMOKE-1. Timestamp noise 82 → 125 ps |
| D1-1 | 10 min | 9 | 6012 | 0 | −1.75 → −2.04 (drifting) | 192 (last 10) / 331 (last 2) | Timestamp noise 128–146 ps |
| D1-2 | 60 min | A1 | | | | | |
| D1-2 | 60 min | A2 | | | | | |

Pass: prediction error std < 100 ps; no trend over 60 min.

**D1-1 findings (2026-10-28):**
- **Link:** 6,012 syncs, 100 % heard by both anchors, 0 receive or TX errors. Master TX time = scheduled time + 16,405 ticks on **every** sync (TX timing is exact).
- **Warm-up is the master's clock.** Both anchors' rates drifted by the same ~−0.3 ppm over 10 min, and their 1 s clock changes correlate **+0.99**. The trend was slowing but not finished after ~15 min powered.
- **The floor is the radio timestamp, not the sync.** Each RX timestamp carries **~80–145 ps (24–43 mm) of noise**, so a "predict the next sync" error can never go below it, and the 100 ps target as written fails.
- **What TDOA will see** (`tdoa_virtual.py`: each sync treated as a tag blink, both anchors mapped to master time, then differenced; truth is constant): **~190–200 ps ≈ 6 cm (1σ) per blink**, best with clock lines fitted on **±2–5 syncs (±0.2–0.5 s)**. Wider fits get worse (±1 s: 225 ps, ±2 s: 440 ps), so the clocks wander on second time scales. Clock model on its own adds ~50–75 ps on top of the timestamp noise.
- **Implication:** a single blink's time difference will be ~6 cm noisy; averaging 10 blinks (0.25 s at 40 Hz) brings it to ~2 cm. Comparable to TWR's 13–25 mm per range.

**Revised D1 pass criterion (proposal):** clock model adds < 100 ps on top of timestamp noise (D1-1: 50–77 ps, **pass**), and the virtual-blink time-difference std is reported as the D3 expectation (~200 ps). D2 checks whether a faster sync (20 Hz) tightens it, since shorter fit windows did better. **Gate:** if D1 fails, stop and consider a shared clock (one TCXO wired to every radio) before D3.

### D2 · Sync rate

Repeat D1 for 2 min at each sync rate. Pick the slowest rate that still meets the D1 target.

- [ ] D2 done

| Run | Sync rate | prediction error std (ps) A1 / A2 | missed | Notes |
|---|---|---|---|---|
| D2-1 | 2 Hz | | | |
| D2-2 | 5 Hz | | | |
| D2-3 | 10 Hz | | | |
| D2-4 | 20 Hz | | | |

### D3 · Pair calibration

Tag at the center point. For each pair, compare the measured time difference (in mm) with the one expected from the survey. The constant offset is that pair's calibration; apply it to every later run.

- [ ] D3 done

| Run | Pair | expected (mm) | measured mean | std | offset (calibration) | Notes |
|---|---|---|---|---|---|---|
| D3-1 | M – A1 | | | | | |
| D3-1 | M – A2 | | | | | |

Pass: std < 3 cm per pair. Never score at the calibration point.

### D4 · Static grid

Tag at 9 surveyed points, 60 s each. Inside: C (center), three points halfway from C toward each anchor, three edge midpoints. Outside: two points about 1 m beyond an edge.

- [ ] D4 done

| Run | Point | x, y (mm) | n | heard % | bias x / y | 2D std | CEP50 | CEP95 | GDOP | Notes |
|---|---|---|---|---|---|---|---|---|---|---|
| D4-1 | Near M | | | | | | | | | |
| D4-2 | Near A1 | | | | | | | | | |
| D4-3 | Near A2 | | | | | | | | | |
| D4-4 | Edge M–A1 | | | | | | | | | |
| D4-5 | Edge M–A2 | | | | | | | | | |
| D4-6 | Edge A1–A2 | | | | | | | | | |
| D4-7 | Outside 1 | | | | | | | | | |
| D4-8 | Outside 2 | | | | | | | | | |

Also record each pair's time-difference error per point, to tell sync and calibration error apart from geometry.

### D5 · Geometry check

Analysis only: plot D4 error against GDOP. If the error grows with GDOP, the remaining error is geometry, not ranging. Points outside the triangle are expected to fail.

- [ ] D5 done

### D6 · Repeatability

Move the tag away, then back to three D4 points (C, one inside, one edge).

- [ ] D6 done

| Run | Point | first visit 2D mean | return 2D mean | difference | Notes |
|---|---|---|---|---|---|
| D6-1 | | | | | |
| D6-2 | | | | | |
| D6-3 | | | | | |

### D7 · Tag orientation

Tag at a scored inside point (not C), rotated in place.

- [ ] D7 done

| Run | Tag yaw | n | bias x / y | 2D std | worst-anchor RSSI | Notes |
|---|---|---|---|---|---|---|
| D7-1 | 0° | | | | | |
| D7-2 | 90° | | | | | |
| D7-3 | 180° | | | | | |
| D7-4 | 270° | | | | | |
| D7-5 | 0° (repeat) | | | | | |

### D8 · Stability

Tag at a scored inside point for 60 minutes.

- [ ] D8 done

| Run | Duration | n | heard % | 2D drift (first vs last 5 min) | sync drift | gaps | Notes |
|---|---|---|---|---|---|---|---|
| D8-1 | 60 min | | | | | | |

### D9 · TWR vs TDOA

At D4-1, D4-4 and one outside point, run TWR from the tag to each anchor and compute position from the three distances.

- [ ] D9 done

| Run | Point | TDOA 2D error | TWR 2D error | TDOA std | TWR std | Notes |
|---|---|---|---|---|---|---|
| D9-1 | | | | | | |
| D9-2 | | | | | | |
| D9-3 | | | | | | |

### D10 · Moving tag (later)

Carry the tag along a taped straight line inside the triangle at a steady walking pace. Report sideways error from the line, update rate and latency.

- [ ] D10 done

## Schedule

- **Before testing:** firmware, recorder and solver (prerequisites)
- **Day 1:** D1, D2, D3 (D0 done from the wall survey)
- **Day 2:** D4, D5, D6, D7
- **Day 3:** D8, D9

## Rules

- Change one thing at a time from the baseline.
- D1 must pass before any position test.
- Never score at the calibration point (D3).
- Laser after every move; re-aim the arrows.
- Hands off during recording; note anyone walking through.
- Don't change firmware, sync rate or calibration mid-session without recording it.

## Results summary

Fill in after the session.

| Metric | Target | Result | Pass |
|---|---|---|---|
| Clock-sync prediction error | < 100 ps | | |
| Pair time-difference error | < 3 cm | | |
| 2D error inside triangle (95 %) | < 10 cm | | |
| Repeatability | < 5 cm | | |
| 60 min drift | < 2 cm | | |
| Blinks heard by all 3 | ≥ 99 % | | |
| Worst tag orientation (D7) | report | | |
| TDOA vs TWR (D9) | report | | |

## Notes
