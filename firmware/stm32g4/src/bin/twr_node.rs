#![no_std]
#![no_main]

// TWR Node — combined initiator + responder, switchable over USB CDC.
//
// Boots in IDLE. The host (web app) sends ASCII line commands over CDC-ACM:
//
//   MODE I            switch to initiator
//   MODE R            switch to responder
//   MODE IDLE         stop ranging
//   CALIB <i32>       set responder antenna-delay offset (DTU)
//   INFO              dump current mode / calib / fw
//   PING              -> "PONG"
//
// Tag → host lines (\n terminated):
//
//   READY                                first line after USB connects
//   INFO mode=<I|R|IDLE> calib=<i32>     in response to INFO
//   OK <cmd>                             ack
//   ERR <reason>                         nack
//   D <seq> <mm>                         responder distance result
//   MISS <seq> <reason>                  responder/initiator skipped this exchange
//
// All TWR logic mirrors the stand-alone twr_initiator.rs / twr_responder.rs
// — see those files for the protocol/timing rationale.

use core::fmt::Write as _;
use core::sync::atomic::{AtomicI32, AtomicU8, Ordering};

use defmt::*;
use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_futures::select::{Either, select};
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::rcc::{
    Hse, HseMode, Hsi48Config, Pll, PllMul, PllPreDiv, PllQDiv, PllRDiv, PllSource, Sysclk, mux,
};
use embassy_stm32::spi::{self, Spi};
use embassy_stm32::time::Hertz;
use embassy_stm32::usb::{self, Driver};
use embassy_stm32::{bind_interrupts, dma, peripherals};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Delay, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;
use heapless::String;

use dw3000_ng::time::{Duration as DwDuration, Instant as DwInstant};
use dw3000_ng::{Config as DwConfig, DW3000, configs::UwbChannel, hl::SendTime};
use embassy_usb::Builder;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb::driver::EndpointError;
use nb;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    DMA1_CHANNEL2 => dma::InterruptHandler<peripherals::DMA1_CH2>;
    DMA1_CHANNEL3 => dma::InterruptHandler<peripherals::DMA1_CH3>;
    USB_LP => usb::InterruptHandler<peripherals::USB>;
});

const POLL_ID: &[u8] = &[0xDE, 0xCA, 0x01];
const RESP_ID: &[u8] = &[0xDE, 0xCA, 0x02];
const FINAL_ID: &[u8] = &[0xDE, 0xCA, 0x03];

const TREPLY_NS: u32 = 10_000_000; // 10 ms (both sides)

const MODE_IDLE: u8 = 0;
const MODE_INITIATOR: u8 = 1;
const MODE_RESPONDER: u8 = 2;
static MODE: AtomicU8 = AtomicU8::new(MODE_IDLE);

// Antenna-delay calibration offset (in DW3000 time units). Volatile — host
// re-asserts via `CALIB` over USB each session.
static CALIB_OFFSET_DTU: AtomicI32 = AtomicI32::new(12241);

static OUT: Channel<CriticalSectionRawMutex, String<96>, 8> = Channel::new();

fn say(line: &str) {
    let mut s: String<96> = String::new();
    let _ = s.push_str(line);
    let _ = OUT.try_send(s);
}

fn say_fmt(args: core::fmt::Arguments<'_>) {
    let mut s: String<96> = String::new();
    let _ = s.write_fmt(args);
    let _ = OUT.try_send(s);
}

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

fn compute_distance_mm(t_round1: u64, t_round2: u64, t_reply1: u64, t_reply2: u64) -> Option<u64> {
    let num = (t_round1 as u128)
        .checked_mul(t_round2 as u128)?
        .checked_sub((t_reply1 as u128).checked_mul(t_reply2 as u128)?)?;
    let den = t_round1 as u128 + t_round2 as u128 + t_reply1 as u128 + t_reply2 as u128;
    if den == 0 {
        return None;
    }
    let tof_dtu = num / den;
    let offset = CALIB_OFFSET_DTU.load(Ordering::Relaxed) as i64;
    let tof_dtu = (tof_dtu as i128 - offset as i128).max(0) as u128;
    let dist = tof_dtu.checked_mul(299_792_458)? / 63_897_600;
    Some(dist as u64)
}

