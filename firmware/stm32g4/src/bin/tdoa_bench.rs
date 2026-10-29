#![no_std]
#![no_main]

// TDOA bench firmware for the D1/D2 clock-sync tests (see
// products/u1/testing/2026-10-28 TDOA testing.md). One file, two roles,
// chosen at build time:
//
//   TDOA_ROLE=master cargo build --release --bin tdoa_bench
//   TDOA_ROLE=anchor cargo build --release --bin tdoa_bench
//
// master: transmits a SYNC frame every SYNC_MS (default 100 ms) at a scheduled
//   DW3000 time, with that scheduled time embedded. Logs `ST seq sched tx_ts`.
// anchor: receives continuously and logs every SYNC it hears as
//   `S seq master_sched rx_ts rssi_x10`. The host fits the anchor clock
//   against the master clock from these lines; nothing is corrected on board.
//
// SYNC frame: [DE CA A0] sched_ts[5] seq[2] (10 B). The first five bytes match
// tdoa_anchor.rs, so its anchors still parse it.
//
// Output is USB CDC (VID C0DE, PID CAFE) like twr_resp, so all three boards
// can be recorded at once without a debugger. Host commands (\n-terminated):
//   INFO          -> role, firmware, id, sync period, counters
//   SYNC_MS <n>   -> master only: set the sync period (10..4000 ms)
//   RESET_STATS   -> zero the counters
//   PING          -> PONG
//
// Pinout: PA3=RST PA4=CS PA5=SCK PA6=MISO PA7=MOSI

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

const FW_VERSION: &str = "tdoa_bench-1";
const SYNC_ID: [u8; 3] = [0xDE, 0xCA, 0xA0];
const MASK40: u64 = (1u64 << 40) - 1;

#[derive(Clone, Copy, PartialEq, Format)]
enum Role {
    Master,
    Anchor,
}

/// The role is chosen when you build: TDOA_ROLE=master or TDOA_ROLE=anchor.
/// Cargo does not always notice a changed TDOA_ROLE, so touch this file
/// between role builds.
const fn role() -> Role {
    match option_env!("TDOA_ROLE") {
        Some(r) if r.len() == 6 && r.as_bytes()[0] == b'm' => Role::Master,
        Some(r) if r.len() == 6 && r.as_bytes()[0] == b'a' => Role::Anchor,
        _ => core::panic!("build with TDOA_ROLE=master or TDOA_ROLE=anchor"),
    }
}
const ROLE: Role = role();

static SYNC_MS: AtomicU32 = AtomicU32::new(100);
static OUT: Channel<CriticalSectionRawMutex, String<96>, 16> = Channel::new();
static OUT_DROPS: AtomicU32 = AtomicU32::new(0);
static HOST_CONNECTED: AtomicBool = AtomicBool::new(false);
static SYNCS: AtomicU32 = AtomicU32::new(0); // master: sent, anchor: heard
static TX_ERR: AtomicU32 = AtomicU32::new(0);
static RX_ERR: AtomicU32 = AtomicU32::new(0);
static OTHER_FRAMES: AtomicU32 = AtomicU32::new(0);

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

fn reset_stats() {
    for c in [&OUT_DROPS, &SYNCS, &TX_ERR, &RX_ERR, &OTHER_FRAMES] {
        c.store(0, Ordering::Relaxed);
    }
}

fn pack_ts(ts: u64, buf: &mut [u8]) {
    buf[..5].copy_from_slice(&ts.to_le_bytes()[..5]);
}

fn unpack_ts(buf: &[u8]) -> u64 {
    u64::from_le_bytes([buf[0], buf[1], buf[2], buf[3], buf[4], 0, 0, 0])
}

// Round up to the 512-tick grid that delayed TX uses, staying in 40 bits.
fn align512(raw: u64) -> DwInstant {
    DwInstant::new(((raw + 511) & !511u64) & MASK40).unwrap()
}

fn strip_prefix_ci<'a>(s: &'a str, p: &str) -> Option<&'a str> {
    if s.len() < p.len() {
        return None;
    }
    let (a, b) = s.split_at(p.len());
    a.eq_ignore_ascii_case(p).then_some(b)
}

