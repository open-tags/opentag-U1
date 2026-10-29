#![no_std]
#![no_main]

// CAN RX node — listens for frames and echoes them back with ID 0x456.
// Flash this on the second board. Flash can_tx.rs on the first.
//
// Hardware:
//   TCAN1044VDRQ1 transceiver
//   PB12 = FDCAN2_RX (AF9)
//   PB13 = FDCAN2_TX (AF9)
//   STB pulled low (transceiver always active)

use defmt::*;
use embassy_executor::Spawner;
use embassy_stm32::can::{self, Can, Frame};
use embassy_stm32::can::filter::{StandardFilter, StandardFilterSlot};
use embassy_stm32::can::{IT0InterruptHandler, IT1InterruptHandler};
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::rcc::{Hse, HseMode, Pll, PllMul, PllPreDiv, PllRDiv, PllSource, Sysclk};
use embassy_stm32::time::Hertz;
use embassy_stm32::{bind_interrupts, peripherals};
use embassy_time::Timer;
use {defmt_rtt as _, panic_probe as _};

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
    }
    let p = embassy_stm32::init(config);

    // PA0: receive pulse, PA1: echo pulse, PA8: every-fourth-packet heartbeat.
    let mut rx_led = Output::new(p.PA0, Level::Low, Speed::Low);
    let mut echo_led = Output::new(p.PA1, Level::Low, Speed::Low);
    let mut heartbeat_led = Output::new(p.PA8, Level::Low, Speed::Low);

    info!("CAN RX node starting");

    let mut can = can::CanConfigurator::new(p.FDCAN2, p.PB12, p.PB13, Irqs);
    can.set_bitrate(500_000);

    // Accept all standard frames
    can.properties().set_standard_filter(
        StandardFilterSlot::_0,
        StandardFilter::accept_all_into_fifo0(),
    );

    let mut can: Can = can.into_normal_mode();

    info!("CAN ready at 500 kbps — waiting for frames...");

    let mut count: u8 = 0;
    loop {
        match can.read().await {
            Ok(env) => {
                rx_led.set_high();
                let raw_id = match env.frame.id() {
                    embedded_can::Id::Standard(id) => id.as_raw() as u32,
                    embedded_can::Id::Extended(id) => id.as_raw(),
                };
                info!("RX  id=0x{:03X} data={:02X}", raw_id, env.frame.data());

                Timer::after_millis(150).await;
                rx_led.set_low();

                let frame = Frame::new_standard(0x456, env.frame.data()).unwrap();
                can.write(&frame).await;
                echo_led.set_high();
                info!("TX  id=0x456 (echo)");
                Timer::after_millis(150).await;
                echo_led.set_low();

                count = count.wrapping_add(1);
                if count & 0x03 == 0 {
                    heartbeat_led.set_high();
                    Timer::after_millis(100).await;
                    heartbeat_led.set_low();
                }
            }
            Err(e) => {
                error!("RX error: {:?}", e);
                heartbeat_led.set_high();
                Timer::after_millis(500).await;
                heartbeat_led.set_low();
            }
        }
    }
}
