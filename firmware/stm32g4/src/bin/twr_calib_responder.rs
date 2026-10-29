#![no_std]
#![no_main]

// TWR Calibration Responder — Antenna Delay Calibration for DS-TWR
//
// Purpose:
//   Measures the systematic error in the DS-TWR range estimate by running
//   N_SAMPLES exchanges at a precisely known distance and printing the
//   average raw ToF (in DW3000 time units), the expected ToF, and the
//   suggested CALIB_OFFSET_DTU to apply in twr_responder.rs.
//
// Procedure:
//   1. Place this device and a twr_initiator device exactly KNOWN_DISTANCE_MM
//      apart on a flat surface with clear line of sight.
//   2. Flash twr_initiator on the far unit, this binary on the near unit.
//   3. Watch RTT output (probe-rs / defmt-print).
//   4. After each N_SAMPLES exchanges you will see:
//
//        CALIB[100] avg_tof=43 dtu  dist_raw=202mm  expected_tof=42 dtu
//        => CALIB_OFFSET_DTU = 1  (add this constant to twr_responder.rs)
//
//   5. Repeat a few rounds to confirm the value is stable, then copy it.
//
// Applying the result in twr_responder.rs:
//   In compute_distance_mm(), subtract CALIB_OFFSET_DTU from tof_dtu before
//   the distance conversion:
//
//     let tof_dtu = num / den;
//     let tof_dtu = tof_dtu.saturating_sub(CALIB_OFFSET_DTU as u128);
//     let dist = tof_dtu.checked_mul(299_792_458)? / 63_897_600;
//
// Pinout:
//   PA3=RST  PA4=CS  PA5=SCK  PA6=MISO  PA7=MOSI

use defmt::*;
use embassy_executor::Spawner;
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::rcc::{Hse, HseMode, Pll, PllMul, PllPreDiv, PllRDiv, PllSource, Sysclk};
use embassy_stm32::spi::{self, Spi};
use embassy_stm32::time::Hertz;
use embassy_stm32::{bind_interrupts, dma, peripherals};
use embassy_time::{Delay, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;
use dw3000_ng::{Config, DW3000, configs::UwbChannel, hl::SendTime};
use dw3000_ng::time::{Instant as DwInstant, Duration as DwDuration};
use nb;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    DMA1_CHANNEL2 => dma::InterruptHandler<peripherals::DMA1_CH2>;
    DMA1_CHANNEL3 => dma::InterruptHandler<peripherals::DMA1_CH3>;
});

// ── Calibration parameters ────────────────────────────────────────────────────
/// True physical separation between the two devices, in millimetres.
/// Measure this carefully with a tape measure; accuracy here determines
/// the quality of the calibration.
const KNOWN_DISTANCE_MM: u64 = 200;

/// How many successful exchanges to average per calibration window.
const N_SAMPLES: u32 = 100;
// ─────────────────────────────────────────────────────────────────────────────

const POLL_ID:  &[u8] = &[0xDE, 0xCA, 0x01];
const RESP_ID:  &[u8] = &[0xDE, 0xCA, 0x02];
const FINAL_ID: &[u8] = &[0xDE, 0xCA, 0x03];

const TREPLY1_NS: u32 = 10_000_000; // 10 ms

fn pack_ts(ts: u64, buf: &mut [u8]) {
    let b = ts.to_le_bytes();
    buf[..5].copy_from_slice(&b[..5]);
}

fn unpack_ts(buf: &[u8]) -> u64 {
    u64::from_le_bytes([buf[0], buf[1], buf[2], buf[3], buf[4], 0, 0, 0])
}

fn align512(raw: u64) -> DwInstant {
    DwInstant::new((raw + 511) & !511u64).unwrap()
}

/// APS013 DS-TWR product formula.  Returns raw tof in DTU (no antenna correction).
fn compute_tof_dtu(
    t_round1: u64, t_round2: u64,
    t_reply1: u64, t_reply2: u64,
) -> Option<u64> {
    let num = (t_round1 as u128)
        .checked_mul(t_round2 as u128)?
        .checked_sub((t_reply1 as u128).checked_mul(t_reply2 as u128)?)?;
    let den = t_round1 as u128 + t_round2 as u128 + t_reply1 as u128 + t_reply2 as u128;
    if den == 0 { return None; }
    Some((num / den) as u64)
}

