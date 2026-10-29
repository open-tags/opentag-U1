#![no_std]
#![no_main]

// CAN FD TX throughput tester
//
// Sends 64-byte frames back-to-back as fast as possible.
// Embeds a tick-count timestamp in each frame so the RX side can measure latency.
//
// Bitrates (edit constants below to experiment):
//   NOM_BPS  — arbitration phase (must match RX)
//   DATA_BPS — data phase with BRS (must match RX)
//
// Hardware: TCAN1044VDRQ1 on FDCAN2 — PB12=RX PB13=TX, STB pulled low
//
// Log format parsed by can_plot:
//   CANFD TX seq=N dropped=N

use defmt::*;
use embassy_executor::Spawner;
use embassy_stm32::can::frame::{FdFrame, Header};
use embassy_stm32::can::{self, Can};
use embassy_stm32::can::filter::{StandardFilter, StandardFilterSlot};
use embassy_stm32::can::{IT0InterruptHandler, IT1InterruptHandler};
use embassy_stm32::rcc::{Hse, HseMode, Pll, PllMul, PllPreDiv, PllRDiv, PllSource, Sysclk};
use embassy_stm32::rcc::mux::Fdcansel;
use embassy_stm32::time::Hertz;
use embassy_stm32::{bind_interrupts, peripherals};
use embassy_time::Instant;
use {defmt_rtt as _, panic_probe as _};

const NOM_BPS:  u32 = 1_000_000; // 1 Mbps  arbitration phase
const DATA_BPS: u32 = 2_000_000; // 2 Mbps  data phase (TCAN1044V max)

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

    info!("CAN FD TX starting  nom={}bps data={}bps", NOM_BPS, DATA_BPS);

    let mut configurator = can::CanConfigurator::new(p.FDCAN2, p.PB12, p.PB13, Irqs);
    configurator.set_bitrate(NOM_BPS);
    configurator.set_fd_data_bitrate(DATA_BPS, true); // TDC on

    configurator.properties().set_standard_filter(
        StandardFilterSlot::_0,
        StandardFilter::accept_all_into_fifo0(),
    );

    let mut can: Can = configurator.into_normal_mode();
    info!("CAN FD ready");

    let mut seq: u32 = 0;
    let mut dropped: u32 = 0;

    loop {
        // Embed seq + TX timestamp so RX can measure latency
        let tx_ticks = Instant::now().as_ticks() as u32;
        let mut data = [0xAAu8; 64];
        data[0..4].copy_from_slice(&seq.to_le_bytes());
        data[4..8].copy_from_slice(&tx_ticks.to_le_bytes());

        // Build FD frame with BRS (bit-rate switching for fast data phase)
        let header = Header::new_fd(
            embedded_can::StandardId::new(0x100).unwrap().into(),
            64,
            false, // not RTR
            true,  // BRS — switch to DATA_BPS for payload
        );
        let frame = FdFrame::new(header, &data).unwrap();

        if can.write_fd(&frame).await.is_some() {
            dropped += 1;
        }

        // Log every 256 frames to keep RTT overhead low
        if seq & 0xFF == 0 {
            info!("CANFD TX seq={} dropped={}", seq, dropped);
        }

        seq = seq.wrapping_add(1);
    }
}
