/// opentags TDOA visualizer
///
/// Reads tdoa_tag RTT/defmt output from stdin, parses position lines, and
/// streams anchors and the latest tag position into Rerun.
///
/// Usage:
///
///   cd firmware/rust_bringup/stm32g4
///   probe-rs attach --chip STM32G491CE target/thumbv7em-none-eabi/debug/tdoa_tag 2>&1 \
///     | cargo run --manifest-path ../twr_plot/Cargo.toml \
///         --target "$(rustc -vV | awk '/^host:/ { print $2 }')" \
///         --bin tdoa_plot
///
/// Parsed tag lines:
///
///   seq=9 pos=(1400 mm, 723 mm)  tdoa=(38 64 dtu)
///   seq=22 invalid TDOA=(-16362 -16317 dtu): outside anchor geometry

use std::io::{self, BufRead};
use std::thread;
use std::time::Duration;

use rerun::{blueprint, RecordingStreamBuilder};

const ANCHORS: [(f32, f32); 3] = [
    (0.0, 0.0),
    (3000.0, 0.0),
    (1500.0, 2598.0),
];

const ANCHOR_LABELS: [&str; 3] = ["A0", "A1", "A2"];
const CENTROID: (f32, f32) = (1500.0, 866.0);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let echo_raw = std::env::args().any(|arg| arg == "--echo-raw");
    let rec = RecordingStreamBuilder::new("opentag/tdoa").spawn()?;

    blueprint::Blueprint::new(
        blueprint::Spatial2DView::new("TDOA Live Position")
            .with_origin("world")
            .with_contents(["world/**"])
    )
    .send(
        &rec,
        blueprint::BlueprintActivation {
            make_active: true,
            make_default: true,
        },
    )?;

    let anchor_positions: Vec<[f32; 2]> = ANCHORS.iter().map(|&(x, y)| [x, y]).collect();
    rec.log_static(
        "world/anchors",
        &rerun::Points2D::new(anchor_positions.iter().map(|p| (p[0], p[1])))
            .with_radii([45.0_f32])
            .with_colors([[0u8, 120, 255, 255]; 3])
            .with_labels(ANCHOR_LABELS),
    )?;

    rec.log_static(
        "world/centroid",
        &rerun::Points2D::new([(CENTROID.0, CENTROID.1)])
            .with_radii([28.0_f32])
            .with_colors([[255u8, 190, 0, 255]])
            .with_labels(["centroid"]),
    )?;

    rec.log_static(
        "world/anchor_outline",
        &rerun::LineStrips2D::new([vec![
            ANCHORS[0], ANCHORS[1], ANCHORS[2], ANCHORS[0],
        ]])
        .with_colors([[0u8, 120, 255, 120]]),
    )?;

    let fix_re = regex::Regex::new(
        r"seq=(\d+)\s+pos=\((-?\d+)\s+mm,\s+(-?\d+)\s+mm\)\s+tdoa=\((-?\d+)\s+(-?\d+)\s+dtu\)",
    )?;
    let invalid_re = regex::Regex::new(
        r"seq=(\d+)\s+invalid TDOA=\((-?\d+)\s+(-?\d+)\s+dtu\)",
    )?;

    eprintln!("tdoa_plot: waiting for tdoa_tag log lines...");

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        if echo_raw {
            eprintln!("raw: {line}");
        }

        if let Some(caps) = fix_re.captures(&line) {
            let seq: i64 = caps[1].parse()?;
            let x: f32 = caps[2].parse()?;
            let y: f32 = caps[3].parse()?;
            let tdoa10: f64 = caps[4].parse()?;
            let tdoa20: f64 = caps[5].parse()?;

            rec.set_time_sequence("seq", seq);

            rec.log("world/tag/latest", &rerun::Clear::flat())?;
            rec.log(
                "world/tag/latest",
                &rerun::Points2D::new([(x, y)])
                    .with_radii([55.0_f32])
                    .with_colors([[255u8, 60, 60, 255]])
                    .with_labels([format!("{x:.0}, {y:.0}")]),
            )?;

            eprintln!("seq={seq:4} pos=({x:.0}, {y:.0}) mm  tdoa=({tdoa10:.0}, {tdoa20:.0}) dtu");
            continue;
        }

        if let Some(caps) = invalid_re.captures(&line) {
            let seq: i64 = caps[1].parse()?;
            let tdoa10: f64 = caps[2].parse()?;
            let tdoa20: f64 = caps[3].parse()?;

            eprintln!("seq={seq:4} invalid tdoa=({tdoa10:.0}, {tdoa20:.0}) dtu");
        }
    }

    eprintln!("tdoa_plot: stdin ended; keeping Rerun open. Press Ctrl-C to quit.");
    loop {
        thread::sleep(Duration::from_secs(1));
    }
}
