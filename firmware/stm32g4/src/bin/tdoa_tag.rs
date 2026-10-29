#![no_std]
#![no_main]

// Configurable TDOA Location Tag.
//
// The tag transmits a BLINK, receives master-clock-corrected REPORT timestamps
// from either three anchors (2D with a configured tag height) or four anchors
// (3D), solves its own position, and streams fixes over USB CDC.
//
// USB output:
//   READY
//   INFO mode=TDOA dimensions=<2|3> anchors=<3|4> biases=<b1>,<b2>,<b3>
//   P <seq> <x_mm> <y_mm> <z_mm> <raw10> <raw20> <raw30>
//   MISS <seq> <anchor-bitmask>
//   CAL result <bias10> <bias20> <bias30> n=<samples>
//
// USB commands:
//   INFO
//   BIAS <b10> <b20> [b30]
//   CAL START <x_mm> <y_mm> <z_mm> <samples>
//   CAL STOP

use core::fmt::Write as _;
use core::sync::atomic::{AtomicI32, AtomicU32, Ordering};

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
use embassy_time::{Delay, Duration, Instant, Timer};
use embassy_usb::Builder;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb::driver::EndpointError;
use embedded_hal_bus::spi::ExclusiveDevice;
use heapless::String;
use libm::{roundf, sqrtf};

use dw3000_ng::time::Instant as DwInstant;
use dw3000_ng::{Config, DW3000, configs::UwbChannel, hl::SendTime};
use nb;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    DMA1_CHANNEL2 => dma::InterruptHandler<peripherals::DMA1_CH2>;
    DMA1_CHANNEL3 => dma::InterruptHandler<peripherals::DMA1_CH3>;
    USB_LP => usb::InterruptHandler<peripherals::USB>;
});

const BLINK_ID: [u8; 3] = [0xDE, 0xCA, 0xB0];
const REPORT_ID: [u8; 3] = [0xDE, 0xCA, 0xC0];
const MASK40: u64 = (1u64 << 40) - 1;
const MAX_ANCHORS: usize = 4;
const FILTER_LEN: usize = 5;
const DTU_TO_MM: f32 = 299_792_458.0 / 63_897_600.0;

