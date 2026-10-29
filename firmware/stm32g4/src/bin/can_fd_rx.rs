#![no_std]
#![no_main]

// CAN FD RX throughput / latency tester
//
// Receives 64-byte FD frames from can_fd_tx and measures:
//   - One-way latency (TX timestamp embedded in payload vs local tick)
//   - Received throughput (kbps)
//   - Dropped frames (gap in sequence numbers)
//
// Log format parsed by can_plot:
//   CANFD RX seq=N latency_us=N bytes=N
//   CANFD STAT elapsed_ms=N frames=N bytes=N kbps=N drops=N

use defmt::*;
use embassy_executor::Spawner;
use embassy_stm32::can::frame::FdEnvelope;
use embassy_stm32::can::{self, Can};
use embassy_stm32::can::filter::{StandardFilter, StandardFilterSlot};
use embassy_stm32::can::{IT0InterruptHandler, IT1InterruptHandler};
use embassy_stm32::rcc::{Hse, HseMode, Pll, PllMul, PllPreDiv, PllRDiv, PllSource, Sysclk};
use embassy_stm32::rcc::mux::Fdcansel;
use embassy_stm32::time::Hertz;
use embassy_stm32::{bind_interrupts, peripherals};
use embassy_time::Instant;
use {defmt_rtt as _, panic_probe as _};

const NOM_BPS:  u32 = 1_000_000;
const DATA_BPS: u32 = 2_000_000;

// embassy_time tick rate (tick-hz-32_768 feature)
const TICKS_PER_SEC: u64 = 32_768;

bind_interrupts!(struct Irqs {
    FDCAN2_IT0 => IT0InterruptHandler<peripherals::FDCAN2>;
    FDCAN2_IT1 => IT1InterruptHandler<peripherals::FDCAN2>;
});

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let mut config = embassy_stm32::Config::default();
    {
        config.rcc.hse = Some(Hse { freq: Hertz(8_000_000), mode: HseMode::Oscillator });
        config.rcc.pll = Some(Pll {
            source: PllSource::HSE,
            prediv: PllPreDiv::DIV2,
            mul: PllMul::MUL85,
            divp: None,
            divq: None,
            divr: Some(PllRDiv::DIV2),
        });
        config.rcc.sys = Sysclk::PLL1_R;
        // Route FDCAN to PCLK1 (170 MHz) — default HSE (8 MHz) can't do >1 Mbps data
        config.rcc.mux.fdcansel = Fdcansel::PCLK1;
    }
    let p = embassy_stm32::init(config);

    info!("CAN FD RX starting  nom={}bps data={}bps", NOM_BPS, DATA_BPS);

    let mut configurator = can::CanConfigurator::new(p.FDCAN2, p.PB12, p.PB13, Irqs);
    configurator.set_bitrate(NOM_BPS);
    configurator.set_fd_data_bitrate(DATA_BPS, true);

    configurator.properties().set_standard_filter(
        StandardFilterSlot::_0,
        StandardFilter::accept_all_into_fifo0(),
    );

    let mut can: Can = configurator.into_normal_mode();
    info!("CAN FD ready — waiting for frames...");

    let mut total_frames: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut total_drops: u64 = 0;
    let mut expected_seq: u32 = 0;
    let stat_start = Instant::now();

    loop {
        match can.read_fd().await {
            Ok(FdEnvelope { frame, .. }) => {
                let rx_ticks = Instant::now().as_ticks();
                let data = frame.data();
                let bytes = data.len() as u64;

                // Extract TX-embedded seq + timestamp
                let seq = u32::from_le_bytes(data[0..4].try_into().unwrap());
                let tx_ticks = u32::from_le_bytes(data[4..8].try_into().unwrap()) as u64;

                // Latency — wrapping subtraction handles tick counter rollover
                let delta_ticks = rx_ticks.wrapping_sub(tx_ticks);
                let latency_us = delta_ticks * 1_000_000 / TICKS_PER_SEC;

                // Sequence gap detection
                let gap = seq.wrapping_sub(expected_seq);
                if gap != 0 {
                    total_drops += gap as u64;
                }
                expected_seq = seq.wrapping_add(1);

                total_frames += 1;
                total_bytes += bytes;

                info!("CANFD RX seq={} latency_us={} bytes={}", seq, latency_us, bytes);

                // Print throughput stat every 512 frames
                if total_frames & 0x1FF == 0 {
                    let elapsed_ms = stat_start.elapsed().as_millis();
                    let kbps = if elapsed_ms > 0 {
                        total_bytes * 8 * 1000 / elapsed_ms / 1000
                    } else { 0 };
                    info!(
                        "CANFD STAT elapsed_ms={} frames={} bytes={} kbps={} drops={}",
                        elapsed_ms, total_frames, total_bytes, kbps, total_drops
                    );
                }
            }
            Err(e) => {
                error!("CAN FD RX error: {:?}", e);
            }
        }
    }
}
