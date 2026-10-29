#![no_std]
#![no_main]

// TWR Bench Responder — benchmark variant of twr_resp.rs. Same PHY, frame
// protocol, reply timing and distance math, but no USB, CAN or host commands:
// every record goes out over defmt/RTT for logging with probe-rs.
//
// Build with info-level logging so radio-driver traces don't flood RTT:
//   DEFMT_LOG=info TWR_CALIB=<dtu> TWR_PHY=<a|b> cargo run --release --bin twr_bench_resp
//
// TWR_PHY selects the radio configuration; both boards must use the same one:
//   a (default) 850 kbps, 128-symbol preamble, IEEE 802.15.4a SFD, driver PAC (16) — as twr_resp
//   b           6.8 Mbps, 128-symbol preamble, IEEE 802.15.4z SFD, PAC 8 — Qorvo's default
// Attach without reflashing for each benchmark run:
//   probe-rs attach --chip STM32G474CC <elf>
//
// RTT records (each prefixed with defmt's uptime timestamp):
//   BENCH fw=<name> git=<hash> uid=<hex> calib=<dtu> phy=<a|b> ch=9 br=<kbps> pre=128 sfd=<4a|4z> pac=<16|8> sysclk_hz=144000000
//   D <seq> <dist_mm> <exchange_us> <rssi_tenths> <los_pct>
//   MISS <seq> <reason>
//   STATS polls=<n> ok=<n> no_final=<n> bad_final=<n> seq_final=<n> compute_fail=<n> rx_err=<n> resp_tx_err=<n>
//
// <seq> is the initiator's u8 sequence number and wraps at 256.

use core::sync::atomic::{AtomicU32, Ordering};

use defmt::*;
use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::rcc::{Hse, HseMode, Pll, PllMul, PllPreDiv, PllQDiv, PllRDiv, PllSource, Sysclk};
use embassy_stm32::spi::{self, Spi};
use embassy_stm32::time::Hertz;
use embassy_stm32::{bind_interrupts, dma, peripherals, uid};
use embassy_time::{Delay, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;

use dw3000_ng::time::{Duration as DwDuration, Instant as DwInstant};
use dw3000_ng::{
    Config as DwConfig, DW3000,
    configs::{BitRate, PreambleLength, SfdSequence, UwbChannel},
    hl::SendTime,
};
use nb;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    DMA1_CHANNEL2 => dma::InterruptHandler<peripherals::DMA1_CH2>;
    DMA1_CHANNEL3 => dma::InterruptHandler<peripherals::DMA1_CH3>;
});

const POLL_ID: &[u8] = &[0xDE, 0xCA, 0x01];
const RESP_ID: &[u8] = &[0xDE, 0xCA, 0x02];
const FINAL_ID: &[u8] = &[0xDE, 0xCA, 0x03];
const TREPLY1_NS: u32 = 8_000_000;
const FINAL_TIMEOUT_MS: u64 = 20;
const STATS_PERIOD_S: u64 = 10;
const FW_VERSION: &str = "twr_bench_resp-1";
const GIT_HASH: &str = match option_env!("GIT_HASH") {
    Some(h) => h,
    None => "unknown",
};

// Antenna-delay calibration in DTU, fixed at build time. Matches the
// twr_resp default unless TWR_CALIB is set; changing it requires a rebuild.
const DEFAULT_CALIB_DTU: i32 = 12241;

static POLLS: AtomicU32 = AtomicU32::new(0);
static OK: AtomicU32 = AtomicU32::new(0);
static NO_FINAL: AtomicU32 = AtomicU32::new(0);
static BAD_FINAL: AtomicU32 = AtomicU32::new(0);
static SEQ_FINAL: AtomicU32 = AtomicU32::new(0);
static COMPUTE_FAIL: AtomicU32 = AtomicU32::new(0);
static RX_ERR: AtomicU32 = AtomicU32::new(0);
static RESP_TX_ERR: AtomicU32 = AtomicU32::new(0);

// Receive errors by DW3000 status cause, split by when they happened: while
// idle waiting for a POLL, or inside the FINAL window where one ends the exchange.
const RX_KINDS: usize = 10;
const RX_KIND_NAMES: [&str; RX_KINDS] = [
    "fcs", "phy", "rs", "sfd_to", "frame_wait_to", "overrun", "preamble_to", "filter", "too_small", "spi",
];
static RX_ERR_IDLE: [AtomicU32; RX_KINDS] = [const { AtomicU32::new(0) }; RX_KINDS];
static RX_ERR_FINAL: [AtomicU32; RX_KINDS] = [const { AtomicU32::new(0) }; RX_KINDS];

