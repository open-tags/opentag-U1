#![no_std]
#![no_main]

// TWR Responder (USB) — role-locked variant of twr_responder.rs that adds
// USB CDC so the web console gets `D <seq> <mm> <exchange_us>` lines and can
// push antenna-delay calibration via `CALIB <i32>`.
//
// Host commands accepted over CDC (\n-terminated):
//   INFO        -> role, calibration, firmware, device ID, target rate, and drops
//   CALIB <n>   -> set antenna-delay offset (DTU); -> "OK CALIB <n>"
//   PING        -> "PONG"

use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use defmt::*;
use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_futures::select::select;
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::rcc::{
    Hse, HseMode, Hsi48Config, Pll, PllMul, PllPreDiv, PllQDiv, PllRDiv, PllSource, Sysclk, mux,
};
use embassy_stm32::spi::{self, Spi};
use embassy_stm32::time::Hertz;
use embassy_stm32::usb::{self, Driver};
use embassy_stm32::{bind_interrupts, dma, peripherals, uid};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Delay, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;
use heapless::String;

use dw3000_ng::time::{Duration as DwDuration, Instant as DwInstant};
use dw3000_ng::{
    Config as DwConfig, DW3000,
    configs::{BitRate, PreambleLength, SfdSequence, UwbChannel},
    hl::SendTime,
};
use embassy_usb::Builder;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb::driver::EndpointError;
use nb;
use {defmt_rtt as _, panic_probe as _};

#[cfg(feature = "range-can")]
#[path = "../range_can.rs"]
mod range_can;

bind_interrupts!(struct Irqs {
    DMA1_CHANNEL2 => dma::InterruptHandler<peripherals::DMA1_CH2>;
    DMA1_CHANNEL3 => dma::InterruptHandler<peripherals::DMA1_CH3>;
    USB_LP => usb::InterruptHandler<peripherals::USB>;
});

const POLL_ID: &[u8] = &[0xDE, 0xCA, 0x01];
const RESP_ID: &[u8] = &[0xDE, 0xCA, 0x02];
const FINAL_ID: &[u8] = &[0xDE, 0xCA, 0x03];
const TREPLY1_NS: u32 = 8_000_000;
const FINAL_TIMEOUT_MS: u64 = 20;
#[cfg(not(feature = "range-can"))]
const FW_VERSION: &str = "twr_resp-5";
#[cfg(feature = "range-can")]
const FW_VERSION: &str = "twr_resp-can-1";

// Default antenna-delay calibration baked into the .bin. The web flasher
// finds CALIB_MAGIC inside the binary and overwrites the i32 immediately
// after with the user's last calibration before flashing — that way each
// re-flash carries forward whatever offset was last computed.
//
// Layout: [16 bytes magic][i32 little-endian offset]
const CALIB_MAGIC: [u8; 16] = *b"OTAG-CALIB-DEF\0\0";

#[repr(C)]
struct CalibBlock {
    magic: [u8; 16],
    offset: i32,
}

#[used]
static CALIB_BLOCK: CalibBlock = CalibBlock {
    magic: CALIB_MAGIC,
    offset: 12241,
};

static CALIB_OFFSET_DTU: AtomicI32 = AtomicI32::new(0);
static OUT: Channel<CriticalSectionRawMutex, String<96>, 8> = Channel::new();
static OUT_DROPS: AtomicU32 = AtomicU32::new(0);
static HOST_CONNECTED: AtomicBool = AtomicBool::new(false);
static POLLS: AtomicU32 = AtomicU32::new(0);
static NO_FINAL: AtomicU32 = AtomicU32::new(0);
static BAD_FINAL: AtomicU32 = AtomicU32::new(0);
static SEQ_FINAL: AtomicU32 = AtomicU32::new(0);
static RX_ERR: AtomicU32 = AtomicU32::new(0);
static RESP_TX_ERR: AtomicU32 = AtomicU32::new(0);

