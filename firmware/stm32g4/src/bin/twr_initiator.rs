#![no_std]
#![no_main]

// TWR Initiator — Qorvo APS013 Double-Sided Two-Way Ranging (DS-TWR)
//
// 3-message exchange:
//   Msg 1  Poll     Initiator → Responder    seq                     (4 B)
//   Msg 2  Response Responder → Initiator    seq + T2(5B) + T3(5B)   (14 B)
//   Msg 3  Final    Initiator → Responder    seq + T1(5B) + T4(5B) + T5(5B) (19 B)
//
// Timestamps (all 40-bit DW3000 DTU, little-endian 5 bytes):
//   T1 = initiator TX Poll
//   T2 = responder RX Poll      (embedded in Response)
//   T3 = responder TX Response  (embedded in Response, via delayed TX)
//   T4 = initiator RX Response
//   T5 = initiator TX Final     (pre-calculated, embedded in Final, via delayed TX)
//   T6 = responder RX Final     (used at responder to compute distance)
//
// Distance is computed and logged by the responder (it holds all 6 timestamps).
// Formula (APS013):
//   tof = (Tround1 × Tround2 - Treply1 × Treply2) / (Tround1 + Tround2 + Treply1 + Treply2)
//   where  Tround1 = T4-T1,  Tround2 = T6-T3,  Treply1 = T3-T2,  Treply2 = T5-T4
//
// Pinout:
//   PA3=RST  PA4=CS  PA5=SCK  PA6=MISO  PA7=MOSI
//   PA0=Poll TX LED  PA1=exchange OK LED  PA8=miss/error LED

use defmt::*;
use embassy_executor::Spawner;
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::rcc::{Hse, HseMode, Pll, PllMul, PllPreDiv, PllRDiv, PllSource, Sysclk};
use embassy_stm32::spi::{self, Spi};
use embassy_stm32::time::Hertz;
use embassy_stm32::{bind_interrupts, dma, peripherals};
use embassy_time::{Delay, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;
use dw3000_ng::{Config, DW3000, configs::UwbChannel, hl::SendTime};
use dw3000_ng::time::{Instant as DwInstant, Duration as DwDuration};
use nb;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    DMA1_CHANNEL2 => dma::InterruptHandler<peripherals::DMA1_CH2>;
    DMA1_CHANNEL3 => dma::InterruptHandler<peripherals::DMA1_CH3>;
});

const POLL_ID:   &[u8] = &[0xDE, 0xCA, 0x01];
const RESP_ID:   &[u8] = &[0xDE, 0xCA, 0x02];
const FINAL_ID:  &[u8] = &[0xDE, 0xCA, 0x03];