fn rx_kind<SPI: embedded_hal_1::spi::ErrorType>(e: &dw3000_ng::Error<SPI>) -> usize {
    use dw3000_ng::Error as E;
    match e {
        E::Fcs => 0,
        E::Phy => 1,
        E::ReedSolomon => 2,
        E::SfdTimeout => 3,
        E::FrameWaitTimeout => 4,
        E::Overrun => 5,
        E::PreambleDetectionTimeout => 6,
        E::FrameFilteringRejection => 7,
        E::BufferTooSmall { .. } => 8,
        _ => 9,
    }
}

fn log_rx_errors(label: &str, counts: &[AtomicU32; RX_KINDS]) {
    let mut v = [0u32; RX_KINDS];
    for (slot, c) in v.iter_mut().zip(counts) {
        *slot = c.load(Ordering::Relaxed);
    }
    info!(
        "RXERR {=str} {=str}={=u32} {=str}={=u32} {=str}={=u32} {=str}={=u32} {=str}={=u32} {=str}={=u32} {=str}={=u32} {=str}={=u32} {=str}={=u32} {=str}={=u32}",
        label,
        RX_KIND_NAMES[0], v[0], RX_KIND_NAMES[1], v[1], RX_KIND_NAMES[2], v[2],
        RX_KIND_NAMES[3], v[3], RX_KIND_NAMES[4], v[4], RX_KIND_NAMES[5], v[5],
        RX_KIND_NAMES[6], v[6], RX_KIND_NAMES[7], v[7], RX_KIND_NAMES[8], v[8],
        RX_KIND_NAMES[9], v[9],
    );
}

/// TWR_LISTEN=1 logs every receive error (`RXE <kind>`) and every decoded
/// frame that isn't a POLL (`FRAME len=<n> rssi=<dBm×10> <first bytes>`), to
/// fingerprint background traffic. Off by default; it adds RTT load.
fn listen_mode() -> bool {
    matches!(option_env!("TWR_LISTEN"), Some("1"))
}

/// True for TWR_PHY=b (Qorvo default PHY), false for the twr_resp PHY.
fn phy_is_b() -> bool {
    match option_env!("TWR_PHY") {
        None | Some("a") => false,
        Some("b") => true,
        Some(_) => defmt::panic!("TWR_PHY must be a or b"),
    }
}

fn build_calib() -> i32 {
    match option_env!("TWR_CALIB") {
        Some(s) => match s.trim().parse::<i32>() {
            Ok(v) => v,
            Err(_) => defmt::panic!("TWR_CALIB is not an integer"),
        },
        None => DEFAULT_CALIB_DTU,
    }
}

fn pack_ts(ts: u64, buf: &mut [u8]) {
    buf[..5].copy_from_slice(&ts.to_le_bytes()[..5]);
}

fn unpack_ts(buf: &[u8]) -> u64 {
    u64::from_le_bytes([buf[0], buf[1], buf[2], buf[3], buf[4], 0, 0, 0])
}

fn align512(raw: u64) -> DwInstant {
    DwInstant::new((raw + 511) & !511u64).unwrap()
}

// Identical to twr_resp::compute_distance_mm, with the calibration passed in.
fn compute_distance_mm(
    t_round1: u64,
    t_round2: u64,
    t_reply1: u64,
    t_reply2: u64,
    calib_dtu: i32,
) -> Option<u64> {
    let num = (t_round1 as u128)
        .checked_mul(t_round2 as u128)?
        .checked_sub((t_reply1 as u128).checked_mul(t_reply2 as u128)?)?;
    let den = t_round1 as u128 + t_round2 as u128 + t_reply1 as u128 + t_reply2 as u128;
    if den == 0 {
        return None;
    }
    let corrected_num = (num as i128)
        .checked_sub((calib_dtu as i128).checked_mul(den as i128)?)?
        .max(0) as u128;
    let scaled_num = corrected_num.checked_mul(299_792_458)?;
    let scaled_den = den.checked_mul(63_897_600)?;
    Some((scaled_num / scaled_den) as u64)
}