fn say(line: &str) {
    let mut s: String<96> = String::new();
    let _ = s.push_str(line);
    if OUT.try_send(s).is_err() {
        OUT_DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

fn say_fmt(args: core::fmt::Arguments<'_>) {
    let mut s: String<96> = String::new();
    let _ = s.write_fmt(args);
    if OUT.try_send(s).is_err() {
        OUT_DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

fn stream_fmt(args: core::fmt::Arguments<'_>) {
    if HOST_CONNECTED.load(Ordering::Relaxed) {
        say_fmt(args);
    }
}

fn start_host_session() {
    if !HOST_CONNECTED.swap(true, Ordering::Relaxed) {
        OUT_DROPS.store(0, Ordering::Relaxed);
    }
}

fn reset_stats() {
    #[cfg(feature = "range-can")]
    range_can::reset_stats();
    OUT_DROPS.store(0, Ordering::Relaxed);
    POLLS.store(0, Ordering::Relaxed);
    NO_FINAL.store(0, Ordering::Relaxed);
    BAD_FINAL.store(0, Ordering::Relaxed);
    SEQ_FINAL.store(0, Ordering::Relaxed);
    RX_ERR.store(0, Ordering::Relaxed);
    RESP_TX_ERR.store(0, Ordering::Relaxed);
}

fn pack_ts(ts: u64, buf: &mut [u8]) {
    buf[..5].copy_from_slice(&ts.to_le_bytes()[..5]);
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
    let offset = CALIB_OFFSET_DTU.load(Ordering::Relaxed) as i64;
    let corrected_num = (num as i128)
        .checked_sub((offset as i128).checked_mul(den as i128)?)?
        .max(0) as u128;
    let scaled_num = corrected_num.checked_mul(299_792_458)?;
    let scaled_den = den.checked_mul(63_897_600)?;
    Some((scaled_num / scaled_den) as u64)
}

fn strip_prefix_ci<'a>(s: &'a str, p: &str) -> Option<&'a str> {
    if s.len() < p.len() {
        return None;
    }
    let (a, b) = s.split_at(p.len());
    if a.eq_ignore_ascii_case(p) {
        Some(b)
    } else {
        None
    }
}

fn handle_command(line: &str) {
    let line = line.trim();
    start_host_session();
    #[cfg(feature = "range-can")]
    if range_can::handle_command(line) {
        return;
    }
    if line.eq_ignore_ascii_case("RESET_STATS") {
        reset_stats();
        say("OK RESET_STATS");
        return;
    }
    if line.eq_ignore_ascii_case("PING") {
        say("PONG");
        return;
    }
    if line.eq_ignore_ascii_case("INFO") {
        let calib = CALIB_OFFSET_DTU.load(Ordering::Relaxed);
        say_fmt(format_args!(
            "INFO mode=R calib={} fw={} id={} target_hz=40 drops={}",
            calib,
            FW_VERSION,
            uid::uid_hex(),
            OUT_DROPS.load(Ordering::Relaxed),
        ));
        say_fmt(format_args!(
            "DIAG polls={} no_final={} bad_final={} seq_final={}",
            POLLS.load(Ordering::Relaxed),
            NO_FINAL.load(Ordering::Relaxed),
            BAD_FINAL.load(Ordering::Relaxed),
            SEQ_FINAL.load(Ordering::Relaxed),
        ));
        say_fmt(format_args!(
            "DIAG rx_err={} resp_tx_err={}",
            RX_ERR.load(Ordering::Relaxed),
            RESP_TX_ERR.load(Ordering::Relaxed),
        ));
        say("PHY br=850 pre=128 sfd=ieee ch=9");
        #[cfg(feature = "range-can")]
        range_can::report();
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

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let mut config = embassy_stm32::Config::default();
    {
        config.rcc.hsi48 = Some(Hsi48Config {
            sync_from_usb: true,
        });
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
        #[cfg(feature = "range-can")]
        {
            config.rcc.mux.fdcansel = mux::Fdcansel::PLL1_Q;
        } // 48 MHz, separate from the USB clock.
    }
    let p = embassy_stm32::init(config);
    #[cfg(feature = "range-can")]
    let can = range_can::configure(p.FDCAN2, p.PB12, p.PB13);

    // Pull the patched calibration default out of flash. read_volatile
    // prevents the optimizer from constant-folding it (which would
    // make patched bins flash but ignore the new value).
    let initial_calib = unsafe { core::ptr::read_volatile(&CALIB_BLOCK.offset) };
    CALIB_OFFSET_DTU.store(initial_calib, Ordering::Relaxed);

    info!("twr_resp starting (calib={})", initial_calib);

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
        sfd_sequence: SfdSequence::IeeeShort,
        preamble_length: PreambleLength::Symbols128,
        bitrate: BitRate::Kbps850,
        sfd_timeout: 129,
        ..DwConfig::default()
    };
    let mut dw3000 = dw3000
        .config(dw_config, Delay)
        .await
        .expect("DW3000 config failed");
    dw3000
        .set_full_cia_diagnostics(true)
        .await
        .expect("CIA diagnostics");
    for pin in 0..4u8 {
        dw3000.gpio_config_mode(pin, 1).await.expect("gpio mode");
    }
    dw3000.gpio_config_clocks().await.expect("gpio clocks");

    let driver = Driver::new(p.USB, Irqs, p.PA12, p.PA11);
    let mut usb_config = embassy_usb::Config::new(0xc0de, 0xcafe);
    usb_config.manufacturer = Some("opentags");
    usb_config.product = Some("opentags TWR Responder");
    usb_config.serial_number = Some(uid::uid_hex());

    static CONFIG_DESC: static_cell::StaticCell<[u8; 256]> = static_cell::StaticCell::new();
    static BOS_DESC: static_cell::StaticCell<[u8; 256]> = static_cell::StaticCell::new();
    static CONTROL_BUF: static_cell::StaticCell<[u8; 64]> = static_cell::StaticCell::new();
    static STATE_CELL: static_cell::StaticCell<State> = static_cell::StaticCell::new();
    let config_desc = CONFIG_DESC.init([0; 256]);
    let bos_desc = BOS_DESC.init([0; 256]);
    let control_buf = CONTROL_BUF.init([0; 64]);
    let state = STATE_CELL.init(State::new());

    let mut builder = Builder::new(
        driver,
        usb_config,
        config_desc,
        bos_desc,
        &mut [],
        control_buf,
    );
    let class = CdcAcmClass::new(&mut builder, state, 64);
    let mut usb = builder.build();
    let usb_fut = usb.run();
    let (mut tx, mut rx) = class.split();

    let writer_fut = async {
        loop {
            tx.wait_connection().await;
            while OUT.try_receive().is_ok() {}
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
                    HOST_CONNECTED.store(false, Ordering::Relaxed);
                    break;
                }
            }
            while OUT.try_receive().is_ok() {}
        }
    };

    let reader_fut = async {
        loop {
            rx.wait_connection().await;
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
        }
    };

    let cdc_fut = async {
        let _ = select(writer_fut, reader_fut).await;
    };

    // ── Responder loop ───────────────────────────────────────────────────────
    let twr_fut = async {
        loop {
            let mut receiving = dw3000.receive(dw_config).await.expect("rx");
            let mut buf = [0u8; 128];
            let poll_result = loop {
                match receiving.r_wait_buf(&mut buf).await {
                    Ok((l, ts, _)) => break Some((l, ts)),
                    Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                    Err(nb::Error::Other(_)) => {
                        RX_ERR.fetch_add(1, Ordering::Relaxed);
                        break None;
                    }
                }
            };
            dw3000 = receiving
                .finish_receiving()
                .await
                .unwrap_or_else(|(_, _)| core::panic!("finish"));
            let Some((poll_len, t2)) = poll_result else {
                continue;
            };
            POLLS.fetch_add(1, Ordering::Relaxed);
            let exchange_start = embassy_time::Instant::now();

            let frame = &buf[..poll_len];
            let Some(pos) = frame.windows(3).position(|w| w == POLL_ID) else {
                continue;
            };
            let seq = frame.get(pos + 3).copied().unwrap_or(0);

            let t3 = align512((t2 + DwDuration::from_nanos(TREPLY1_NS)).value());
            let mut resp = [0u8; 14];
            resp[0] = RESP_ID[0];
            resp[1] = RESP_ID[1];
            resp[2] = RESP_ID[2];
            resp[3] = seq;
            pack_ts(t2.value(), &mut resp[4..]);
            pack_ts(t3.value(), &mut resp[9..]);

            let mut sending = dw3000
                .send(&resp, SendTime::Delayed(t3), dw_config)
                .await
                .expect("resp send");
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
                .unwrap_or_else(|(_, _)| core::panic!("finish"));
            if !resp_ok {
                RESP_TX_ERR.fetch_add(1, Ordering::Relaxed);
                #[cfg(feature = "range-can")]
                range_can::publish(seq, None, None);
                continue;
            }

            let mut receiving = dw3000.receive(dw_config).await.expect("rx");
            let deadline = embassy_time::Instant::now()
                + embassy_time::Duration::from_millis(FINAL_TIMEOUT_MS);
            let final_data = loop {
                if embassy_time::Instant::now() > deadline {
                    break None;
                }
                match receiving.r_wait_buf(&mut buf).await {
                    Ok((l, ts, quality)) => break Some((l, ts, quality)),
                    Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                    Err(nb::Error::Other(_)) => {
                        RX_ERR.fetch_add(1, Ordering::Relaxed);
                        break None;
                    }
                }
            };
            dw3000 = receiving
                .finish_receiving()
                .await
                .unwrap_or_else(|(_, _)| core::panic!("finish"));

            let Some((final_len, t6, final_quality)) = final_data else {
                NO_FINAL.fetch_add(1, Ordering::Relaxed);
                stream_fmt(format_args!("MISS {} no-final", seq));
                #[cfg(feature = "range-can")]
                range_can::publish(seq, None, None);
                continue;
            };

            let frame = &buf[..final_len];
            let Some(pos) = frame.windows(3).position(|w| w == FINAL_ID) else {
                BAD_FINAL.fetch_add(1, Ordering::Relaxed);
                stream_fmt(format_args!("MISS {} bad-final-id", seq));
                #[cfg(feature = "range-can")]
                range_can::publish(seq, None, None);
                continue;
            };
            let data = &frame[pos..];
            if data.len() < 19 {
                BAD_FINAL.fetch_add(1, Ordering::Relaxed);
                #[cfg(feature = "range-can")]
                range_can::publish(seq, None, None);
                continue;
            }
            if data[3] != seq {
                SEQ_FINAL.fetch_add(1, Ordering::Relaxed);
                stream_fmt(format_args!("MISS {} final-seq-{}", seq, data[3]));
                #[cfg(feature = "range-can")]
                range_can::publish(seq, None, None);
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
                    let exchange_us = embassy_time::Instant::now()
                        .duration_since(exchange_start)
                        .as_micros();
                    let rssi_tenths = (final_quality.rssi * 10.0) as i16;
                    #[cfg(feature = "range-can")]
                    range_can::publish(
                        seq,
                        Some(dist_mm),
                        final_quality.rssi.is_finite().then_some(rssi_tenths),
                    );
                    stream_fmt(format_args!(
                        "D {} {} {} {}",
                        seq, dist_mm, exchange_us, rssi_tenths
                    ));
                }
                None => {
                    stream_fmt(format_args!("MISS {} compute-fail", seq));
                    #[cfg(feature = "range-can")]
                    range_can::publish(seq, None, None);
                }
            }
        }
    };

    let host_fut = async {
        join(cdc_fut, twr_fut).await;
    };
    #[cfg(feature = "range-can")]
    embassy_futures::join::join3(usb_fut, host_fut, range_can::run(can)).await;
    #[cfg(not(feature = "range-can"))]
    join(usb_fut, host_fut).await;
}
