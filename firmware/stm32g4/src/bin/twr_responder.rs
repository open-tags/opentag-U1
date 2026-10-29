#![no_std]
#![no_main]

// TWR Responder — Qorvo APS013 Double-Sided Two-Way Ranging (DS-TWR)
//
// 3-message exchange:
//   Msg 1  Poll     Initiator → Responder    seq                     (4 B)
//   Msg 2  Response Responder → Initiator    seq + T2(5B) + T3(5B)   (14 B)
//   Msg 3  Final    Initiator → Responder    seq + T1(5B) + T4(5B) + T5(5B) (19 B)
//
// This side holds T2, T3, T6 and extracts T1, T4, T5 from the Final message.
// All 6 timestamps are then used to compute distance via APS013 product formula:
//
//   T_round1 = T4 - T1   (initiator: Poll TX → Response RX)
//   T_round2 = T6 - T3   (responder: Response TX → Final RX)
//   T_reply1 = T3 - T2   (responder: Poll RX → Response TX)
//   T_reply2 = T5 - T4   (initiator: Response RX → Final TX)
//
//   tof_dtu = (T_round1 × T_round2 − T_reply1 × T_reply2)
//             ─────────────────────────────────────────────
//             (T_round1 + T_round2 + T_reply1 + T_reply2)
//
//   distance_mm = tof_dtu × 299_792_458 / 63_897_600
//
// Pinout:
//   PA3=RST  PA4=CS  PA5=SCK  PA6=MISO  PA7=MOSI
//   PA0=Poll RX LED  PA1=distance OK LED  PA8=miss/error LED

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

const POLL_ID:   &[u8] = &[0xDE, 0xCA, 0x01];
const RESP_ID:   &[u8] = &[0xDE, 0xCA, 0x02];
const FINAL_ID:  &[u8] = &[0xDE, 0xCA, 0x03];

// Delay from Poll RX (T2) to Response TX (T3).
// Must exceed finish_receiving() + send() SPI overhead (~6 ms).
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

/// Antenna delay calibration offset in DW3000 time units (DTU).
///
/// Run twr_calib_responder.rs at a known distance (e.g. 200 mm) and copy
/// the printed CALIB_OFFSET_DTU value here.  Positive values reduce the
/// reported distance (raw reading is too high); negative values increase it.
/// Set to 0 until calibrated.
const CALIB_OFFSET_DTU: i64 = 12241;