fn log_stats() {
    info!(
        "STATS polls={=u32} ok={=u32} no_final={=u32} bad_final={=u32} seq_final={=u32} compute_fail={=u32} rx_err={=u32} resp_tx_err={=u32}",
        POLLS.load(Ordering::Relaxed),
        OK.load(Ordering::Relaxed),
        NO_FINAL.load(Ordering::Relaxed),
        BAD_FINAL.load(Ordering::Relaxed),
        SEQ_FINAL.load(Ordering::Relaxed),
        COMPUTE_FAIL.load(Ordering::Relaxed),
        RX_ERR.load(Ordering::Relaxed),
        RESP_TX_ERR.load(Ordering::Relaxed),
    );
    log_rx_errors("idle", &RX_ERR_IDLE);
    log_rx_errors("final", &RX_ERR_FINAL);
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    // Same 144 MHz system clock as twr_resp; the USB-only HSI48 is left off.
    let mut config = embassy_stm32::Config::default();
    {
        config.rcc.hse = Some(Hse {
            freq: Hertz(8_000_000),
            mode: HseMode::Oscillator,
        });
        config.rcc.pll = Some(Pll {
            source: PllSource::HSE,
            prediv: PllPreDiv::DIV2,
            mul: PllMul::MUL72,
            divp: None,
            divq: Some(PllQDiv::DIV6),
            divr: Some(PllRDiv::DIV2),
        });
        config.rcc.sys = Sysclk::PLL1_R;
        config.rcc.boost = true;
    }
    let p = embassy_stm32::init(config);

    let calib_dtu = build_calib();
    let phy_b = phy_is_b();
    info!(
        "BENCH fw={=str} git={=str} uid={=str} calib={=i32} phy={=str} ch=9 br={=str} pre=128 sfd={=str} pac={=str} sysclk_hz=144000000",
        FW_VERSION,
        GIT_HASH,
        uid::uid_hex(),
        calib_dtu,
        if phy_b { "b" } else { "a" },
        if phy_b { "6800" } else { "850" },
        if phy_b { "4z" } else { "4a" },
        if phy_b { "8" } else { "16" },
    );

    let mut rst = Output::new(p.PA3, Level::Low, Speed::Low);
    Timer::after_millis(10).await;
    rst.set_high();
    Timer::after_millis(5).await;

    let mut spi_config = spi::Config::default();
    spi_config.frequency = Hertz(4_000_000);
    let spi = Spi::new(
        p.SPI1, p.PA5, p.PA7, p.PA6, p.DMA1_CH3, p.DMA1_CH2, Irqs, spi_config,
    );
    let cs = Output::new(p.PA4, Level::High, Speed::High);
    let spi_dev = ExclusiveDevice::new(spi, cs, Delay).unwrap();

    let dw3000 = DW3000::new(spi_dev);
    let dw3000 = dw3000.init().await.expect("DW3000 init failed");
    // dw3000-ng's SfdSequence::Ieee is SFD type 3, the 802.15.4z 8-symbol SFD.
    let dw_config = DwConfig {
        channel: UwbChannel::Channel9,
        sfd_sequence: if phy_b { SfdSequence::Ieee } else { SfdSequence::IeeeShort },
        preamble_length: PreambleLength::Symbols128,
        bitrate: if phy_b { BitRate::Kbps6800 } else { BitRate::Kbps850 },
        sfd_timeout: 129,
        ..DwConfig::default()
    };
    let mut dw3000 = dw3000
        .config(dw_config, Delay)
        .await
        .expect("DW3000 config failed");
    if phy_b {
        // The driver programs PAC 16 for 128-symbol preambles; Qorvo's table says PAC 8 (code 0).
        dw3000
            .ll()
            .dtune0()
            .modify(|_, w| w.pac(0))
            .await
            .expect("PAC 8");
    }
    dw3000
        .set_full_cia_diagnostics(true)
        .await
        .expect("CIA diagnostics");
    for pin in 0..4u8 {
        dw3000.gpio_config_mode(pin, 1).await.expect("gpio mode");
    }
    dw3000.gpio_config_clocks().await.expect("gpio clocks");

    let stats_fut = async {
        loop {
            Timer::after_secs(STATS_PERIOD_S).await;
            log_stats();
        }
    };

    // ── Responder loop (protocol and timing identical to twr_resp) ──────────
    let listen = listen_mode();
    let twr_fut = async {
        loop {
            let mut receiving = dw3000.receive(dw_config).await.expect("rx");
            let mut buf = [0u8; 128];
            let poll_result = loop {
                match receiving.r_wait_buf(&mut buf).await {
                    Ok((l, ts, q)) => {
                        if listen && !buf[..l].windows(3).any(|w| w == POLL_ID) {
                            let n = l.min(12);
                            info!(
                                "FRAME len={=usize} rssi={=i16} {=[u8]:02x}",
                                l,
                                (q.rssi * 10.0) as i16,
                                buf[..n]
                            );
                        }
                        break Some((l, ts));
                    }
                    Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                    Err(nb::Error::Other(e)) => {
                        RX_ERR.fetch_add(1, Ordering::Relaxed);
                        let kind = rx_kind(&e);
                        RX_ERR_IDLE[kind].fetch_add(1, Ordering::Relaxed);
                        if listen {
                            info!("RXE {=str}", RX_KIND_NAMES[kind]);
                        }
                        break None;
                    }
                }
            };
            dw3000 = receiving
                .finish_receiving()
                .await
                .unwrap_or_else(|(_, _)| core::panic!("finish"));
            let Some((poll_len, t2)) = poll_result else {
                continue;
            };
            POLLS.fetch_add(1, Ordering::Relaxed);
            let exchange_start = embassy_time::Instant::now();

            let frame = &buf[..poll_len];
            let Some(pos) = frame.windows(3).position(|w| w == POLL_ID) else {
                continue;
            };
            let seq = frame.get(pos + 3).copied().unwrap_or(0);

            let t3 = align512((t2 + DwDuration::from_nanos(TREPLY1_NS)).value());
            let mut resp = [0u8; 14];
            resp[0] = RESP_ID[0];
            resp[1] = RESP_ID[1];
            resp[2] = RESP_ID[2];
            resp[3] = seq;
            pack_ts(t2.value(), &mut resp[4..]);
            pack_ts(t3.value(), &mut resp[9..]);

            let mut sending = dw3000
                .send(&resp, SendTime::Delayed(t3), dw_config)
                .await
                .expect("resp send");
            let resp_ok = loop {
                match sending.s_wait().await {
                    Ok(_) => break true,
                    Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                    Err(nb::Error::Other(_)) => break false,
                }
            };
            dw3000 = sending
                .finish_sending()
                .await
                .unwrap_or_else(|(_, _)| core::panic!("finish"));
            if !resp_ok {
                RESP_TX_ERR.fetch_add(1, Ordering::Relaxed);
                info!("MISS {=u8} resp-tx", seq);
                continue;
            }

            let mut receiving = dw3000.receive(dw_config).await.expect("rx");
            let deadline = embassy_time::Instant::now()
                + embassy_time::Duration::from_millis(FINAL_TIMEOUT_MS);
            let final_data = loop {
                if embassy_time::Instant::now() > deadline {
                    break None;
                }
                match receiving.r_wait_buf(&mut buf).await {
                    Ok((l, ts, quality)) => break Some((l, ts, quality)),
                    Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                    Err(nb::Error::Other(e)) => {
                        RX_ERR.fetch_add(1, Ordering::Relaxed);
                        RX_ERR_FINAL[rx_kind(&e)].fetch_add(1, Ordering::Relaxed);
                        break None;
                    }
                }
            };
            dw3000 = receiving
                .finish_receiving()
                .await
                .unwrap_or_else(|(_, _)| core::panic!("finish"));

            let Some((final_len, t6, final_quality)) = final_data else {
                NO_FINAL.fetch_add(1, Ordering::Relaxed);
                info!("MISS {=u8} no-final", seq);
                continue;
            };

            let frame = &buf[..final_len];
            let Some(pos) = frame.windows(3).position(|w| w == FINAL_ID) else {
                BAD_FINAL.fetch_add(1, Ordering::Relaxed);
                info!("MISS {=u8} bad-final-id", seq);
                continue;
            };
            let data = &frame[pos..];
            if data.len() < 19 {
                BAD_FINAL.fetch_add(1, Ordering::Relaxed);
                info!("MISS {=u8} short-final", seq);
                continue;
            }
            if data[3] != seq {
                SEQ_FINAL.fetch_add(1, Ordering::Relaxed);
                info!("MISS {=u8} final-seq-{=u8}", seq, data[3]);
                continue;
            }

            let t1 = unpack_ts(&data[4..]);
            let t4 = unpack_ts(&data[9..]);
            let t5 = unpack_ts(&data[14..]);

            let t_round1 = DwInstant::new(t4)
                .unwrap()
                .duration_since(DwInstant::new(t1).unwrap())
                .value();
            let t_round2 = t6.duration_since(t3).value();
            let t_reply1 = t3.duration_since(t2).value();
            let t_reply2 = DwInstant::new(t5)
                .unwrap()
                .duration_since(DwInstant::new(t4).unwrap())
                .value();

            match compute_distance_mm(t_round1, t_round2, t_reply1, t_reply2, calib_dtu) {
                Some(dist_mm) => {
                    OK.fetch_add(1, Ordering::Relaxed);
                    let exchange_us = embassy_time::Instant::now()
                        .duration_since(exchange_start)
                        .as_micros();
                    let rssi_tenths = (final_quality.rssi * 10.0) as i16;
                    // dw3000-ng's line-of-sight confidence (0–1, per Qorvo APS006), as a percentage.
                    let los_pct = (final_quality.los_confidence_level * 100.0) as u8;
                    info!(
                        "D {=u8} {=u64} {=u64} {=i16} {=u8}",
                        seq, dist_mm, exchange_us, rssi_tenths, los_pct
                    );
                }
                None => {
                    COMPUTE_FAIL.fetch_add(1, Ordering::Relaxed);
                    info!("MISS {=u8} compute-fail", seq);
                }
            }
        }
    };

    join(stats_fut, twr_fut).await;
}
