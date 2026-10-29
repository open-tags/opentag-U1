#![no_std]
#![no_main]

// TWR Bench Initiator — benchmark variant of twr_init.rs, paired with
// twr_bench_resp. Same frame protocol and timing (TREPLY2 8 ms, response
// timeout 20 ms, 8 ms between ranges, 2 ms error backoff), but no USB: misses
// and counters go out over defmt/RTT. A poll TX error is counted and skipped
// instead of halting the board.
//
// Build with the same TWR_PHY as the responder (see twr_bench_resp.rs):
//   DEFMT_LOG=info TWR_PHY=<a|b> cargo run --release --bin twr_bench_init
//
// RTT records (each prefixed with defmt's uptime timestamp):
//   BENCH fw=<name> git=<hash> uid=<hex> phy=<a|b> ...
//   MISS <seq> <reason>
//   STATS attempts=<n> ok=<n> no_resp=<n> bad_resp=<n> seq_resp=<n> poll_tx_err=<n> final_tx_err=<n>
//   RXERR resp fcs=<n> phy=<n> ...      (receive errors while waiting for the RESP)

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
const TREPLY2_NS: u32 = 8_000_000;
const RESPONSE_TIMEOUT_MS: u64 = 20;
const INTER_RANGE_MS: u64 = 8;
const ERROR_BACKOFF_MS: u64 = 2;
const STATS_PERIOD_S: u64 = 10;
const FW_VERSION: &str = "twr_bench_init-1";
const GIT_HASH: &str = match option_env!("GIT_HASH") {
    Some(h) => h,
    None => "unknown",
};

static ATTEMPTS: AtomicU32 = AtomicU32::new(0);
static OK: AtomicU32 = AtomicU32::new(0);
static NO_RESP: AtomicU32 = AtomicU32::new(0);
static BAD_RESP: AtomicU32 = AtomicU32::new(0);
static SEQ_RESP: AtomicU32 = AtomicU32::new(0);
static POLL_TX_ERR: AtomicU32 = AtomicU32::new(0);
static FINAL_TX_ERR: AtomicU32 = AtomicU32::new(0);

const RX_KINDS: usize = 10;
const RX_KIND_NAMES: [&str; RX_KINDS] = [
    "fcs", "phy", "rs", "sfd_to", "frame_wait_to", "overrun", "preamble_to", "filter", "too_small", "spi",
];
static RX_ERR_RESP: [AtomicU32; RX_KINDS] = [const { AtomicU32::new(0) }; RX_KINDS];

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

/// True for TWR_PHY=b (Qorvo default PHY), false for the twr_init PHY.
fn phy_is_b() -> bool {
    match option_env!("TWR_PHY") {
        None | Some("a") => false,
        Some("b") => true,
        Some(_) => defmt::panic!("TWR_PHY must be a or b"),
    }
}

fn pack_ts(ts: u64, buf: &mut [u8]) {
    buf[..5].copy_from_slice(&ts.to_le_bytes()[..5]);
}

fn align512(raw: u64) -> DwInstant {
    DwInstant::new((raw + 511) & !511u64).unwrap()
}