/// APS013 DS-TWR product formula. Returns distance in mm.
/// Uses u128 internally to prevent overflow in the cross-products.
fn compute_distance_mm(
    t_round1: u64, t_round2: u64,
    t_reply1: u64, t_reply2: u64,
) -> Option<u64> {
    let num = (t_round1 as u128)
        .checked_mul(t_round2 as u128)?
        .checked_sub((t_reply1 as u128).checked_mul(t_reply2 as u128)?)?;
    let den = t_round1 as u128 + t_round2 as u128 + t_reply1 as u128 + t_reply2 as u128;
    if den == 0 { return None; }
    let tof_dtu = num / den;
    // Apply antenna delay calibration offset
    let tof_dtu = (tof_dtu as i128 - CALIB_OFFSET_DTU as i128).max(0) as u128;
    // distance_mm = tof_dtu × 299_792_458 / 63_897_600
    let dist = tof_dtu.checked_mul(299_792_458)? / 63_897_600;
    Some(dist as u64)
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

    info!("TWR Responder (DS-TWR) starting");

    let mut poll_led = Output::new(p.PA0, Level::Low, Speed::Low);
    let mut ok_led = Output::new(p.PA1, Level::Low, Speed::Low);
    let mut err_led = Output::new(p.PA8, Level::Low, Speed::Low);

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

    info!("DW3000 ready — listening for polls...");

    loop {
        // ── MSG 1: Receive Poll ──────────────────────────────────────────────
        let mut receiving = dw3000.receive(dw_config).await.expect("receive failed");
        let mut buf = [0u8; 128];

        let poll_result = loop {
            match receiving.r_wait_buf(&mut buf).await {
                Ok((l, ts, _)) => break Some((l, ts)),
                Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                Err(nb::Error::Other(_)) => break None,
            }
        };
        dw3000 = receiving.finish_receiving().await.unwrap_or_else(|(_, _)| core::panic!("finish_receiving"));

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
        poll_led.set_high();

        // ── MSG 2: Send Response ─────────────────────────────────────────────
        // T3 = T2 + TREPLY1, aligned to 512-tick boundary
        let t3 = align512((t2 + DwDuration::from_nanos(TREPLY1_NS)).value());

        let mut resp = [0u8; 14];
        resp[0] = RESP_ID[0]; resp[1] = RESP_ID[1]; resp[2] = RESP_ID[2];
        resp[3] = seq;
        pack_ts(t2.value(), &mut resp[4..]);
        pack_ts(t3.value(), &mut resp[9..]);

        let mut sending = dw3000.send(&resp, SendTime::Delayed(t3), dw_config).await.expect("response send failed");

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
        dw3000 = sending.finish_sending().await.unwrap_or_else(|(_, _)| core::panic!("finish_sending"));
        poll_led.set_low();

        if !resp_ok {
            err_led.set_high();
            Timer::after_millis(200).await;
            err_led.set_low();
            continue;
        }

        // ── MSG 3: Receive Final ─────────────────────────────────────────────
        let mut receiving = dw3000.receive(dw_config).await.expect("receive failed");

        // Final arrives at T6 ≈ T3 + TREPLY2 + ToF ≈ T3 + ~10 ms
        let deadline = embassy_time::Instant::now() + embassy_time::Duration::from_millis(200);
        let final_data = loop {
            if embassy_time::Instant::now() > deadline { break None; }
            match receiving.r_wait_buf(&mut buf).await {
                Ok((l, ts, _)) => break Some((l, ts)),
                Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                Err(nb::Error::Other(_)) => { error!("RX error on final"); break None; }
            }
        };
        dw3000 = receiving.finish_receiving().await.unwrap_or_else(|(_, _)| core::panic!("finish_receiving"));

        let Some((final_len, t6)) = final_data else {
            warn!("seq={} no Final received (timeout)", seq);
            err_led.set_high();
            Timer::after_millis(200).await;
            err_led.set_low();
            continue;
        };

        // Find Final payload and extract T1, T4, T5
        let frame = &buf[..final_len];
        let Some(pos) = frame.windows(3).position(|w| w == FINAL_ID) else {
            warn!("seq={} unexpected frame instead of Final (len={})", seq, final_len);
            err_led.set_high();
            Timer::after_millis(200).await;
            err_led.set_low();
            continue;
        };
        let data = &frame[pos..];
        if data.len() < 19 {
            warn!("seq={} Final too short ({})", seq, data.len());
            err_led.set_high();
            Timer::after_millis(200).await;
            err_led.set_low();
            continue;
        }

        let t1 = unpack_ts(&data[4..]);
        let t4 = unpack_ts(&data[9..]);
        let t5 = unpack_ts(&data[14..]);

        // ── Compute distance (APS013 DS-TWR formula) ─────────────────────────
        let t_round1 = DwInstant::new(t4).unwrap().duration_since(DwInstant::new(t1).unwrap()).value();
        let t_round2 = t6.duration_since(t3).value();
        let t_reply1 = t3.duration_since(t2).value();
        let t_reply2 = DwInstant::new(t5).unwrap().duration_since(DwInstant::new(t4).unwrap()).value();

        match compute_distance_mm(t_round1, t_round2, t_reply1, t_reply2) {
            Some(dist_mm) => {
                info!(
                    "seq={} distance={}.{:03} m  [Rr1={} Rr2={} Rp1={} Rp2={}]",
                    seq,
                    dist_mm / 1000,
                    dist_mm % 1000,
                    t_round1, t_round2, t_reply1, t_reply2
                );
                ok_led.set_high();
                Timer::after_millis(200).await;
                ok_led.set_low();
            }
            None => {
                warn!("seq={} distance compute failed (overflow or zero denominator)", seq);
                err_led.set_high();
                Timer::after_millis(200).await;
                err_led.set_low();
            }
        }
    }
}
