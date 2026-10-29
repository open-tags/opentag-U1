#![no_std]
#![no_main]

// Configurable TDOA Anchor firmware. The browser patches the configuration
// block in the release binary before flashing each board.
//
// ANCHOR_ID 0 = master: broadcasts SYNC every 200 ms with its TX timestamp
//   embedded, so slaves can track its clock.  On receiving a BLINK it sends a
//   REPORT with its raw RX timestamp (master IS the time reference).
//
// ANCHOR_ID 1..3 = slave: on each SYNC updates its clock offset relative to
//   master; on each BLINK sends a REPORT with the blink RX timestamp corrected
//   to master-clock space.
//
// Frame layout:
//   SYNC   [DE CA A0] tx_ts[5]                         8 B   A0 → all
//   BLINK  [DE CA B0] seq[1]                           4 B   tag → all
//   REPORT [DE CA C0] anchor_id[1] seq[1] corr_ts[5] 10 B   anchor → tag
//
// Report TX is staggered to avoid collisions (10, 14, 18, or 22 ms).
//
// Pinout: PA3=RST PA4=CS PA5=SCK PA6=MISO PA7=MOSI

use defmt::*;
use dw3000_ng::time::{Duration as DwDuration, Instant as DwInstant};
use dw3000_ng::{Config, DW3000, configs::UwbChannel, hl::SendTime};
use embassy_executor::Spawner;
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::rcc::{Hse, HseMode, Pll, PllMul, PllPreDiv, PllRDiv, PllSource, Sysclk};
use embassy_stm32::spi::{self, Spi};
use embassy_stm32::time::Hertz;
use embassy_stm32::{bind_interrupts, dma, peripherals};
use embassy_time::{Delay, Duration, Instant, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;
use nb;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    DMA1_CHANNEL2 => dma::InterruptHandler<peripherals::DMA1_CH2>;
    DMA1_CHANNEL3 => dma::InterruptHandler<peripherals::DMA1_CH3>;
});

// Browser-patchable configuration (32 bytes, little-endian):
//   00 magic[16] = "OTAG-TDOA-A-V1!!"
//   16 version, 17 anchor_id, 18 anchor_count, 19 reserved
//   20 propagation_delay_from_A0_dtu: i32
//   24 report_delay_ns: u32
//   28 sync_tx_timestamp_bias_dtu: i32
#[used]
#[unsafe(link_section = ".rodata.tdoa_anchor_config")]
static TDOA_ANCHOR_CONFIG: [u8; 32] = [
    b'O', b'T', b'A', b'G', b'-', b'T', b'D', b'O', b'A', b'-', b'A', b'-', b'V', b'1', b'!', b'!',
    1, 0, 4, 0, 0, 0, 0, 0, 0x80, 0x96, 0x98, 0x00, // 10_000_000 ns
    0x00, 0x40, 0x00, 0x00, // 16_384 DTU
];

fn cfg_i32(offset: usize) -> i32 {
    i32::from_le_bytes([
        TDOA_ANCHOR_CONFIG[offset],
        TDOA_ANCHOR_CONFIG[offset + 1],
        TDOA_ANCHOR_CONFIG[offset + 2],
        TDOA_ANCHOR_CONFIG[offset + 3],
    ])
}

fn cfg_u32(offset: usize) -> u32 {
    u32::from_le_bytes([
        TDOA_ANCHOR_CONFIG[offset],
        TDOA_ANCHOR_CONFIG[offset + 1],
        TDOA_ANCHOR_CONFIG[offset + 2],
        TDOA_ANCHOR_CONFIG[offset + 3],
    ])
}

const SYNC_ID: [u8; 3] = [0xDE, 0xCA, 0xA0];
const BLINK_ID: [u8; 3] = [0xDE, 0xCA, 0xB0];
const REPORT_ID: [u8; 3] = [0xDE, 0xCA, 0xC0];

// SYNC period and usable RX window within it.
const SYNC_PERIOD_NS: u32 = 200_000_000; // 200 ms
const RX_WINDOW_MS: u64 = 185;

const MASK40: u64 = (1u64 << 40) - 1;

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

