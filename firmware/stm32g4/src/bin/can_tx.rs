#![no_std]
#![no_main]

// CAN TX node — sends a frame every second and listens for a response.
// Flash this on one board. Flash can_rx.rs on the other.
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
use embassy_stm32::rcc::{Hse, HseMode, Pll, PllMul, PllPreDiv, PllRDiv, PllSource, Sysclk};
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::time::Hertz;
use embassy_stm32::{bind_interrupts, peripherals};
use embassy_time::{Duration, Timer, with_timeout};
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

    // PA0: TX pulse, PA1: echo received, PA8: timeout/error.
    let mut tx_led = Output::new(p.PA0, Level::Low, Speed::Low);
    let mut ack_led = Output::new(p.PA1, Level::Low, Speed::Low);
    let mut err_led = Output::new(p.PA8, Level::Low, Speed::Low);

    info!("CAN TX node starting");

    let mut can = can::CanConfigurator::new(p.FDCAN2, p.PB12, p.PB13, Irqs);
    can.set_bitrate(500_000);

    // Accept all standard frames
    can.properties().set_standard_filter(
        StandardFilterSlot::_0,
        StandardFilter::accept_all_into_fifo0(),
    );

    let mut can: Can = can.into_normal_mode();

    info!("CAN ready at 500 kbps");

    let mut seq: u8 = 0;
    loop {
        let data = [0xCA, 0xFE, seq, 0x00];
        let frame = Frame::new_standard(0x123, &data).unwrap();

        tx_led.set_high();
        info!("TX  id=0x123 data={:02X}", data);
        can.write(&frame).await;
        Timer::after_millis(100).await;
        tx_led.set_low();

        match with_timeout(Duration::from_millis(700), can.read()).await {
            Ok(Ok(env)) => {
                let is_echo = match env.frame.id() {
                    embedded_can::Id::Standard(id) => id.as_raw() == 0x456,
                    embedded_can::Id::Extended(_) => false,
                };

                if is_echo && env.frame.data().get(0..3) == Some(&data[0..3]) {
                    let raw_id = match env.frame.id() {
                        embedded_can::Id::Standard(id) => id.as_raw() as u32,
                        embedded_can::Id::Extended(id) => id.as_raw(),
                    };
                    info!("RX  id=0x{:03X} data={:02X}", raw_id, env.frame.data());
                    ack_led.set_high();
                    Timer::after_millis(200).await;
                    ack_led.set_low();
                } else {
                    warn!("Unexpected response");
                    err_led.set_high();
                    Timer::after_millis(500).await;
                    err_led.set_low();
                }
            }
            Ok(Err(e)) => {
                error!("RX error: {:?}", e);
                err_led.set_high();
                Timer::after_millis(500).await;
                err_led.set_low();
            }
            Err(_) => {
                warn!("No response");
                err_led.set_high();
                Timer::after_millis(500).await;
                err_led.set_low();
            }
        }

        seq = seq.wrapping_add(1);
        Timer::after_secs(1).await;
    }
}