fn log_stats() {
    info!(
        "STATS attempts={=u32} ok={=u32} no_resp={=u32} bad_resp={=u32} seq_resp={=u32} poll_tx_err={=u32} final_tx_err={=u32}",
        ATTEMPTS.load(Ordering::Relaxed),
        OK.load(Ordering::Relaxed),
        NO_RESP.load(Ordering::Relaxed),
        BAD_RESP.load(Ordering::Relaxed),
        SEQ_RESP.load(Ordering::Relaxed),
        POLL_TX_ERR.load(Ordering::Relaxed),
        FINAL_TX_ERR.load(Ordering::Relaxed),
    );
    let mut v = [0u32; RX_KINDS];
    for (slot, c) in v.iter_mut().zip(&RX_ERR_RESP) {
        *slot = c.load(Ordering::Relaxed);
    }
    info!(
        "RXERR resp {=str}={=u32} {=str}={=u32} {=str}={=u32} {=str}={=u32} {=str}={=u32} {=str}={=u32} {=str}={=u32} {=str}={=u32} {=str}={=u32} {=str}={=u32}",
        RX_KIND_NAMES[0], v[0], RX_KIND_NAMES[1], v[1], RX_KIND_NAMES[2], v[2],
        RX_KIND_NAMES[3], v[3], RX_KIND_NAMES[4], v[4], RX_KIND_NAMES[5], v[5],
        RX_KIND_NAMES[6], v[6], RX_KIND_NAMES[7], v[7], RX_KIND_NAMES[8], v[8],
        RX_KIND_NAMES[9], v[9],
    );
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    // Same 144 MHz system clock as twr_init; the USB-only HSI48 is left off.
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

    let phy_b = phy_is_b();
    info!(
        "BENCH fw={=str} git={=str} uid={=str} phy={=str} ch=9 br={=str} pre=128 sfd={=str} pac={=str} sysclk_hz=144000000",
        FW_VERSION,
        GIT_HASH,
        uid::uid_hex(),
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

    // ── Initiator loop (protocol and timing identical to twr_init) ──────────
    let twr_fut = async {
        let mut seq: u8 = 0;
        loop {
            ATTEMPTS.fetch_add(1, Ordering::Relaxed);
            let poll = [POLL_ID[0], POLL_ID[1], POLL_ID[2], seq];
            let mut sending = dw3000
                .send(&poll, SendTime::Now, dw_config)
                .await
                .expect("poll send");
            let t1 = loop {
                match sending.s_wait().await {
                    Ok(ts) => break Some(ts),
                    Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                    Err(nb::Error::Other(_)) => break None,
                }
            };
            dw3000 = sending
                .finish_sending()
                .await
                .unwrap_or_else(|(_, _)| core::panic!("finish"));
            let Some(t1) = t1 else {
                POLL_TX_ERR.fetch_add(1, Ordering::Relaxed);
                info!("MISS {=u8} poll-tx", seq);
                seq = seq.wrapping_add(1);
                Timer::after_millis(ERROR_BACKOFF_MS).await;
                continue;
            };

            let mut receiving = dw3000.receive(dw_config).await.expect("rx");
            let mut rx_buf = [0u8; 128];
            let deadline = embassy_time::Instant::now()
                + embassy_time::Duration::from_millis(RESPONSE_TIMEOUT_MS);
            let rx_data = loop {
                if embassy_time::Instant::now() > deadline {
                    break None;
                }
                match receiving.r_wait_buf(&mut rx_buf).await {
                    Ok((len, ts, _)) => break Some((len, ts)),
                    Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                    Err(nb::Error::Other(e)) => {
                        RX_ERR_RESP[rx_kind(&e)].fetch_add(1, Ordering::Relaxed);
                        break None;
                    }
                }
            };
            dw3000 = receiving
                .finish_receiving()
                .await
                .unwrap_or_else(|(_, _)| core::panic!("finish"));

            let Some((len, t4)) = rx_data else {
                NO_RESP.fetch_add(1, Ordering::Relaxed);
                info!("MISS {=u8} no-resp", seq);
                seq = seq.wrapping_add(1);
                Timer::after_millis(ERROR_BACKOFF_MS).await;
                continue;
            };

            let frame = &rx_buf[..len];
            let Some(pos) = frame.windows(3).position(|w| w == RESP_ID) else {
                BAD_RESP.fetch_add(1, Ordering::Relaxed);
                info!("MISS {=u8} bad-resp-id", seq);
                seq = seq.wrapping_add(1);
                Timer::after_millis(ERROR_BACKOFF_MS).await;
                continue;
            };
            if frame[pos..].len() < 14 {
                BAD_RESP.fetch_add(1, Ordering::Relaxed);
                info!("MISS {=u8} short-resp", seq);
                seq = seq.wrapping_add(1);
                Timer::after_millis(ERROR_BACKOFF_MS).await;
                continue;
            }
            if frame[pos + 3] != seq {
                SEQ_RESP.fetch_add(1, Ordering::Relaxed);
                info!("MISS {=u8} resp-seq-{=u8}", seq, frame[pos + 3]);
                seq = seq.wrapping_add(1);
                Timer::after_millis(ERROR_BACKOFF_MS).await;
                continue;
            }

            let t5 = align512((t4 + DwDuration::from_nanos(TREPLY2_NS)).value());
            let mut final_msg = [0u8; 19];
            final_msg[0] = FINAL_ID[0];
            final_msg[1] = FINAL_ID[1];
            final_msg[2] = FINAL_ID[2];
            final_msg[3] = seq;
            pack_ts(t1.value(), &mut final_msg[4..]);
            pack_ts(t4.value(), &mut final_msg[9..]);
            pack_ts(t5.value(), &mut final_msg[14..]);

            let mut sending = dw3000
                .send(&final_msg, SendTime::Delayed(t5), dw_config)
                .await
                .expect("final send");
            let final_ok = loop {
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
            if final_ok {
                OK.fetch_add(1, Ordering::Relaxed);
            } else {
                FINAL_TX_ERR.fetch_add(1, Ordering::Relaxed);
                info!("MISS {=u8} final-tx", seq);
            }

            seq = seq.wrapping_add(1);
            Timer::after_millis(INTER_RANGE_MS).await;
        }
    };

    join(stats_fut, twr_fut).await;
}
