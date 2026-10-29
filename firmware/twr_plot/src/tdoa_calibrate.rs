/// Estimate opentags TDOA bias constants from logs collected at a known point.
///
/// Usage:
///
///   cargo run --manifest-path ../twr_plot/Cargo.toml --bin tdoa_calibrate -- 1500 866 < log.txt
///
/// The two printed bias values are intended for:
///   TDOA_10_BIAS_DTU = measured(A1-A0) - expected(A1-A0)
///   TDOA_20_BIAS_DTU = measured(A2-A0) - expected(A2-A0)

use std::io::{self, BufRead};

const AX: [f64; 3] = [0.0, 3000.0, 1500.0];
const AY: [f64; 3] = [0.0, 0.0, 2598.0];
const DTU_TO_MM: f64 = 299_792_458.0 / 63_897_600.0;

fn distance_mm(x: f64, y: f64, anchor: usize) -> f64 {
    let dx = x - AX[anchor];
    let dy = y - AY[anchor];
    (dx * dx + dy * dy).sqrt()
}

fn median(values: &mut [i64]) -> i64 {
    values.sort_unstable();
    values[values.len() / 2]
}

fn mean(values: &[i64]) -> f64 {
    values.iter().map(|&v| v as f64).sum::<f64>() / values.len() as f64
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let Some(x_arg) = args.next() else {
        eprintln!("usage: tdoa_calibrate X_MM Y_MM < tdoa_tag.log");
        std::process::exit(2);
    };
    let Some(y_arg) = args.next() else {
        eprintln!("usage: tdoa_calibrate X_MM Y_MM < tdoa_tag.log");
        std::process::exit(2);
    };

    let x: f64 = x_arg.parse()?;
    let y: f64 = y_arg.parse()?;

    let d0 = distance_mm(x, y, 0);
    let expected_10 = ((distance_mm(x, y, 1) - d0) / DTU_TO_MM).round() as i64;
    let expected_20 = ((distance_mm(x, y, 2) - d0) / DTU_TO_MM).round() as i64;

    let fix_re = regex::Regex::new(
        r"seq=(\d+)\s+pos=\((-?\d+)\s+mm,\s+(-?\d+)\s+mm\)\s+tdoa=\((-?\d+)\s+(-?\d+)\s+dtu\)(?:\s+raw=\((-?\d+)\s+(-?\d+)\s+dtu\))?",
    )?;

    let mut measured_10 = Vec::new();
    let mut measured_20 = Vec::new();

    for line in io::stdin().lock().lines() {
        let line = line?;
        let Some(caps) = fix_re.captures(&line) else {
            continue;
        };

        // Prefer raw values if logs came from filtered firmware; otherwise use tdoa.
        let t10 = caps
            .get(6)
            .or_else(|| caps.get(4))
            .expect("tdoa capture")
            .as_str()
            .parse::<i64>()?;
        let t20 = caps
            .get(7)
            .or_else(|| caps.get(5))
            .expect("tdoa capture")
            .as_str()
            .parse::<i64>()?;

        measured_10.push(t10);
        measured_20.push(t20);
    }

    if measured_10.is_empty() {
        eprintln!("no tdoa_tag position lines found on stdin");
        std::process::exit(1);
    }

    let mean_10 = mean(&measured_10);
    let mean_20 = mean(&measured_20);
    let med_10 = median(&mut measured_10);
    let med_20 = median(&mut measured_20);

    let bias_10 = med_10 - expected_10;
    let bias_20 = med_20 - expected_20;

    println!("known_position=({x:.0}, {y:.0}) mm");
    println!("expected_tdoa=({expected_10} {expected_20}) dtu");
    println!("measured_mean=({mean_10:.1} {mean_20:.1}) dtu");
    println!("measured_median=({med_10} {med_20}) dtu");
    println!();
    println!("Paste into tdoa_tag.rs:");
    println!("const TDOA_10_BIAS_DTU: i64 = {bias_10};");
    println!("const TDOA_20_BIAS_DTU: i64 = {bias_20};");

    Ok(())
}
