/// UWB Trilateration Visualizer
///
/// Reads the tag's RTT output from stdin, parses RANGE lines,
/// computes 2D position via trilateration, and streams the result
/// into the Rerun viewer as a live 2D spatial plot.
///
/// Usage (two options):
///
///   # Option A: direct pipe from probe-rs
///   cd stm32g4
///   cargo run --bin twr_tag 2>&1 | cargo run --manifest-path ../twr_plot/Cargo.toml --bin twr_locate
///
///   # Option B: attach to a running board
///   probe-rs attach --chip STM32G491CE 2>&1 | cargo run --bin twr_locate
///
/// ── Anchor positions ────────────────────────────────────────────────────────
/// Edit ANCHORS to match your physical setup (mm, measured from Anchor 1).
/// Default: equilateral triangle, 2 m sides.
///
///          A3 (1000, 1732)
///         /  \
///        /    \
///       /      \
///   A1 (0,0)──A2 (2000,0)
/// ─────────────────────────────────────────────────────────────────────────────

use std::io::{self, BufRead};
use rerun::RecordingStreamBuilder;

// ── Configure anchor positions here (mm) ─────────────────────────────────────
//
//   A3 (-200, 200)
//   |
//   | 200 mm
//   |
//   A2 (-200, 0) ──── 200 mm ──── A1 (0, 0)
//
const ANCHORS: [(f32, f32); 3] = [
    (  0.0,   0.0), // Anchor 1 — bottom right
    (-200.0,  0.0), // Anchor 2 — bottom left
    (-200.0, 200.0), // Anchor 3 — top left
];
// ─────────────────────────────────────────────────────────────────────────────

/// Linear trilateration from 3 distances to ANCHORS.
/// Solves the 2×2 system derived by subtracting circle 1 from circles 2 and 3.
/// Returns None if the anchor geometry is degenerate.
fn trilaterate(d: [f32; 3]) -> Option<(f32, f32)> {
    let (x1, y1) = ANCHORS[0];
    let (x2, y2) = ANCHORS[1];
    let (x3, y3) = ANCHORS[2];
    let (d1, d2, d3) = (d[0], d[1], d[2]);

    // 2 × A × [x; y] = b
    let a11 = 2.0 * (x2 - x1);
    let a12 = 2.0 * (y2 - y1);
    let a21 = 2.0 * (x3 - x1);
    let a22 = 2.0 * (y3 - y1);

    let b1 = d1 * d1 - d2 * d2 + x2 * x2 - x1 * x1 + y2 * y2 - y1 * y1;
    let b2 = d1 * d1 - d3 * d3 + x3 * x3 - x1 * x1 + y3 * y3 - y1 * y1;

    let det = a11 * a22 - a12 * a21;
    if det.abs() < 1.0 { return None; } // anchors too close to collinear

    let x = (b1 * a22 - b2 * a12) / det;
    let y = (a11 * b2 - a21 * b1) / det;
    Some((x, y))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rec = RecordingStreamBuilder::new("opentag/locate").spawn()?;

    // ── Log static anchor positions (once) ───────────────────────────────────
    let anchor_positions: Vec<[f32; 2]> = ANCHORS.iter()
        .map(|&(x, y)| [x, y])
        .collect();

    rec.log_static(
        "world/anchors",
        &rerun::Points2D::new(anchor_positions.iter().map(|p| (p[0], p[1])))
            .with_radii([30.0_f32])
            .with_colors([[0u8, 120, 255, 255]; 3])
            .with_labels(["A1", "A2", "A3"]),
    )?;

    // ── Parse stdin and update tag position ──────────────────────────────────
    let re = regex::Regex::new(r"RANGE a1=(\d+) a2=(\d+) a3=(\d+)")?;

    let mut frame: i64 = 0;
    let mut trail: Vec<[f32; 2]> = Vec::new();

    eprintln!("twr_locate: waiting for RANGE lines — start twr_tag...");

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;

        let Some(caps) = re.captures(&line) else { continue };

        let d1: f32 = caps[1].parse()?;
        let d2: f32 = caps[2].parse()?;
        let d3: f32 = caps[3].parse()?;

        // Skip if any anchor hasn't reported yet (distance = 0)
        if d1 == 0.0 || d2 == 0.0 || d3 == 0.0 { continue; }

        let Some((x, y)) = trilaterate([d1, d2, d3]) else {
            eprintln!("trilaterate failed — check anchor geometry");
            continue;
        };

        rec.set_time_sequence("frame", frame);
        frame += 1;

        // Tag position
        rec.log(
            "world/tag",
            &rerun::Points2D::new([(x, y)])
                .with_radii([20.0_f32])
                .with_colors([[255u8, 50, 50, 255]]),
        )?;

        // Rolling trail (last 100 positions)
        trail.push([x, y]);
        if trail.len() > 100 { trail.remove(0); }
        if trail.len() > 1 {
            rec.log(
                "world/trail",
                &rerun::LineStrips2D::new([trail.iter().map(|p| (p[0], p[1])).collect::<Vec<_>>()])
                    .with_colors([[255u8, 100, 100, 80]]),
            )?;
        }

        // Range lines from each anchor to the tag
        let range_paths = ["world/range_1", "world/range_2", "world/range_3"];
        let dist_paths  = ["distances/A1",  "distances/A2",  "distances/A3"];
        for (i, &(ax, ay)) in ANCHORS.iter().enumerate() {
            let d = [d1, d2, d3][i];
            rec.log(
                range_paths[i],
                &rerun::LineStrips2D::new([vec![(ax, ay), (x, y)]])
                    .with_colors([[100u8, 200, 100, 60]]),
            )?;
            rec.log(
                dist_paths[i],
                &rerun::Scalars::new([d as f64]),
            )?;
        }

        eprintln!("pos=({:.0}, {:.0}) mm  d1={:.0} d2={:.0} d3={:.0}", x, y, d1, d2, d3);
    }

    Ok(())
}
