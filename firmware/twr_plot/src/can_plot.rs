/// CAN FD throughput / latency visualizer
///
/// Reads probe-rs RTT output from stdin, parses CAN FD log lines,
/// and streams them into the Rerun viewer.
///
/// Usage (attach to RX board):
///   probe-rs attach --chip STM32G491CE target/thumbv7em-none-eabi/debug/can_fd_rx 2>&1 \
///     | cargo run --bin can_plot --manifest-path twr_plot/Cargo.toml
///
/// Parses lines of the form:
///   CANFD RX seq=N latency_us=N bytes=N
///   CANFD STAT elapsed_ms=N frames=N bytes=N kbps=N drops=N

use std::io::{self, BufRead};
use rerun::{RecordingStreamBuilder, blueprint};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rec = RecordingStreamBuilder::new("opentag/can_fd").spawn()?;

    blueprint::Blueprint::new(
        blueprint::Vertical::new([
            blueprint::TimeSeriesView::new("Latency (µs)")
                .with_origin("can/latency")
                .with_contents(["can/latency/**"])
                .into(),
            blueprint::TimeSeriesView::new("Throughput (kbps)")
                .with_origin("can/throughput")
                .with_contents(["can/throughput/**"])
                .into(),
            blueprint::TimeSeriesView::new("Drops")
                .with_origin("can/drops")
                .with_contents(["can/drops/**"])
                .into(),
        ])
    )
    .send(&rec, blueprint::BlueprintActivation {
        make_active: true,
        make_default: true,
    })?;

    // CANFD RX seq=N latency_us=N bytes=N
    let re_rx = regex::Regex::new(
        r"CANFD RX seq=(\d+) latency_us=(\d+) bytes=(\d+)"
    )?;

    // CANFD STAT elapsed_ms=N frames=N bytes=N kbps=N drops=N
    let re_stat = regex::Regex::new(
        r"CANFD STAT elapsed_ms=(\d+) frames=(\d+) bytes=(\d+) kbps=(\d+) drops=(\d+)"
    )?;

    eprintln!("can_plot: waiting for data — reset the RX board...");

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;

        if let Some(caps) = re_rx.captures(&line) {
            let seq: i64       = caps[1].parse()?;
            let latency_us: f64 = caps[2].parse()?;
            let bytes: f64      = caps[3].parse()?;

            rec.set_time_sequence("seq", seq);
            rec.log("can/latency/us", &rerun::Scalars::new([latency_us]))?;
            rec.log("can/latency/frame_bytes", &rerun::Scalars::new([bytes]))?;

            eprintln!("[seq={seq:6}] latency={latency_us:.0} µs  {bytes:.0} B");
        }

        if let Some(caps) = re_stat.captures(&line) {
            let elapsed_ms: i64 = caps[1].parse()?;
            let frames: f64     = caps[2].parse()?;
            let kbps: f64       = caps[4].parse()?;
            let drops: f64      = caps[5].parse()?;

            rec.set_time_sequence("elapsed_ms", elapsed_ms);
            rec.log("can/throughput/kbps", &rerun::Scalars::new([kbps]))?;
            rec.log("can/throughput/frames", &rerun::Scalars::new([frames]))?;
            rec.log("can/drops/total", &rerun::Scalars::new([drops]))?;

            eprintln!("[t={elapsed_ms:6}ms] {kbps:.0} kbps  {frames:.0} frames  {drops:.0} drops");
        }
    }

    Ok(())
}