fn handle_command(line: &str) {
    let line = line.trim();
    if line.eq_ignore_ascii_case("PING") {
        say("PONG");
        return;
    }
    if line.eq_ignore_ascii_case("INFO") {
        let mode = match MODE.load(Ordering::Relaxed) {
            MODE_INITIATOR => "I",
            MODE_RESPONDER => "R",
            _ => "IDLE",
        };
        let calib = CALIB_OFFSET_DTU.load(Ordering::Relaxed);
        say_fmt(format_args!("INFO mode={} calib={} fw=twr_node-1", mode, calib));
        return;
    }
    if let Some(rest) = strip_prefix_ci(line, "MODE ") {
        let rest = rest.trim();
        let m = if rest.eq_ignore_ascii_case("I") || rest.eq_ignore_ascii_case("INITIATOR") {
            Some(MODE_INITIATOR)
        } else if rest.eq_ignore_ascii_case("R") || rest.eq_ignore_ascii_case("RESPONDER") {
            Some(MODE_RESPONDER)
        } else if rest.eq_ignore_ascii_case("IDLE") || rest.eq_ignore_ascii_case("OFF") {
            Some(MODE_IDLE)
        } else {
            None
        };
        match m {
            Some(v) => {
                MODE.store(v, Ordering::Relaxed);
                say_fmt(format_args!("OK MODE {}", rest));
            }
            None => say("ERR bad-mode"),
        }
        return;
    }
    if let Some(rest) = strip_prefix_ci(line, "CALIB ") {
        match rest.trim().parse::<i32>() {
            Ok(v) => {
                CALIB_OFFSET_DTU.store(v, Ordering::Relaxed);
                say_fmt(format_args!("OK CALIB {}", v));
            }
            Err(_) => say("ERR bad-int"),
        }
        return;
    }
    say("ERR unknown");
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() < prefix.len() {
        return None;
    }
    let (a, b) = s.split_at(prefix.len());
    if a.eq_ignore_ascii_case(prefix) {
        Some(b)
    } else {
        None
    }
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let mut config = embassy_stm32::Config::default();
    {
        config.rcc.hsi48 = Some(Hsi48Config { sync_from_usb: true });
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
        config.rcc.mux.clk48sel = mux::Clk48sel::HSI48;
    }
    let p = embassy_stm32::init(config);

    info!("twr_node starting");

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
    let dw_config = DwConfig {
        channel: UwbChannel::Channel9,
        ..DwConfig::default()
    };
    let mut dw3000 = dw3000
        .config(dw_config, Delay)
        .await
        .expect("DW3000 config failed");

    for pin in 0..4u8 {
        dw3000
            .gpio_config_mode(pin, 1)
            .await
            .expect("gpio mode failed");
    }
    dw3000
        .gpio_config_clocks()
        .await
        .expect("gpio clocks failed");

    // ── USB CDC setup ─────────────────────────────────────────────────────────
    let driver = Driver::new(p.USB, Irqs, p.PA12, p.PA11);

    let mut usb_config = embassy_usb::Config::new(0xc0de, 0xcafe);
    usb_config.manufacturer = Some("opentags");
    usb_config.product = Some("opentags TWR Node");
    usb_config.serial_number = Some("0001");

    static CONFIG_DESC: static_cell::StaticCell<[u8; 256]> = static_cell::StaticCell::new();
    static BOS_DESC: static_cell::StaticCell<[u8; 256]> = static_cell::StaticCell::new();
    static CONTROL_BUF: static_cell::StaticCell<[u8; 64]> = static_cell::StaticCell::new();
    static STATE_CELL: static_cell::StaticCell<State> = static_cell::StaticCell::new();

    let config_desc = CONFIG_DESC.init([0; 256]);
    let bos_desc = BOS_DESC.init([0; 256]);
    let control_buf = CONTROL_BUF.init([0; 64]);
    let state = STATE_CELL.init(State::new());

    let mut builder = Builder::new(driver, usb_config, config_desc, bos_desc, &mut [], control_buf);
    let class = CdcAcmClass::new(&mut builder, state, 64);
    let mut usb = builder.build();

    let usb_fut = usb.run();

    let (mut tx, mut rx) = class.split();

    let writer_fut = async {
        loop {
            tx.wait_connection().await;
            say("READY");
            loop {
                let line = OUT.receive().await;
                let bytes = line.as_bytes();
                let mut i = 0;
                let mut ok = true;
                while i < bytes.len() {
                    let end = (i + 63).min(bytes.len());
                    if tx.write_packet(&bytes[i..end]).await.is_err() {
                        ok = false;
                        break;
                    }
                    i = end;
                }
                if !ok || tx.write_packet(b"\n").await.is_err() {
                    break;
                }
            }
            info!("writer disconnected");
        }
    };

    let reader_fut = async {
        loop {
            rx.wait_connection().await;
            info!("reader connected");
            let mut buf = [0u8; 64];
            let mut line: String<128> = String::new();
            loop {
                let n = match rx.read_packet(&mut buf).await {
                    Ok(n) => n,
                    Err(EndpointError::Disabled) => break,
                    Err(EndpointError::BufferOverflow) => continue,
                };
                for &b in &buf[..n] {
                    if b == b'\n' || b == b'\r' {
                        if !line.is_empty() {
                            handle_command(line.as_str());
                            line.clear();
                        }
                    } else if line.push(b as char).is_err() {
                        line.clear();
                    }
                }
            }
            info!("reader disconnected");
        }
    };

    let cdc_fut = async {
        match select(writer_fut, reader_fut).await {
            Either::First(_) | Either::Second(_) => {}
        }
    };

    // ── TWR loop: re-checks MODE every iteration. dw3000 moves by value through
    //    each state transition, mirroring twr_initiator.rs / twr_responder.rs. ──
    let twr_fut = async {
        let mut seq: u8 = 0;
        loop {
            match MODE.load(Ordering::Relaxed) {
                MODE_IDLE => {
                    Timer::after_millis(50).await;
                }
                MODE_INITIATOR => {
                    // ── MSG 1: Poll ──
                    let poll = [POLL_ID[0], POLL_ID[1], POLL_ID[2], seq];
                    let mut sending = dw3000
                        .send(&poll, SendTime::Now, dw_config)
                        .await
                        .expect("poll send failed");
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
                        .unwrap_or_else(|(_, _)| core::panic!("finish failed"));
                    let Some(t1) = t1 else {
                        say_fmt(format_args!("MISS {} poll-wait", seq));
                        Timer::after_millis(200).await;
                        seq = seq.wrapping_add(1);
                        continue;
                    };

                    // ── MSG 2: Receive Response ──
                    let mut receiving = dw3000.receive(dw_config).await.expect("rx start");
                    let mut rx_buf = [0u8; 128];
                    let deadline = embassy_time::Instant::now()
                        + embassy_time::Duration::from_millis(500);
                    let rx_data = loop {
                        if embassy_time::Instant::now() > deadline {
                            break None;
                        }
                        match receiving.r_wait_buf(&mut rx_buf).await {
                            Ok((len, ts, _)) => break Some((len, ts)),
                            Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                            Err(nb::Error::Other(_)) => break None,
                        }
                    };
                    dw3000 = receiving
                        .finish_receiving()
                        .await
                        .unwrap_or_else(|(_, _)| core::panic!("finish failed"));

                    let Some((len, t4)) = rx_data else {
                        say_fmt(format_args!("MISS {} no-resp", seq));
                        Timer::after_millis(200).await;
                        seq = seq.wrapping_add(1);
                        continue;
                    };

                    let frame = &rx_buf[..len];
                    let Some(pos) = frame.windows(3).position(|w| w == RESP_ID) else {
                        say_fmt(format_args!("MISS {} bad-resp-id", seq));
                        Timer::after_millis(200).await;
                        seq = seq.wrapping_add(1);
                        continue;
                    };
                    let data = &frame[pos..];
                    if data.len() < 14 {
                        say_fmt(format_args!("MISS {} resp-short", seq));
                        Timer::after_millis(200).await;
                        seq = seq.wrapping_add(1);
                        continue;
                    }

                    // ── MSG 3: Final ──
                    let t5 = align512((t4 + DwDuration::from_nanos(TREPLY_NS)).value());
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
                        .expect("final send failed");
                    let tx_ok = loop {
                        match sending.s_wait().await {
                            Ok(_) => break true,
                            Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                            Err(nb::Error::Other(_)) => break false,
                        }
                    };
                    dw3000 = sending
                        .finish_sending()
                        .await
                        .unwrap_or_else(|(_, _)| core::panic!("finish failed"));

                    if !tx_ok {
                        say_fmt(format_args!("MISS {} final-late", seq));
                    }

                    seq = seq.wrapping_add(1);
                    Timer::after_millis(200).await;
                }
                MODE_RESPONDER => {
                    // ── MSG 1: Receive Poll (bounded so MODE flips are seen) ──
                    let mut receiving = dw3000.receive(dw_config).await.expect("rx start");
                    let mut buf = [0u8; 128];
                    let deadline = embassy_time::Instant::now()
                        + embassy_time::Duration::from_millis(200);
                    let poll_result = loop {
                        if embassy_time::Instant::now() > deadline {
                            break None;
                        }
                        match receiving.r_wait_buf(&mut buf).await {
                            Ok((l, ts, _)) => break Some((l, ts)),
                            Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                            Err(nb::Error::Other(_)) => break None,
                        }
                    };
                    dw3000 = receiving
                        .finish_receiving()
                        .await
                        .unwrap_or_else(|(_, _)| core::panic!("finish failed"));

                    let Some((poll_len, t2)) = poll_result else {
                        continue;
                    };

                    let frame = &buf[..poll_len];
                    let Some(pos) = frame.windows(3).position(|w| w == POLL_ID) else {
                        say("MISS - bad-poll-id");
                        continue;
                    };
                    let r_seq = frame.get(pos + 3).copied().unwrap_or(0);

                    // ── MSG 2: Send Response ──
                    let t3 = align512((t2 + DwDuration::from_nanos(TREPLY_NS)).value());
                    let mut resp = [0u8; 14];
                    resp[0] = RESP_ID[0];
                    resp[1] = RESP_ID[1];
                    resp[2] = RESP_ID[2];
                    resp[3] = r_seq;
                    pack_ts(t2.value(), &mut resp[4..]);
                    pack_ts(t3.value(), &mut resp[9..]);

                    let mut sending = dw3000
                        .send(&resp, SendTime::Delayed(t3), dw_config)
                        .await
                        .expect("resp send failed");
                    let resp_ok = loop {
                        match sending.s_wait().await {
                            Ok(_) => break true,
                            Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                            Err(nb::Error::Other(_)) => break false,
                        }
                    };
                    dw3000 = sending
                        .finish_sending()
                        .await
                        .unwrap_or_else(|(_, _)| core::panic!("finish failed"));

                    if !resp_ok {
                        say_fmt(format_args!("MISS {} resp-late", r_seq));
                        continue;
                    }

                    // ── MSG 3: Receive Final ──
                    let mut receiving = dw3000.receive(dw_config).await.expect("rx start");
                    let deadline = embassy_time::Instant::now()
                        + embassy_time::Duration::from_millis(200);
                    let final_data = loop {
                        if embassy_time::Instant::now() > deadline {
                            break None;
                        }
                        match receiving.r_wait_buf(&mut buf).await {
                            Ok((l, ts, _)) => break Some((l, ts)),
                            Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                            Err(nb::Error::Other(_)) => break None,
                        }
                    };
                    dw3000 = receiving
                        .finish_receiving()
                        .await
                        .unwrap_or_else(|(_, _)| core::panic!("finish failed"));

                    let Some((final_len, t6)) = final_data else {
                        say_fmt(format_args!("MISS {} no-final", r_seq));
                        continue;
                    };

                    let frame = &buf[..final_len];
                    let Some(pos) = frame.windows(3).position(|w| w == FINAL_ID) else {
                        say_fmt(format_args!("MISS {} bad-final-id", r_seq));
                        continue;
                    };
                    let data = &frame[pos..];
                    if data.len() < 19 {
                        say_fmt(format_args!("MISS {} final-short", r_seq));
                        continue;
                    }

                    let t1 = unpack_ts(&data[4..]);
                    let t4 = unpack_ts(&data[9..]);
                    let t5 = unpack_ts(&data[14..]);

                    let t_round1 = DwInstant::new(t4)
                        .unwrap()
                        .duration_since(DwInstant::new(t1).unwrap())
                        .value();
                    let t_round2 = t6.duration_since(t3).value();
                    let t_reply1 = t3.duration_since(t2).value();
                    let t_reply2 = DwInstant::new(t5)
                        .unwrap()
                        .duration_since(DwInstant::new(t4).unwrap())
                        .value();

                    match compute_distance_mm(t_round1, t_round2, t_reply1, t_reply2) {
                        Some(dist_mm) => {
                            say_fmt(format_args!("D {} {}", r_seq, dist_mm));
                        }
                        None => {
                            say_fmt(format_args!("MISS {} compute-fail", r_seq));
                        }
                    }
                }
                _ => {
                    MODE.store(MODE_IDLE, Ordering::Relaxed);
                }
            }
        }
    };

    // join {usb_fut, cdc_fut + twr_fut}
    let host_fut = async {
        join(cdc_fut, twr_fut).await;
    };
    join(usb_fut, host_fut).await;
}
