#![no_std]
#![no_main]

// TWR Initiator (USB) — role-locked variant of twr_initiator.rs that also
// brings up USB CDC so the web console can see the tag's INFO/state.
//
// Host commands accepted over CDC (\n-terminated):
//   INFO  -> role, firmware, device ID, target rate, and output drops
//   PING  -> "PONG"
//
// See twr_initiator.rs for the APS013 DS-TWR protocol rationale.

use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

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

bind_interrupts!(struct Irqs {
    DMA1_CHANNEL2 => dma::InterruptHandler<peripherals::DMA1_CH2>;
    DMA1_CHANNEL3 => dma::InterruptHandler<peripherals::DMA1_CH3>;
    USB_LP => usb::InterruptHandler<peripherals::USB>;
});

const POLL_ID: &[u8] = &[0xDE, 0xCA, 0x01];
const RESP_ID: &[u8] = &[0xDE, 0xCA, 0x02];
const FINAL_ID: &[u8] = &[0xDE, 0xCA, 0x03];
const TREPLY2_NS: u32 = 8_000_000;
const RESPONSE_TIMEOUT_MS: u64 = 20;
const INTER_RANGE_MS: u64 = 8;
const ERROR_BACKOFF_MS: u64 = 2;
const FW_VERSION: &str = "twr_init-5";

static OUT: Channel<CriticalSectionRawMutex, String<96>, 8> = Channel::new();
static OUT_DROPS: AtomicU32 = AtomicU32::new(0);
static HOST_CONNECTED: AtomicBool = AtomicBool::new(false);
static ATTEMPTS: AtomicU32 = AtomicU32::new(0);
static NO_RESP: AtomicU32 = AtomicU32::new(0);
static BAD_RESP: AtomicU32 = AtomicU32::new(0);
static SEQ_RESP: AtomicU32 = AtomicU32::new(0);
static RX_ERR: AtomicU32 = AtomicU32::new(0);
static FINAL_TX_ERR: AtomicU32 = AtomicU32::new(0);

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
    OUT_DROPS.store(0, Ordering::Relaxed);
    ATTEMPTS.store(0, Ordering::Relaxed);
    NO_RESP.store(0, Ordering::Relaxed);
    BAD_RESP.store(0, Ordering::Relaxed);
    SEQ_RESP.store(0, Ordering::Relaxed);
    RX_ERR.store(0, Ordering::Relaxed);
    FINAL_TX_ERR.store(0, Ordering::Relaxed);
}

fn pack_ts(ts: u64, buf: &mut [u8]) {
    buf[..5].copy_from_slice(&ts.to_le_bytes()[..5]);
}

fn align512(raw: u64) -> DwInstant {
    DwInstant::new((raw + 511) & !511u64).unwrap()
}

fn handle_command(line: &str) {
    let line = line.trim();
    start_host_session();
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
        say_fmt(format_args!(
            "INFO mode=I calib=- fw={} id={} target_hz=40 drops={}",
            FW_VERSION,
            uid::uid_hex(),
            OUT_DROPS.load(Ordering::Relaxed),
        ));
        say_fmt(format_args!(
            "DIAG attempts={} no_resp={} bad_resp={} seq_resp={}",
            ATTEMPTS.load(Ordering::Relaxed),
            NO_RESP.load(Ordering::Relaxed),
            BAD_RESP.load(Ordering::Relaxed),
            SEQ_RESP.load(Ordering::Relaxed),
        ));
        say_fmt(format_args!(
            "DIAG rx_err={} final_tx_err={}",
            RX_ERR.load(Ordering::Relaxed),
            FINAL_TX_ERR.load(Ordering::Relaxed),
        ));
        say("PHY br=850 pre=128 sfd=ieee ch=9");
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
    }
    let p = embassy_stm32::init(config);
    info!("twr_init starting");

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

    // ── USB CDC ───────────────────────────────────────────────────────────────
    let driver = Driver::new(p.USB, Irqs, p.PA12, p.PA11);
    let mut usb_config = embassy_usb::Config::new(0xc0de, 0xcafe);
    usb_config.manufacturer = Some("opentags");
    usb_config.product = Some("opentags TWR Initiator");
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

    // ── Initiator loop ───────────────────────────────────────────────────────
    let twr_fut = async {
        let mut seq: u8 = 0;
        loop {
            ATTEMPTS.fetch_add(1, Ordering::Relaxed);
            let poll = [POLL_ID[0], POLL_ID[1], POLL_ID[2], seq];
            let mut sending = dw3000
                .send(&poll, SendTime::Now, dw_config)
                .await
                .expect("poll send");
            let t1 = loop {
                match sending.s_wait().await {
                    Ok(ts) => break ts,
                    Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                    Err(nb::Error::Other(_)) => core::panic!("poll s_wait"),
                }
            };
            dw3000 = sending
                .finish_sending()
                .await
                .unwrap_or_else(|(_, _)| core::panic!("finish"));

            let mut receiving = dw3000.receive(dw_config).await.expect("rx");
            let mut rx_buf = [0u8; 128];
            let deadline = embassy_time::Instant::now()
                + embassy_time::Duration::from_millis(RESPONSE_TIMEOUT_MS);
            let rx_data = loop {
                if embassy_time::Instant::now() > deadline {
                    break None;
                }
                match receiving.r_wait_buf(&mut rx_buf).await {
                    Ok((len, ts, _)) => break Some((len, ts)),
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

            let Some((len, t4)) = rx_data else {
                NO_RESP.fetch_add(1, Ordering::Relaxed);
                stream_fmt(format_args!("MISS {} no-resp", seq));
                seq = seq.wrapping_add(1);
                Timer::after_millis(ERROR_BACKOFF_MS).await;
                continue;
            };

            let frame = &rx_buf[..len];
            let Some(pos) = frame.windows(3).position(|w| w == RESP_ID) else {
                BAD_RESP.fetch_add(1, Ordering::Relaxed);
                stream_fmt(format_args!("MISS {} bad-resp-id", seq));
                seq = seq.wrapping_add(1);
                Timer::after_millis(ERROR_BACKOFF_MS).await;
                continue;
            };
            if frame[pos..].len() < 14 {
                BAD_RESP.fetch_add(1, Ordering::Relaxed);
                seq = seq.wrapping_add(1);
                Timer::after_millis(ERROR_BACKOFF_MS).await;
                continue;
            }
            if frame[pos + 3] != seq {
                SEQ_RESP.fetch_add(1, Ordering::Relaxed);
                stream_fmt(format_args!("MISS {} resp-seq-{}", seq, frame[pos + 3]));
                seq = seq.wrapping_add(1);
                Timer::after_millis(ERROR_BACKOFF_MS).await;
                continue;
            }

            let t5 = align512((t4 + DwDuration::from_nanos(TREPLY2_NS)).value());
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
                .expect("final send");
            let final_ok = loop {
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
            if !final_ok {
                FINAL_TX_ERR.fetch_add(1, Ordering::Relaxed);
                stream_fmt(format_args!("MISS {} final-tx", seq));
            }

            seq = seq.wrapping_add(1);
            Timer::after_millis(INTER_RANGE_MS).await;
        }
    };

    let host_fut = async {
        join(cdc_fut, twr_fut).await;
    };
    join(usb_fut, host_fut).await;
}