// Signed difference (a - b) in the DW3000 40-bit timestamp domain.
fn diff40(a: u64, b: u64) -> i64 {
    let diff = (a.wrapping_sub(b)) & MASK40;
    if diff & (1u64 << 39) != 0 {
        (diff | !MASK40) as i64
    } else {
        diff as i64
    }
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let anchor_id = TDOA_ANCHOR_CONFIG[17];
    let anchor_count = TDOA_ANCHOR_CONFIG[18];
    let propagation_delay_dtu = cfg_i32(20) as i64;
    let report_delay_ns = cfg_u32(24);
    let sync_tx_bias_dtu = cfg_i32(28) as i64;

    if TDOA_ANCHOR_CONFIG[16] != 1 || !(3..=4).contains(&anchor_count) || anchor_id >= anchor_count
    {
        core::panic!("invalid TDOA anchor config");
    }

    let mut config = embassy_stm32::Config::default();
    {
        config.rcc.hse = Some(Hse {
            freq: Hertz(8_000_000),
            mode: HseMode::Oscillator,
        });
        config.rcc.pll = Some(Pll {
            source: PllSource::HSE,
            prediv: PllPreDiv::DIV2,
            mul: PllMul::MUL85,
            divp: None,
            divq: None,
            divr: Some(PllRDiv::DIV2),
        });
        config.rcc.sys = Sysclk::PLL1_R;
    }
    let p = embassy_stm32::init(config);

    info!(
        "TDOA Anchor {} of {} starting prop={} dtu",
        anchor_id, anchor_count, propagation_delay_dtu
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
    let dw3000 = dw3000.init().await.expect("DW3000 init");
    let dw_config = Config {
        channel: UwbChannel::Channel9,
        ..Config::default()
    };
    let mut dw3000 = dw3000
        .config(dw_config, Delay)
        .await
        .expect("DW3000 config");

    for pin in 0..4u8 {
        dw3000.gpio_config_mode(pin, 1).await.expect("gpio mode");
    }
    dw3000.gpio_config_clocks().await.expect("gpio clocks");

    info!("DW3000 ready");

    if anchor_id == 0 {
        // ── MASTER ──────────────────────────────────────────────────────────────

        // First SYNC: send immediately to get an initial DW3000 timestamp anchor.
        // Slaves that see tx_ts=0 skip the clock-offset update.
        let boot_sync = [SYNC_ID[0], SYNC_ID[1], SYNC_ID[2], 0, 0, 0, 0, 0];
        let mut sending = dw3000
            .send(&boot_sync, SendTime::Now, dw_config)
            .await
            .expect("boot sync tx");
        let mut last_sync_tx: DwInstant = loop {
            match sending.s_wait().await {
                Ok(ts) => break ts,
                Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                Err(nb::Error::Other(_)) => core::panic!("boot sync s_wait"),
            }
        };
        dw3000 = sending
            .finish_sending()
            .await
            .unwrap_or_else(|(_, _)| core::panic!("finish_sending"));
        info!("A0 boot SYNC sent");

        loop {
            // Plan next SYNC time now so we can embed it in the packet.
            let next_sync =
                align512((last_sync_tx + DwDuration::from_nanos(SYNC_PERIOD_NS)).value());
            let rx_deadline = Instant::now() + Duration::from_millis(RX_WINDOW_MS);

            // ── RX window: listen for BLINKs until time to send next SYNC ───────
            'rx: loop {
                if Instant::now() >= rx_deadline {
                    break;
                }

                let mut receiving = dw3000.receive(dw_config).await.expect("rx start");
                let mut buf = [0u8; 64];
                let mut got: Option<(usize, DwInstant)> = None;

                loop {
                    if Instant::now() >= rx_deadline {
                        break;
                    }
                    match receiving.r_wait_buf(&mut buf).await {
                        Ok((l, ts, _)) => {
                            got = Some((l, ts));
                            break;
                        }
                        Err(nb::Error::WouldBlock) => Timer::after_micros(200).await,
                        Err(_) => break,
                    }
                }
                dw3000 = receiving
                    .finish_receiving()
                    .await
                    .unwrap_or_else(|(_, _)| core::panic!("finish_receiving"));

                let Some((len, blink_rx)) = got else {
                    continue 'rx;
                };
                let frame = &buf[..len];

                let Some(pos) = frame.windows(3).position(|w| w == BLINK_ID) else {
                    continue 'rx;
                };
                if frame.len() < pos + 4 {
                    continue 'rx;
                }
                let seq = frame[pos + 3];

                // A0 is the time reference — its raw RX timestamp IS the master timestamp.
                let report_tx =
                    align512((blink_rx + DwDuration::from_nanos(report_delay_ns)).value());
                let mut report = [0u8; 10];
                report[0..3].copy_from_slice(&REPORT_ID);
                report[3] = 0; // anchor_id
                report[4] = seq;
                pack_ts(blink_rx.value() & MASK40, &mut report[5..]);

                let mut sending = dw3000
                    .send(&report, SendTime::Delayed(report_tx), dw_config)
                    .await
                    .expect("report tx");
                loop {
                    match sending.s_wait().await {
                        Ok(_) => break,
                        Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                        Err(nb::Error::Other(_)) => {
                            warn!("A0 REPORT TX too late seq={}", seq);
                            break;
                        }
                    }
                }
                dw3000 = sending
                    .finish_sending()
                    .await
                    .unwrap_or_else(|(_, _)| core::panic!("finish_sending"));
                info!("A0 REPORT seq={}", seq);
            }

            // ── TX SYNC with next_sync timestamp embedded ────────────────────────
            let mut sync_frame = [0u8; 8];
            sync_frame[0..3].copy_from_slice(&SYNC_ID);
            let sync_tx_ts = ((next_sync.value() as i64 + sync_tx_bias_dtu) as u64) & MASK40;
            pack_ts(sync_tx_ts, &mut sync_frame[3..]);

            let mut sending = dw3000
                .send(&sync_frame, SendTime::Delayed(next_sync), dw_config)
                .await
                .expect("sync tx");
            last_sync_tx = loop {
                match sending.s_wait().await {
                    Ok(ts) => break ts,
                    Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                    Err(nb::Error::Other(_)) => {
                        warn!("A0 SYNC TX too late — increase RX_WINDOW_MS or SYNC_PERIOD_NS");
                        break next_sync;
                    }
                }
            };
            dw3000 = sending
                .finish_sending()
                .await
                .unwrap_or_else(|(_, _)| core::panic!("finish_sending"));
            trace!("A0 SYNC sent");
        }
    } else {
        // ── SLAVE ───────────────────────────────────────────────────────────────
        // clock_offset = local_clock - master_clock (DTU).
        // offset_rate_q32 is d(clock_offset)/d(local_time), in Q32 fixed point.
        // Corrected blink RX = local_rx - predicted clock_offset at blink time.
        let mut clock_offset: i64 = 0;
        let mut offset_rate_q32: i64 = 0;
        let mut last_sync_rx: u64 = 0;
        let mut sync_valid = false;

        loop {
            let mut receiving = dw3000.receive(dw_config).await.expect("rx start");
            let mut buf = [0u8; 64];

            let rx_result = loop {
                match receiving.r_wait_buf(&mut buf).await {
                    Ok((l, ts, _)) => break Some((l, ts)),
                    Err(nb::Error::WouldBlock) => Timer::after_micros(200).await,
                    Err(_) => break None,
                }
            };
            dw3000 = receiving
                .finish_receiving()
                .await
                .unwrap_or_else(|(_, _)| core::panic!("finish_receiving"));

            let Some((len, rx_ts)) = rx_result else {
                continue;
            };
            let frame = &buf[..len];

            // ── SYNC: update clock offset ────────────────────────────────────────
            if let Some(pos) = frame.windows(3).position(|w| w == SYNC_ID) {
                if frame.len() < pos + 8 {
                    continue;
                }
                let sync_tx_master = unpack_ts(&frame[pos + 3..]);
                if sync_tx_master == 0 {
                    continue;
                } // boot sync, skip
                // clock_offset = local_rx - (master_tx + propagation_delay)
                let master_arrival =
                    ((sync_tx_master as i64 + propagation_delay_dtu) as u64) & MASK40;
                let measured_offset = diff40(rx_ts.value(), master_arrival);

                if sync_valid {
                    let dt_local = diff40(rx_ts.value(), last_sync_rx);
                    if dt_local > 0 {
                        let offset_delta = measured_offset - clock_offset;
                        let measured_rate_q32 = ((offset_delta as i128) << 32) / dt_local as i128;
                        // Gentle IIR smoothing: enough to track ppm skew without letting one bad
                        // SYNC dominate the next blink correction.
                        offset_rate_q32 =
                            ((offset_rate_q32 as i128 * 3 + measured_rate_q32) / 4) as i64;
                    }
                }

                clock_offset = measured_offset;
                last_sync_rx = rx_ts.value();
                sync_valid = true;
                trace!(
                    "A{} clock_offset={} rate_q32={}",
                    anchor_id, clock_offset, offset_rate_q32
                );
                continue;
            }

            // ── BLINK: send corrected REPORT ────────────────────────────────────
            if let Some(pos) = frame.windows(3).position(|w| w == BLINK_ID) {
                if frame.len() < pos + 4 {
                    continue;
                }
                let seq = frame[pos + 3];

                if !sync_valid {
                    trace!("A{} BLINK seq={} ignored (no sync yet)", anchor_id, seq);
                    continue;
                }

                // Translate blink RX timestamp into master-clock space by extrapolating
                // the last sync offset to the blink RX epoch.
                let dt_since_sync = diff40(rx_ts.value(), last_sync_rx);
                let predicted_offset = clock_offset
                    + (((offset_rate_q32 as i128 * dt_since_sync as i128) >> 32) as i64);
                let corrected = ((rx_ts.value() as i64 - predicted_offset) as u64) & MASK40;

                let report_tx = align512((rx_ts + DwDuration::from_nanos(report_delay_ns)).value());
                let mut report = [0u8; 10];
                report[0..3].copy_from_slice(&REPORT_ID);
                report[3] = anchor_id;
                report[4] = seq;
                pack_ts(corrected, &mut report[5..]);

                let mut sending = dw3000
                    .send(&report, SendTime::Delayed(report_tx), dw_config)
                    .await
                    .expect("report tx");
                loop {
                    match sending.s_wait().await {
                        Ok(_) => break,
                        Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                        Err(nb::Error::Other(_)) => {
                            warn!("A{} REPORT TX too late seq={}", anchor_id, seq);
                            break;
                        }
                    }
                }
                dw3000 = sending
                    .finish_sending()
                    .await
                    .unwrap_or_else(|(_, _)| core::panic!("finish_sending"));
                info!("A{} REPORT seq={}", anchor_id, seq);
            }
        }
    }
}
