#![no_std]
#![no_main]

// NeoPixel diagnostic: cycles through full-brightness R → G → B → off
// so you can tell at a glance whether the LED responds and what colors work.

use defmt::*;
use embassy_executor::Spawner;
use embassy_stm32::gpio::OutputType;
use embassy_stm32::rcc::{Hse, HseMode, Pll, PllMul, PllPreDiv, PllRDiv, PllSource, Sysclk};
use embassy_stm32::time::{khz, Hertz};
use embassy_stm32::timer::Channel;
use embassy_stm32::timer::low_level::CountingMode;
use embassy_stm32::timer::simple_pwm::{PwmPin, SimplePwm};
use embassy_stm32::{bind_interrupts, dma, peripherals};
use embassy_time::{Duration, Timer};
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    DMA1_CHANNEL1 => dma::InterruptHandler<peripherals::DMA1_CH1>;
});

fn make_frame(r: u8, g: u8, b: u8, n0: u16, n1: u16) -> [u16; 25] {
    let mut buf = [0u16; 25];
    // WS2812 order: G, R, B — MSB first
    for (byte_idx, &byte) in [g, r, b].iter().enumerate() {
        for bit in 0..8u8 {
            buf[byte_idx * 8 + bit as usize] = if byte & (0x80 >> bit) != 0 { n1 } else { n0 };
        }
    }
    // buf[24] = 0 — trailing low for >50µs reset
    buf
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
            divp: None,
            divq: None,
            divr: Some(PllRDiv::DIV2),
        });
        config.rcc.sys = Sysclk::PLL1_R;
    }
    let mut p = embassy_stm32::init(config);

    info!("NeoPixel starting (170 MHz)");

    let mut pwm = SimplePwm::new(
        p.TIM2,
        Some(PwmPin::new(p.PA0, OutputType::PushPull)),
        None, None, None,
        khz(800),
        CountingMode::EdgeAlignedUp,
    );

    let max = pwm.max_duty_cycle() as u16;
    info!("PWM max={} (expect 212 at 170MHz/800kHz)", max);

    // WS2812B: T0H≈0.4µs (32%), T1H≈0.8µs (64%) at 1.25µs period
    let n0 = max * 32 / 100;
    let n1 = max * 64 / 100;
    info!("n0={} n1={}", n0, n1);

    let colors: [(&str, [u16; 25]); 4] = [
        ("RED",   make_frame(0xFF, 0x00, 0x00, n0, n1)),
        ("GREEN", make_frame(0x00, 0xFF, 0x00, n0, n1)),
        ("BLUE",  make_frame(0x00, 0x00, 0xFF, n0, n1)),
        ("OFF",   make_frame(0x00, 0x00, 0x00, n0, n1)),
    ];

    let ch = Channel::Ch1;
    pwm.channel(ch).set_duty_cycle(0);
    pwm.channel(ch).enable();

    let mut cycle = 0u32;
    loop {
        for (name, frame) in &colors {
            info!("cycle={} color={}", cycle, name);
            pwm.waveform_up(p.DMA1_CH1.reborrow(), Irqs, ch, frame).await;
            // Reset pulse: hold data low for >50µs (WS2812) / >280µs (SK6812)
            Timer::after_micros(300).await;
            Timer::after(Duration::from_millis(800)).await;
        }
        cycle += 1;
    }
}
