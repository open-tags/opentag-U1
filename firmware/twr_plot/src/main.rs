/// opentags TWR distance visualizer
///
/// Reads probe-rs RTT output from stdin, parses distance log lines,
/// and streams them into the Rerun viewer.
///
/// Usage:
///   probe-rs attach --chip STM32G491CE 2>&1 | cargo run
///
/// Parses responder lines of the form:
///   seq=N distance=X.XXX m  [Rr1=... Rr2=... Rp1=... Rp2=...]

use std::io::{self, BufRead};
use rerun::{RecordingStreamBuilder, blueprint};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rec = RecordingStreamBuilder::new("opentag/twr").spawn()?;

    // Configure the viewer layout automatically on connect
    blueprint::Blueprint::new(
        blueprint::TimeSeriesView::new("Distance")
            .with_origin("twr")
            .with_contents(["twr/**"])
    )
    .send(&rec, blueprint::BlueprintActivation {
        make_active: true,
        make_default: true,
    })?;

    // seq=(\d+) distance=(\d+\.\d+) m  [Rr1=(\d+) Rr2=(\d+) Rp1=(\d+) Rp2=(\d+)]
    let re = regex::Regex::new(
        r"seq=(\d+)\s+distance=(\d+\.\d+)\s+m(?:\s+\[Rr1=(\d+)\s+Rr2=(\d+)\s+Rp1=(\d+)\s+Rp2=(\d+)\])?"
    )?;

    eprintln!("twr_plot: waiting for data — reset the responder board...");

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;

        if let Some(caps) = re.captures(&line) {
            let seq: i64  = caps[1].parse()?;
            let dist_m: f64 = caps[2].parse()?;

            rec.set_time_sequence("seq", seq);
            rec.log("twr/distance_m", &rerun::Scalars::new([dist_m]))?;

            // Debug: individual timestamp deltas
            if let (Some(rr1), Some(rr2), Some(rp1), Some(rp2)) =
                (caps.get(3), caps.get(4), caps.get(5), caps.get(6))
            {
                let ticks_to_m = |t: u64| t as f64 * 299_792_458.0 / 63_897_600_000.0;
                rec.log("twr/debug/T_round1_m", &rerun::Scalars::new([ticks_to_m(rr1.as_str().parse()?)]))?;
                rec.log("twr/debug/T_round2_m", &rerun::Scalars::new([ticks_to_m(rr2.as_str().parse()?)]))?;
                rec.log("twr/debug/T_reply1_m", &rerun::Scalars::new([ticks_to_m(rp1.as_str().parse()?)]))?;
                rec.log("twr/debug/T_reply2_m", &rerun::Scalars::new([ticks_to_m(rp2.as_str().parse()?)]))?;
            }

            eprintln!("[seq={seq:4}] {dist_m:.3} m");
        }
    }

    Ok(())
}