// Delay from Response RX (T4) to Final TX (T5).
// The old 10 ms value worked but was marginal on v2: occasional missed Finals
// showed up at the responder. Keep this comfortably above RX teardown + SPI
// setup + delayed-TX programming time.
const TREPLY2_NS: u32 = 25_000_000; // 25 ms

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

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let mut config = embassy_stm32::Config::default();
    {
        config.rcc.hse = Some(Hse { freq: Hertz(8_000_000), mode: HseMode::Oscillator });
        config.rcc.pll = Some(Pll {
            source: PllSource::HSE,
            prediv: PllPreDiv::DIV2,
            mul: PllMul::MUL85,
            divp: None, divq: None,
            divr: Some(PllRDiv::DIV2),
        });
        config.rcc.sys = Sysclk::PLL1_R;
    }
    let p = embassy_stm32::init(config);

    info!("TWR Initiator (DS-TWR) starting");

    let mut poll_led = Output::new(p.PA0, Level::Low, Speed::Low);
    let mut ok_led = Output::new(p.PA1, Level::Low, Speed::Low);
    let mut err_led = Output::new(p.PA8, Level::Low, Speed::Low);

    let mut rst = Output::new(p.PA3, Level::Low, Speed::Low);
    Timer::after_millis(10).await;
    rst.set_high();
    Timer::after_millis(5).await;

    let mut spi_config = spi::Config::default();
    spi_config.frequency = Hertz(4_000_000);
    let spi = Spi::new(p.SPI1, p.PA5, p.PA7, p.PA6, p.DMA1_CH3, p.DMA1_CH2, Irqs, spi_config);
    let cs = Output::new(p.PA4, Level::High, Speed::High);
    let spi_dev = ExclusiveDevice::new(spi, cs, Delay).unwrap();

    let dw3000 = DW3000::new(spi_dev);
    let dw3000 = dw3000.init().await.expect("DW3000 init failed");
    let dw_config = Config { channel: UwbChannel::Channel9, ..Config::default() };
    let mut dw3000 = dw3000.config(dw_config, Delay).await.expect("DW3000 config failed");

    for pin in 0..4u8 {
        dw3000.gpio_config_mode(pin, 1).await.expect("gpio mode failed");
    }
    dw3000.gpio_config_clocks().await.expect("gpio clocks failed");

    info!("DW3000 ready — starting DS-TWR exchange loop");

    let mut seq: u8 = 0;
    loop {
        // ── MSG 1: Poll ──────────────────────────────────────────────────────
        let poll = [POLL_ID[0], POLL_ID[1], POLL_ID[2], seq];
        poll_led.set_high();
        let mut sending = dw3000.send(&poll, SendTime::Now, dw_config).await.expect("poll send failed");

        let t1: DwInstant = loop {
            match sending.s_wait().await {
                Ok(ts) => break ts,
                Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                Err(nb::Error::Other(_)) => core::panic!("poll s_wait failed"),
            }
        };
        dw3000 = sending.finish_sending().await.unwrap_or_else(|(_, _)| core::panic!("finish_sending"));

        // ── MSG 2: Receive Response ──────────────────────────────────────────
        let mut receiving = dw3000.receive(dw_config).await.expect("receive failed");
        let mut rx_buf = [0u8; 128];

        let deadline = embassy_time::Instant::now() + embassy_time::Duration::from_millis(500);
        let rx_data = loop {
            if embassy_time::Instant::now() > deadline { break None; }
            match receiving.r_wait_buf(&mut rx_buf).await {
                Ok((len, ts, _)) => break Some((len, ts)),
                Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                Err(nb::Error::Other(_)) => { error!("RX error on response"); break None; }
            }
        };
        dw3000 = receiving.finish_receiving().await.unwrap_or_else(|(_, _)| core::panic!("finish_receiving"));

        let Some((len, t4)) = rx_data else {
            info!("seq={} no response (timeout)", seq);
            poll_led.set_low();
            err_led.set_high();
            Timer::after_millis(200).await;
            err_led.set_low();
            seq = seq.wrapping_add(1);
            Timer::after_millis(500).await;
            continue;
        };

        // Find Response payload and extract T2, T3
        let frame = &rx_buf[..len];
        let Some(pos) = frame.windows(3).position(|w| w == RESP_ID) else {
            info!("seq={} unexpected frame (len={})", seq, len);
            poll_led.set_low();
            err_led.set_high();
            Timer::after_millis(200).await;
            err_led.set_low();
            seq = seq.wrapping_add(1);
            Timer::after_millis(200).await;
            continue;
        };
        let data = &frame[pos..];
        if data.len() < 14 {
            warn!("seq={} response too short", seq);
            poll_led.set_low();
            err_led.set_high();
            Timer::after_millis(200).await;
            err_led.set_low();
            seq = seq.wrapping_add(1);
            Timer::after_millis(200).await;
            continue;
        }
        let _t2 = unpack_ts(&data[4..]);
        let _t3 = unpack_ts(&data[9..]);

        // ── MSG 3: Final ─────────────────────────────────────────────────────
        // T5 = T4 + TREPLY2, aligned to 512-tick boundary (required by DX_TIME)
        let t5 = align512((t4 + DwDuration::from_nanos(TREPLY2_NS)).value());

        let mut final_msg = [0u8; 19];
        final_msg[0] = FINAL_ID[0];
        final_msg[1] = FINAL_ID[1];
        final_msg[2] = FINAL_ID[2];
        final_msg[3] = seq;
        pack_ts(t1.value(), &mut final_msg[4..]);
        pack_ts(t4.value(), &mut final_msg[9..]);
        pack_ts(t5.value(), &mut final_msg[14..]);

        let mut sending = dw3000.send(&final_msg, SendTime::Delayed(t5), dw_config).await.expect("final send failed");

        let tx_ok = loop {
            match sending.s_wait().await {
                Ok(_) => break true,
                Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                Err(nb::Error::Other(_)) => {
                    warn!("seq={} Final TX too late — increase TREPLY2_NS", seq);
                    break false;
                }
            }
        };
        dw3000 = sending.finish_sending().await.unwrap_or_else(|(_, _)| core::panic!("finish_sending"));

        if tx_ok {
            info!("seq={} exchange done — see responder for distance", seq);
            poll_led.set_low();
            ok_led.set_high();
            Timer::after_millis(200).await;
            ok_led.set_low();
        } else {
            poll_led.set_low();
            err_led.set_high();
            Timer::after_millis(200).await;
            err_led.set_low();
        }

        seq = seq.wrapping_add(1);
        Timer::after_millis(500).await;
    }
}