/// Convert DTU to millimetres using the DW3000 clock constant.
fn dtu_to_mm(dtu: u64) -> u64 {
    dtu * 299_792_458 / 63_897_600
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let mut config = embassy_stm32::Config::default();
    {
        config.rcc.hse = Some(Hse { freq: Hertz(8_000_000), mode: HseMode::Oscillator });
        config.rcc.pll = Some(Pll {
            source: PllSource::HSE,
            prediv: PllPreDiv::DIV2,
            mul: PllMul::MUL85,
            divp: None, divq: None,
            divr: Some(PllRDiv::DIV2),
        });
        config.rcc.sys = Sysclk::PLL1_R;
    }
    let p = embassy_stm32::init(config);

    info!("TWR Calibration Responder starting");
    info!("Known distance: {} mm  |  Samples per window: {}", KNOWN_DISTANCE_MM, N_SAMPLES);

    // Expected ToF for the known distance (rounded to nearest DTU)
    // expected_dtu = KNOWN_DISTANCE_MM * 63_897_600 / 299_792_458
    let expected_tof_dtu: u64 = KNOWN_DISTANCE_MM * 63_897_600 / 299_792_458;
    info!("Expected ToF at {} mm = {} DTU", KNOWN_DISTANCE_MM, expected_tof_dtu);

    let mut rst = Output::new(p.PA3, Level::Low, Speed::Low);
    Timer::after_millis(10).await;
    rst.set_high();
    Timer::after_millis(5).await;

    let mut spi_config = spi::Config::default();
    spi_config.frequency = Hertz(4_000_000);
    let spi = Spi::new(p.SPI1, p.PA5, p.PA7, p.PA6, p.DMA1_CH3, p.DMA1_CH2, Irqs, spi_config);
    let cs = Output::new(p.PA4, Level::High, Speed::High);
    let spi_dev = ExclusiveDevice::new(spi, cs, Delay).unwrap();

    let dw3000 = DW3000::new(spi_dev);
    let dw3000 = dw3000.init().await.expect("DW3000 init failed");
    let dw_config = Config { channel: UwbChannel::Channel9, ..Config::default() };
    let mut dw3000 = dw3000.config(dw_config, Delay).await.expect("DW3000 config failed");

    for pin in 0..4u8 {
        dw3000.gpio_config_mode(pin, 1).await.expect("gpio mode failed");
    }
    dw3000.gpio_config_clocks().await.expect("gpio clocks failed");

    info!("DW3000 ready — collecting calibration samples...");
    info!("NOTE: raw tof values will be MUCH larger than expected (~12000+ DTU for");
    info!("  a 200mm setup) due to the crystal frequency offset between the two");
    info!("  DW3000 chips (~20 ppm × 10ms reply = ~12800 DTU systematic bias).");
    info!("  This is NORMAL.  The printed CALIB_OFFSET_DTU corrects for it.");

    // Calibration accumulators
    let mut sum_tof_dtu: u128 = 0;
    let mut good_samples: u32 = 0;
    let mut round: u32 = 1;

    loop {
        // ── MSG 1: Receive Poll ──────────────────────────────────────────────
        let mut receiving = dw3000.receive(dw_config).await.expect("receive failed");
        let mut buf = [0u8; 128];

        let poll_result = loop {
            match receiving.r_wait_buf(&mut buf).await {
                Ok((l, ts, _)) => break Some((l, ts)),
                Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                Err(nb::Error::Other(e)) => {
                    let _ = e;
                    warn!("RX error on poll, restarting");
                    break None;
                }
            }
        };
        dw3000 = receiving.finish_receiving().await
            .unwrap_or_else(|(_, _)| core::panic!("finish_receiving"));
        let Some((poll_len, t2)) = poll_result else { continue; };

        let frame = &buf[..poll_len];
        let poll_pos = frame.windows(3).position(|w| w == POLL_ID);

        let seq = match poll_pos {
            Some(pos) => frame.get(pos + 3).copied().unwrap_or(0),
            None => {
                info!("Unknown frame (len={}), ignoring", poll_len);
                continue;
            }
        };

        // ── MSG 2: Send Response ─────────────────────────────────────────────
        let t3 = align512((t2 + DwDuration::from_nanos(TREPLY1_NS)).value());

        let mut resp = [0u8; 14];
        resp[0] = RESP_ID[0]; resp[1] = RESP_ID[1]; resp[2] = RESP_ID[2];
        resp[3] = seq;
        pack_ts(t2.value(), &mut resp[4..]);
        pack_ts(t3.value(), &mut resp[9..]);

        let mut sending = dw3000.send(&resp, SendTime::Delayed(t3), dw_config).await
            .expect("response send failed");

        let resp_ok = loop {
            match sending.s_wait().await {
                Ok(_) => break true,
                Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                Err(nb::Error::Other(_)) => {
                    warn!("Response TX too late (seq={}) — increase TREPLY1_NS", seq);
                    break false;
                }
            }
        };
        dw3000 = sending.finish_sending().await
            .unwrap_or_else(|(_, _)| core::panic!("finish_sending"));

        if !resp_ok { continue; }

        // ── MSG 3: Receive Final ─────────────────────────────────────────────
        let mut receiving = dw3000.receive(dw_config).await.expect("receive failed");

        let deadline = embassy_time::Instant::now() + embassy_time::Duration::from_millis(200);
        let final_data = loop {
            if embassy_time::Instant::now() > deadline { break None; }
            match receiving.r_wait_buf(&mut buf).await {
                Ok((l, ts, _)) => break Some((l, ts)),
                Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                Err(nb::Error::Other(_)) => { error!("RX error on final"); break None; }
            }
        };
        dw3000 = receiving.finish_receiving().await
            .unwrap_or_else(|(_, _)| core::panic!("finish_receiving"));

        let Some((final_len, t6)) = final_data else {
            warn!("seq={} no Final (timeout) — skipping sample", seq);
            continue;
        };

        let frame = &buf[..final_len];
        let Some(pos) = frame.windows(3).position(|w| w == FINAL_ID) else {
            warn!("seq={} unexpected frame instead of Final", seq);
            continue;
        };
        let data = &frame[pos..];
        if data.len() < 19 {
            warn!("seq={} Final too short ({})", seq, data.len());
            continue;
        }

        let t1 = unpack_ts(&data[4..]);
        let t4 = unpack_ts(&data[9..]);
        let t5 = unpack_ts(&data[14..]);

        let t_round1 = DwInstant::new(t4).unwrap().duration_since(DwInstant::new(t1).unwrap()).value();
        let t_round2 = t6.duration_since(t3).value();
        let t_reply1 = t3.duration_since(t2).value();
        let t_reply2 = DwInstant::new(t5).unwrap().duration_since(DwInstant::new(t4).unwrap()).value();

        let Some(tof_dtu) = compute_tof_dtu(t_round1, t_round2, t_reply1, t_reply2) else {
            warn!("seq={} ToF compute failed — skipping sample", seq);
            continue;
        };

        let dist_raw_mm = dtu_to_mm(tof_dtu);
        sum_tof_dtu += tof_dtu as u128;
        good_samples += 1;

        // Log every sample so the user can watch convergence in real time
        info!(
            "  sample {}/{}: tof={} dtu  dist_raw={}mm",
            good_samples, N_SAMPLES,
            tof_dtu, dist_raw_mm
        );

        // ── Print calibration result every N_SAMPLES ─────────────────────────
        if good_samples >= N_SAMPLES {
            let avg_tof = (sum_tof_dtu / good_samples as u128) as u64;
            let avg_dist_mm = dtu_to_mm(avg_tof);

            // Signed offset: positive means the raw reading is too high
            // (antenna delay adds apparent distance); negative means too low.
            let offset_dtu: i64 = avg_tof as i64 - expected_tof_dtu as i64;
            let offset_mm:  i64 = avg_dist_mm as i64 - KNOWN_DISTANCE_MM as i64;

            info!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
            info!("CALIB RESULT — round {}", round);
            info!("  Samples       : {}", good_samples);
            info!("  Known distance: {} mm", KNOWN_DISTANCE_MM);
            info!("  Avg raw dist  : {} mm  ({}{} mm error)",
                avg_dist_mm,
                if offset_mm >= 0 { "+" } else { "" },
                offset_mm
            );
            info!("  Avg ToF (raw) : {} DTU", avg_tof);
            info!("  Expected ToF  : {} DTU", expected_tof_dtu);
            info!("  => CALIB_OFFSET_DTU = {}{}", if offset_dtu >= 0 { "+" } else { "" }, offset_dtu);
            info!("  Apply in twr_responder.rs:");
            info!("    let tof_dtu = tof_dtu.saturating_sub({} as u128);", offset_dtu);
            info!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

            // Reset for the next window
            sum_tof_dtu = 0;
            good_samples = 0;
            round += 1;

            Timer::after_millis(500).await;
        }
    }
}