fn handle_command(line: &str) {
    let line = line.trim();
    HOST_CONNECTED.store(true, Ordering::Relaxed);
    if line.eq_ignore_ascii_case("PING") {
        say("PONG");
    } else if line.eq_ignore_ascii_case("RESET_STATS") {
        reset_stats();
        say("OK RESET_STATS");
    } else if line.eq_ignore_ascii_case("INFO") {
        say_fmt(format_args!(
            "INFO mode={} fw={} git={} id={} sync_ms={} drops={}",
            if ROLE == Role::Master { "M" } else { "A" },
            FW_VERSION,
            env!("GIT_HASH"),
            uid::uid_hex(),
            SYNC_MS.load(Ordering::Relaxed),
            OUT_DROPS.load(Ordering::Relaxed),
        ));
        say_fmt(format_args!(
            "DIAG syncs={} tx_err={} rx_err={} other={}",
            SYNCS.load(Ordering::Relaxed),
            TX_ERR.load(Ordering::Relaxed),
            RX_ERR.load(Ordering::Relaxed),
            OTHER_FRAMES.load(Ordering::Relaxed),
        ));
        say("PHY br=850 pre=128 sfd=ieee ch=9");
    } else if let Some(rest) = strip_prefix_ci(line, "SYNC_MS ") {
        match rest.trim().parse::<u32>() {
            Ok(v) if ROLE == Role::Master && (10..=4000).contains(&v) => {
                SYNC_MS.store(v, Ordering::Relaxed);
                say_fmt(format_args!("OK SYNC_MS {}", v));
            }
            Ok(_) => say("ERR sync-ms (master only, 10..4000)"),
            Err(_) => say("ERR bad-int"),
        }
    } else {
        say("ERR unknown");
    }
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    // Same clocks as twr_resp: 8 MHz HSE -> 144 MHz, USB from HSI48.
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
    info!("tdoa_bench starting as {}", ROLE);

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

    // Radio config A, identical to twr_resp / twr_init.
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
    usb_config.product = Some(match ROLE {
        Role::Master => "opentags TDOA Master",
        Role::Anchor => "opentags TDOA Anchor",
    });
    usb_config.serial_number = Some(uid::uid_hex());

    static CONFIG_DESC: static_cell::StaticCell<[u8; 256]> = static_cell::StaticCell::new();
    static BOS_DESC: static_cell::StaticCell<[u8; 256]> = static_cell::StaticCell::new();
    static CONTROL_BUF: static_cell::StaticCell<[u8; 64]> = static_cell::StaticCell::new();
    static STATE_CELL: static_cell::StaticCell<State> = static_cell::StaticCell::new();
    let mut builder = Builder::new(
        driver,
        usb_config,
        CONFIG_DESC.init([0; 256]),
        BOS_DESC.init([0; 256]),
        &mut [],
        CONTROL_BUF.init([0; 64]),
    );
    let class = CdcAcmClass::new(&mut builder, STATE_CELL.init(State::new()), 64);
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
            let mut line: String<64> = String::new();
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

    // ── Radio loop ───────────────────────────────────────────────────────────
    let radio_fut = async {
        match ROLE {
            Role::Master => {
                let mut seq: u16 = 0;
                // None = no TX timestamp to schedule from yet (boot, or after a late TX).
                let mut last_tx: Option<DwInstant> = None;
                loop {
                    let sync_ms = SYNC_MS.load(Ordering::Relaxed);
                    let mut frame = [0u8; 10];
                    frame[0..3].copy_from_slice(&SYNC_ID);
                    frame[8..10].copy_from_slice(&seq.to_le_bytes());

                    // Scheduled SYNC: embed the scheduled time so anchors know when it left.
                    // Unscheduled restart SYNC: embed 0 so anchors skip it.
                    let send_time = match last_tx {
                        Some(t) => {
                            let sched = align512((t + DwDuration::from_nanos(sync_ms * 1_000_000)).value());
                            pack_ts(sched.value(), &mut frame[3..8]);
                            SendTime::Delayed(sched)
                        }
                        None => SendTime::Now,
                    };
                    let sched_value = match send_time {
                        SendTime::Delayed(s) => s.value(),
                        _ => 0,
                    };

                    let mut sending = dw3000
                        .send(&frame, send_time, dw_config)
                        .await
                        .expect("sync send");
                    if sched_value != 0 {
                        // Sleep most of the period instead of polling SPI the whole time.
                        Timer::after_millis(sync_ms.saturating_sub(3) as u64).await;
                    }
                    let result = loop {
                        match sending.s_wait().await {
                            Ok(ts) => break Some(ts),
                            Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                            Err(nb::Error::Other(_)) => break None,
                        }
                    };
                    dw3000 = sending
                        .finish_sending()
                        .await
                        .unwrap_or_else(|(_, _)| core::panic!("finish_sending"));

                    match result {
                        Some(ts) => {
                            SYNCS.fetch_add(1, Ordering::Relaxed);
                            stream_fmt(format_args!("ST {} {} {}", seq, sched_value, ts.value()));
                            last_tx = Some(ts);
                        }
                        None => {
                            TX_ERR.fetch_add(1, Ordering::Relaxed);
                            stream_fmt(format_args!("MISS {} tx-late", seq));
                            last_tx = None;
                        }
                    }
                    seq = seq.wrapping_add(1);
                }
            }
            Role::Anchor => loop {
                let mut receiving = dw3000.receive(dw_config).await.expect("rx");
                let mut buf = [0u8; 128];
                let result = loop {
                    match receiving.r_wait_buf(&mut buf).await {
                        Ok(r) => break Some(r),
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
                    .unwrap_or_else(|(_, _)| core::panic!("finish_receiving"));
                let Some((len, rx_ts, quality)) = result else {
                    continue;
                };

                let frame = &buf[..len];
                let Some(pos) = frame.windows(3).position(|w| w == SYNC_ID) else {
                    OTHER_FRAMES.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                if frame.len() < pos + 10 {
                    OTHER_FRAMES.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                let sched = unpack_ts(&frame[pos + 3..]);
                let seq = u16::from_le_bytes([frame[pos + 8], frame[pos + 9]]);
                SYNCS.fetch_add(1, Ordering::Relaxed);
                stream_fmt(format_args!(
                    "S {} {} {} {}",
                    seq,
                    sched,
                    rx_ts.value(),
                    (quality.rssi * 10.0) as i16
                ));
            },
        }
    };

    join(usb_fut, join(cdc_fut, radio_fut)).await;
}
