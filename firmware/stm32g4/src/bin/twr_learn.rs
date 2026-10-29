#![no_std]
#![no_main]

// twr_learn — a from-scratch TWR learning file. One binary, two roles:
//
//   ROLE=init cargo run --release --bin twr_learn
//   ROLE=resp cargo run --release --bin twr_learn
//
// Step 1 (this file today): bring up the STM32 clocks, reset the DW3000 and
// read its DEV_ID register over raw SPI. Expect 0xDECA03xx.
// Next steps: configure the radio, send a frame, receive a frame, then TWR.

use defmt::*;
use embassy_executor::Spawner;
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::rcc::{Hse, HseMode, Pll, PllMul, PllPreDiv, PllRDiv, PllSource, Sysclk};
use embassy_stm32::spi::{self, Spi};
use embassy_stm32::time::Hertz;
use embassy_time::Timer;
use {defmt_rtt as _, panic_probe as _};

#[derive(Format)]
enum Role {
    Initiator,
    Responder,
}

/// The role is chosen when you build: the board has no command line of its own.
fn role() -> Role {
    match option_env!("ROLE") {
        Some("init") => Role::Initiator,
        Some("resp") => Role::Responder,
        _ => defmt::panic!("build with ROLE=init or ROLE=resp"),
    }
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    // 8 MHz crystal (Y2) → PLL → 144 MHz system clock, same as twr_resp.
    let mut config = embassy_stm32::Config::default();
    config.rcc.hse = Some(Hse {
        freq: Hertz(8_000_000),
        mode: HseMode::Oscillator,
    });
    config.rcc.pll = Some(Pll {
        source: PllSource::HSE,
        prediv: PllPreDiv::DIV2,
        mul: PllMul::MUL72,
        divp: None,
        divq: None,
        divr: Some(PllRDiv::DIV2),
    });
    config.rcc.sys = Sysclk::PLL1_R;
    config.rcc.boost = true;
    let p = embassy_stm32::init(config);

    let role = role();
    info!("twr_learn starting as {}", role);

    // Hold the DW3000 in reset (PA3 low), then let it boot.
    let mut rst = Output::new(p.PA3, Level::Low, Speed::Low);
    Timer::after_millis(10).await;
    rst.set_high();
    Timer::after_millis(5).await;

    // SPI1: SCK PA5, MOSI PA7, MISO PA6. Chip select PA4 is a plain GPIO.
    let mut spi_config = spi::Config::default();
    spi_config.frequency = Hertz(2_000_000);
    let mut spi = Spi::new_blocking(p.SPI1, p.PA5, p.PA7, p.PA6, spi_config);
    let mut cs = Output::new(p.PA4, Level::High, Speed::High);

    // Read DEV_ID: register file 0x00, offset 0. The 1-byte SPI header 0x00
    // means "read, short address, file 0"; the chip then clocks out 4 bytes,
    // least-significant first.
    let mut dev_id = [0u8; 4];
    cs.set_low();
    unwrap!(spi.blocking_write(&[0x00u8]));
    unwrap!(spi.blocking_read(&mut dev_id));
    cs.set_high();
    let dev_id = u32::from_le_bytes(dev_id);
    info!("DEV_ID = {=u32:#010x} (expect 0xdeca03xx)", dev_id);

    match role {
        Role::Initiator => {
            // TODO: send POLL, wait for RESP, send FINAL.
        }
        Role::Responder => {
            // TODO: wait for POLL, send RESP, wait for FINAL, compute distance.
        }
    }

    loop {
        Timer::after_secs(1).await;
        info!("alive as {}", role);
    }
}
