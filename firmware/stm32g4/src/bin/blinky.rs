#![no_std]
#![no_main]

use defmt::*;
use embassy_executor::Spawner;
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_time::Timer;
use {defmt_rtt as _, panic_probe as _};

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_stm32::init(Default::default());
    info!("opentags v2 PA0/PA1/PA8 blinky");

    let mut pa0 = Output::new(p.PA0, Level::Low, Speed::Low);
    let mut pa1 = Output::new(p.PA1, Level::Low, Speed::Low);
    let mut pa8 = Output::new(p.PA8, Level::Low, Speed::Low);

    loop {
        info!("PA0");
        pa0.set_high();
        Timer::after_secs(1).await;
        pa0.set_low();

        info!("PA1");
        pa1.set_high();
        Timer::after_secs(1).await;
        pa1.set_low();

        info!("PA8");
        pa8.set_high();
        Timer::after_secs(1).await;
        pa8.set_low();
    }
}