// Browser-patchable configuration (88 bytes, little-endian):
//   00 magic[16] = "OTAG-TDOA-T-V1!!"
//   16 version, 17 anchor_count, 18 dimensions, 19 reserved
//   20 fixed_tag_z_mm: i32 (used by 2D mode)
//   24 anchors[4][3]: i32 millimetres (x, y, z)
//   72 tdoa_biases[4]: i32 DTU (A0 must be zero)
#[used]
#[unsafe(link_section = ".rodata.tdoa_tag_config")]
static TDOA_TAG_CONFIG: [u8; 88] = [
    0x4f, 0x54, 0x41, 0x47, 0x2d, 0x54, 0x44, 0x4f, 0x41, 0x2d, 0x54, 0x2d, 0x56, 0x31, 0x21, 0x21,
    0x01, 0x04, 0x03, 0x00, 0x92, 0x03, 0x00, 0x00, 0xe5, 0x00, 0x00, 0x00, 0xe5, 0x00, 0x00, 0x00,
    0x09, 0x08, 0x00, 0x00, 0x03, 0x0b, 0x00, 0x00, 0xe5, 0x00, 0x00, 0x00, 0x62, 0x02, 0x00, 0x00,
    0x03, 0x0b, 0x00, 0x00, 0x03, 0x0b, 0x00, 0x00, 0x09, 0x08, 0x00, 0x00, 0xe5, 0x00, 0x00, 0x00,
    0x03, 0x0b, 0x00, 0x00, 0x62, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

static BIAS_1: AtomicI32 = AtomicI32::new(0);
static BIAS_2: AtomicI32 = AtomicI32::new(0);
static BIAS_3: AtomicI32 = AtomicI32::new(0);

static CAL_REMAIN: AtomicU32 = AtomicU32::new(0);
static CAL_TARGET: AtomicU32 = AtomicU32::new(0);
static CAL_X: AtomicI32 = AtomicI32::new(0);
static CAL_Y: AtomicI32 = AtomicI32::new(0);
static CAL_Z: AtomicI32 = AtomicI32::new(0);
static CAL_SUM_1: AtomicI32 = AtomicI32::new(0);
static CAL_SUM_2: AtomicI32 = AtomicI32::new(0);
static CAL_SUM_3: AtomicI32 = AtomicI32::new(0);

static OUT: Channel<CriticalSectionRawMutex, String<192>, 12> = Channel::new();

fn say(line: &str) {
    let mut s: String<192> = String::new();
    let _ = s.push_str(line);
    let _ = OUT.try_send(s);
}

fn say_fmt(args: core::fmt::Arguments<'_>) {
    let mut s: String<192> = String::new();
    let _ = s.write_fmt(args);
    let _ = OUT.try_send(s);
}

fn cfg_i32(offset: usize) -> i32 {
    i32::from_le_bytes([
        TDOA_TAG_CONFIG[offset],
        TDOA_TAG_CONFIG[offset + 1],
        TDOA_TAG_CONFIG[offset + 2],
        TDOA_TAG_CONFIG[offset + 3],
    ])
}

fn anchor_position(index: usize) -> [f32; 3] {
    let offset = 24 + index * 12;
    [
        cfg_i32(offset) as f32,
        cfg_i32(offset + 4) as f32,
        cfg_i32(offset + 8) as f32,
    ]
}

fn configured_bias(index: usize) -> i32 {
    cfg_i32(72 + index * 4)
}

fn runtime_bias(index: usize) -> i32 {
    match index {
        1 => BIAS_1.load(Ordering::Relaxed),
        2 => BIAS_2.load(Ordering::Relaxed),
        3 => BIAS_3.load(Ordering::Relaxed),
        _ => 0,
    }
}

fn unpack_ts(buf: &[u8]) -> u64 {
    u64::from_le_bytes([buf[0], buf[1], buf[2], buf[3], buf[4], 0, 0, 0])
}

fn tdoa_dtu(a: u64, b: u64) -> i64 {
    let diff = (a.wrapping_sub(b)) & MASK40;
    if diff & (1u64 << 39) != 0 {
        (diff | !MASK40) as i64
    } else {
        diff as i64
    }
}

fn median(mut values: [i64; FILTER_LEN], len: usize) -> i64 {
    let len = len.min(FILTER_LEN);
    for i in 1..len {
        let mut j = i;
        while j > 0 && values[j - 1] > values[j] {
            values.swap(j - 1, j);
            j -= 1;
        }
    }
    values[len / 2]
}

fn distance(point: [f32; 3], anchor: [f32; 3]) -> f32 {
    sqrtf(
        (point[0] - anchor[0]) * (point[0] - anchor[0])
            + (point[1] - anchor[1]) * (point[1] - anchor[1])
            + (point[2] - anchor[2]) * (point[2] - anchor[2]),
    )
    .max(1.0)
}

fn solve_2d(tdoa_mm: [f32; 3]) -> Option<[f32; 3]> {
    let a0 = anchor_position(0);
    let a1 = anchor_position(1);
    let a2 = anchor_position(2);
    let z = cfg_i32(20) as f32;
    let mut x = (a0[0] + a1[0] + a2[0]) / 3.0;
    let mut y = (a0[1] + a1[1] + a2[1]) / 3.0;

    for _ in 0..16 {
        let point = [x, y, z];
        let d0 = distance(point, a0);
        let d1 = distance(point, a1);
        let d2 = distance(point, a2);
        let f1 = d1 - d0 - tdoa_mm[0];
        let f2 = d2 - d0 - tdoa_mm[1];
        let j11 = (x - a1[0]) / d1 - (x - a0[0]) / d0;
        let j12 = (y - a1[1]) / d1 - (y - a0[1]) / d0;
        let j21 = (x - a2[0]) / d2 - (x - a0[0]) / d0;
        let j22 = (y - a2[1]) / d2 - (y - a0[1]) / d0;
        let det = j11 * j22 - j12 * j21;
        if det.abs() < 1e-7 {
            return None;
        }
        let step_x = (j22 * f1 - j12 * f2) / det;
        let step_y = (-j21 * f1 + j11 * f2) / det;
        x -= step_x;
        y -= step_y;
        if step_x * step_x + step_y * step_y < 0.01 {
            break;
        }
    }
    if x.is_finite() && y.is_finite() {
        Some([x, y, z])
    } else {
        None
    }
}

fn solve_3d(tdoa_mm: [f32; 3]) -> Option<[f32; 3]> {
    let anchors = [
        anchor_position(0),
        anchor_position(1),
        anchor_position(2),
        anchor_position(3),
    ];
    let mut point = [0.0f32; 3];
    for anchor in anchors {
        for axis in 0..3 {
            point[axis] += anchor[axis] / 4.0;
        }
    }

    for _ in 0..20 {
        let d0 = distance(point, anchors[0]);
        let mut f = [0.0f32; 3];
        let mut j = [[0.0f32; 3]; 3];
        for row in 0..3 {
            let di = distance(point, anchors[row + 1]);
            f[row] = di - d0 - tdoa_mm[row];
            for axis in 0..3 {
                j[row][axis] = (point[axis] - anchors[row + 1][axis]) / di
                    - (point[axis] - anchors[0][axis]) / d0;
            }
        }
        let det = j[0][0] * (j[1][1] * j[2][2] - j[1][2] * j[2][1])
            - j[0][1] * (j[1][0] * j[2][2] - j[1][2] * j[2][0])
            + j[0][2] * (j[1][0] * j[2][1] - j[1][1] * j[2][0]);
        if det.abs() < 1e-9 {
            return None;
        }
        let det_x = f[0] * (j[1][1] * j[2][2] - j[1][2] * j[2][1])
            - j[0][1] * (f[1] * j[2][2] - j[1][2] * f[2])
            + j[0][2] * (f[1] * j[2][1] - j[1][1] * f[2]);
        let det_y = j[0][0] * (f[1] * j[2][2] - j[1][2] * f[2])
            - f[0] * (j[1][0] * j[2][2] - j[1][2] * j[2][0])
            + j[0][2] * (j[1][0] * f[2] - f[1] * j[2][0]);
        let det_z = j[0][0] * (j[1][1] * f[2] - f[1] * j[2][1])
            - j[0][1] * (j[1][0] * f[2] - f[1] * j[2][0])
            + f[0] * (j[1][0] * j[2][1] - j[1][1] * j[2][0]);
        let step = [det_x / det, det_y / det, det_z / det];
        for axis in 0..3 {
            point[axis] -= step[axis];
        }
        if step[0] * step[0] + step[1] * step[1] + step[2] * step[2] < 0.01 {
            break;
        }
    }
    if point.iter().all(|value| value.is_finite()) {
        Some(point)
    } else {
        None
    }
}

fn expected_tdoa(point: [f32; 3], index: usize) -> i32 {
    roundf(
        (distance(point, anchor_position(index)) - distance(point, anchor_position(0))) / DTU_TO_MM,
    ) as i32
}

fn record_calibration(raw: [i64; 3], anchor_count: usize) {
    let remaining = CAL_REMAIN.load(Ordering::Relaxed);
    if remaining == 0 {
        return;
    }
    CAL_SUM_1.fetch_add(raw[0] as i32, Ordering::Relaxed);
    CAL_SUM_2.fetch_add(raw[1] as i32, Ordering::Relaxed);
    if anchor_count == 4 {
        CAL_SUM_3.fetch_add(raw[2] as i32, Ordering::Relaxed);
    }
    if CAL_REMAIN.fetch_sub(1, Ordering::Relaxed) != 1 {
        return;
    }

    let n = CAL_TARGET.load(Ordering::Relaxed).max(1) as i32;
    let point = [
        CAL_X.load(Ordering::Relaxed) as f32,
        CAL_Y.load(Ordering::Relaxed) as f32,
        CAL_Z.load(Ordering::Relaxed) as f32,
    ];
    let b1 = CAL_SUM_1.load(Ordering::Relaxed) / n - expected_tdoa(point, 1);
    let b2 = CAL_SUM_2.load(Ordering::Relaxed) / n - expected_tdoa(point, 2);
    let b3 = if anchor_count == 4 {
        CAL_SUM_3.load(Ordering::Relaxed) / n - expected_tdoa(point, 3)
    } else {
        0
    };
    BIAS_1.store(b1, Ordering::Relaxed);
    BIAS_2.store(b2, Ordering::Relaxed);
    BIAS_3.store(b3, Ordering::Relaxed);
    say_fmt(format_args!("CAL result {} {} {} n={}", b1, b2, b3, n));
}

fn handle_command(line: &str, anchor_count: usize, dimensions: u8) {
    let mut parts = line.split_whitespace();
    let Some(command) = parts.next() else {
        return;
    };
    if command.eq_ignore_ascii_case("INFO") {
        say_fmt(format_args!(
            "INFO mode=TDOA dimensions={} anchors={} biases={},{},{}",
            dimensions,
            anchor_count,
            BIAS_1.load(Ordering::Relaxed),
            BIAS_2.load(Ordering::Relaxed),
            BIAS_3.load(Ordering::Relaxed)
        ));
        return;
    }
    if command.eq_ignore_ascii_case("BIAS") {
        let parsed = (parts.next(), parts.next(), parts.next());
        if let (Some(a), Some(b), c) = parsed {
            if let (Ok(b1), Ok(b2), Ok(b3)) = (
                a.parse::<i32>(),
                b.parse::<i32>(),
                c.unwrap_or("0").parse::<i32>(),
            ) {
                BIAS_1.store(b1, Ordering::Relaxed);
                BIAS_2.store(b2, Ordering::Relaxed);
                BIAS_3.store(b3, Ordering::Relaxed);
                say_fmt(format_args!("OK BIAS {} {} {}", b1, b2, b3));
                return;
            }
        }
        say("ERR bad-bias");
        return;
    }
    if command.eq_ignore_ascii_case("CAL") {
        let action = parts.next().unwrap_or("");
        if action.eq_ignore_ascii_case("STOP") {
            CAL_REMAIN.store(0, Ordering::Relaxed);
            say("OK CAL STOP");
            return;
        }
        if action.eq_ignore_ascii_case("START") {
            let parsed = (parts.next(), parts.next(), parts.next(), parts.next());
            if let (Some(x), Some(y), Some(z), Some(n)) = parsed {
                if let (Ok(x), Ok(y), Ok(z), Ok(n)) = (
                    x.parse::<i32>(),
                    y.parse::<i32>(),
                    z.parse::<i32>(),
                    n.parse::<u32>(),
                ) {
                    if (10..=500).contains(&n) {
                        CAL_X.store(x, Ordering::Relaxed);
                        CAL_Y.store(y, Ordering::Relaxed);
                        CAL_Z.store(z, Ordering::Relaxed);
                        CAL_SUM_1.store(0, Ordering::Relaxed);
                        CAL_SUM_2.store(0, Ordering::Relaxed);
                        CAL_SUM_3.store(0, Ordering::Relaxed);
                        CAL_TARGET.store(n, Ordering::Relaxed);
                        CAL_REMAIN.store(n, Ordering::Relaxed);
                        say_fmt(format_args!("OK CAL START {} {} {} n={}", x, y, z, n));
                        return;
                    }
                }
            }
            say("ERR bad-cal");
            return;
        }
    }
    say("ERR unknown");
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let anchor_count = TDOA_TAG_CONFIG[17] as usize;
    let dimensions = TDOA_TAG_CONFIG[18];
    if TDOA_TAG_CONFIG[16] != 1
        || !((anchor_count == 3 && dimensions == 2) || (anchor_count == 4 && dimensions == 3))
    {
        core::panic!("invalid TDOA tag config");
    }
    BIAS_1.store(configured_bias(1), Ordering::Relaxed);
    BIAS_2.store(configured_bias(2), Ordering::Relaxed);
    BIAS_3.store(configured_bias(3), Ordering::Relaxed);

    let mut config = embassy_stm32::Config::default();
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
    let p = embassy_stm32::init(config);

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
    let dw3000 = DW3000::new(spi_dev).init().await.expect("DW3000 init");
    let dw_config = Config {
        channel: UwbChannel::Channel9,
        ..Config::default()
    };
    let mut dw3000 = dw3000
        .config(dw_config, Delay)
        .await
        .expect("DW3000 config");
    for pin in 0..4u8 {
        dw3000.gpio_config_mode(pin, 1).await.expect("gpio mode");
    }
    dw3000.gpio_config_clocks().await.expect("gpio clocks");

    let driver = Driver::new(p.USB, Irqs, p.PA12, p.PA11);
    let mut usb_config = embassy_usb::Config::new(0xc0de, 0xcafe);
    usb_config.manufacturer = Some("opentags");
    usb_config.product = Some("opentags TDOA Location Tag");
    usb_config.serial_number = Some("TDOA");
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
                let mut offset = 0;
                let mut connected = true;
                while offset < bytes.len() {
                    let end = (offset + 63).min(bytes.len());
                    if tx.write_packet(&bytes[offset..end]).await.is_err() {
                        connected = false;
                        break;
                    }
                    offset = end;
                }
                if !connected || tx.write_packet(b"\n").await.is_err() {
                    break;
                }
            }
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
                for &byte in &buf[..n] {
                    if byte == b'\n' || byte == b'\r' {
                        if !line.is_empty() {
                            handle_command(line.as_str(), anchor_count, dimensions);
                            line.clear();
                        }
                    } else if line.push(byte as char).is_err() {
                        line.clear();
                    }
                }
            }
        }
    };
    let cdc_fut = async {
        match select(writer_fut, reader_fut).await {
            Either::First(_) | Either::Second(_) => {}
        }
    };

    let tdoa_fut = async {
        let mut seq = 0u8;
        let mut windows = [[0i64; FILTER_LEN]; 3];
        let mut window_index = 0usize;
        let mut window_len = 0usize;
        loop {
            let blink = [BLINK_ID[0], BLINK_ID[1], BLINK_ID[2], seq];
            let mut sending = dw3000
                .send(&blink, SendTime::Now, dw_config)
                .await
                .expect("blink tx");
            loop {
                match sending.s_wait().await {
                    Ok(_) => break,
                    Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                    Err(nb::Error::Other(_)) => core::panic!("blink wait"),
                }
            }
            dw3000 = sending
                .finish_sending()
                .await
                .unwrap_or_else(|(_, _)| core::panic!("finish tx"));

            let mut reports: [Option<u64>; MAX_ANCHORS] = [None; MAX_ANCHORS];
            let deadline = Instant::now() + Duration::from_millis(70);
            while Instant::now() < deadline
                && !reports[..anchor_count]
                    .iter()
                    .all(|report| report.is_some())
            {
                let mut receiving = dw3000.receive(dw_config).await.expect("rx start");
                let mut buf = [0u8; 64];
                let mut received: Option<(usize, DwInstant)> = None;
                loop {
                    if Instant::now() >= deadline {
                        break;
                    }
                    match receiving.r_wait_buf(&mut buf).await {
                        Ok((len, ts, _)) => {
                            received = Some((len, ts));
                            break;
                        }
                        Err(nb::Error::WouldBlock) => Timer::after_micros(100).await,
                        Err(_) => break,
                    }
                }
                dw3000 = receiving
                    .finish_receiving()
                    .await
                    .unwrap_or_else(|(_, _)| core::panic!("finish rx"));
                let Some((len, _)) = received else {
                    continue;
                };
                let frame = &buf[..len];
                let Some(pos) = frame.windows(3).position(|window| window == REPORT_ID) else {
                    continue;
                };
                if frame.len() < pos + 10 || frame[pos + 4] != seq {
                    continue;
                }
                let anchor_id = frame[pos + 3] as usize;
                if anchor_id < anchor_count {
                    reports[anchor_id] = Some(unpack_ts(&frame[pos + 5..]));
                }
            }

            if reports[..anchor_count]
                .iter()
                .all(|report| report.is_some())
            {
                let t0 = reports[0].unwrap();
                let mut raw = [0i64; 3];
                let mut valid = true;
                for index in 1..anchor_count {
                    raw[index - 1] = tdoa_dtu(reports[index].unwrap(), t0);
                    let baseline = distance(anchor_position(index), anchor_position(0));
                    let max_dtu = (baseline / DTU_TO_MM) as i64 + 100;
                    if (raw[index - 1] - runtime_bias(index) as i64).abs() > max_dtu {
                        valid = false;
                    }
                }
                if valid {
                    record_calibration(raw, anchor_count);
                    for index in 0..(anchor_count - 1) {
                        windows[index][window_index] = raw[index] - runtime_bias(index + 1) as i64;
                    }
                    window_index = (window_index + 1) % FILTER_LEN;
                    window_len = (window_len + 1).min(FILTER_LEN);
                    let mut filtered_mm = [0.0f32; 3];
                    for index in 0..(anchor_count - 1) {
                        filtered_mm[index] = median(windows[index], window_len) as f32 * DTU_TO_MM;
                    }
                    let position = if dimensions == 2 {
                        solve_2d(filtered_mm)
                    } else {
                        solve_3d(filtered_mm)
                    };
                    if let Some(position) = position {
                        say_fmt(format_args!(
                            "P {} {} {} {} {} {} {}",
                            seq,
                            position[0] as i32,
                            position[1] as i32,
                            position[2] as i32,
                            raw[0],
                            raw[1],
                            raw[2]
                        ));
                    } else {
                        say_fmt(format_args!("MISS {} solve", seq));
                    }
                } else {
                    say_fmt(format_args!("MISS {} geometry", seq));
                }
            } else {
                let mut mask = 0u8;
                for index in 0..anchor_count {
                    if reports[index].is_some() {
                        mask |= 1 << index;
                    }
                }
                say_fmt(format_args!("MISS {} mask={}", seq, mask));
            }
            seq = seq.wrapping_add(1);
            Timer::after_millis(120).await;
        }
    };

    let host_fut = async {
        join(cdc_fut, tdoa_fut).await;
    };
    join(usb_fut, host_fut).await;
}
