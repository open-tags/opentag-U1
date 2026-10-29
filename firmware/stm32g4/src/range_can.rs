//! Independent, best-effort CAN publisher. No CAN await occurs in the UWB loop.

use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_stm32::can::config::{GlobalFilter, TxBufferMode};
use embassy_stm32::can::enums::BusErrorMode;
use embassy_stm32::can::{Can, CanConfigurator, Frame, IT0InterruptHandler, IT1InterruptHandler};
use embassy_stm32::{Peri, bind_interrupts, pac, peripherals};
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer, with_timeout};

#[path = "can_protocol.rs"]
mod protocol;

const DEFAULT_NODE_ID: u8 = match option_env!("OPENTAGS_CAN_NODE_ID") {
    Some(value) => match protocol::parse_node_id(value) {
        Some(node) => node,
        None => panic!("OPENTAGS_CAN_NODE_ID must be a decimal integer from 1 to 63"),
    },
    None => 1,
};
const TX_TIMEOUT_MS: u64 = 3;
const MAX_PENDING_AGE_MS: u64 = 100;

#[repr(C)]
struct NodeConfig {
    magic: [u8; 16],
    node: u32,
}

#[used]
static NODE_CONFIG: NodeConfig = NodeConfig {
    magic: *b"OTAG-CAN-NODE\0\0\0",
    node: DEFAULT_NODE_ID as u32,
};

bind_interrupts!(struct CanIrqs {
    FDCAN2_IT0 => IT0InterruptHandler<peripherals::FDCAN2>;
    FDCAN2_IT1 => IT1InterruptHandler<peripherals::FDCAN2>;
});

struct PendingRange {
    data: [u8; 8],
    measured_at: Instant,
}

static LATEST: Signal<CriticalSectionRawMutex, PendingRange> = Signal::new();
static LAST_VALID: Mutex<CriticalSectionRawMutex, Cell<Option<Instant>>> =
    Mutex::new(Cell::new(None));
static LATEST_VALID: AtomicBool = AtomicBool::new(false);
static ENABLED: AtomicBool = AtomicBool::new(true);
static NODE_ID: AtomicU32 = AtomicU32::new(DEFAULT_NODE_ID as u32);
static TX_OK: AtomicU32 = AtomicU32::new(0);
static TX_DROP: AtomicU32 = AtomicU32::new(0);
static COALESCED: AtomicU32 = AtomicU32::new(0);
static BUS_MODE: AtomicU32 = AtomicU32::new(0);
static TX_ERRORS: AtomicU32 = AtomicU32::new(0);
static RX_ERRORS: AtomicU32 = AtomicU32::new(0);

pub fn configure(
    instance: Peri<'static, peripherals::FDCAN2>,
    rx: Peri<'static, peripherals::PB12>,
    tx: Peri<'static, peripherals::PB13>,
) -> Can<'static> {
    // Like the existing calibration block, this can be personalized in the .bin
    // before flashing. Read flash at runtime so the patched ID is not folded away.
    let node = unsafe { core::ptr::read_volatile(&NODE_CONFIG.node) };
    if (1..=63).contains(&node) {
        NODE_ID.store(node, Ordering::Relaxed);
    } else {
        ENABLED.store(false, Ordering::Relaxed);
    }
    let mut can = CanConfigurator::new(instance, rx, tx, CanIrqs);
    can.set_bitrate(protocol::BITRATE);
    can.set_config(
        can.config()
            // Telemetry is replaced by the next sample, not retried indefinitely.
            .set_automatic_retransmit(false)
            .set_automatic_bus_off_recovery(true)
            // Avoid the HAL's priority-mailbox replacement path, which spins on abort.
            .set_tx_buffer_mode(TxBufferMode::Fifo)
            .set_global_filter(GlobalFilter::reject_all()),
    );
    can.into_normal_mode()
}

pub fn publish(sequence: u8, distance_mm: Option<u64>, rssi_tenths: Option<i16>) {
    let now = Instant::now();
    let data = protocol::encode_range(sequence, distance_mm, rssi_tenths);
    let valid = data[0] & 1 != 0;
    LATEST_VALID.store(valid, Ordering::Relaxed);
    if valid {
        LAST_VALID.lock(|last| last.set(Some(now)));
    }
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    if LATEST.signaled() {
        COALESCED.fetch_add(1, Ordering::Relaxed);
    }
    LATEST.signal(PendingRange {
        data,
        measured_at: now,
    });
}

pub fn reset_stats() {
    TX_OK.store(0, Ordering::Relaxed);
    TX_DROP.store(0, Ordering::Relaxed);
    COALESCED.store(0, Ordering::Relaxed);
}

pub fn report() {
    crate::say_fmt(format_args!(
        "DIAG can_on={} can_node={} can_bps={} can_proto={}",
        u8::from(ENABLED.load(Ordering::Relaxed)),
        NODE_ID.load(Ordering::Relaxed),
        protocol::BITRATE,
        protocol::VERSION
    ));
    crate::say_fmt(format_args!(
        "DIAG can_tx={} can_drop={} can_coalesced={}",
        TX_OK.load(Ordering::Relaxed),
        TX_DROP.load(Ordering::Relaxed),
        COALESCED.load(Ordering::Relaxed)
    ));
    crate::say_fmt(format_args!(
        "DIAG can_mode={} can_tec={} can_rec={}",
        BUS_MODE.load(Ordering::Relaxed),
        TX_ERRORS.load(Ordering::Relaxed),
        RX_ERRORS.load(Ordering::Relaxed)
    ));
}

pub fn handle_command(line: &str) -> bool {
    if line.eq_ignore_ascii_case("CAN ON") || line.eq_ignore_ascii_case("CAN OFF") {
        let enabled = line.eq_ignore_ascii_case("CAN ON");
        LATEST.reset();
        ENABLED.store(enabled, Ordering::Relaxed);
        crate::say(if enabled { "OK CAN ON" } else { "OK CAN OFF" });
        return true;
    }
    let Some((command, value)) = line.split_once(' ') else {
        return false;
    };
    if !command.eq_ignore_ascii_case("CAN_NODE") {
        return false;
    }
    match protocol::parse_node_id(value.trim()) {
        Some(node) => {
            LATEST.reset();
            NODE_ID.store(node as u32, Ordering::Relaxed);
            crate::say_fmt(format_args!("OK CAN_NODE {}", node));
        }
        None => crate::say("ERR CAN_NODE expected 1..63"),
    }
    true
}

fn pending() -> bool {
    (0..3).any(|slot| pac::FDCAN2.txbrp().read().trp(slot))
}

async fn cancel_pending() -> bool {
    // Only this task produces frames. Non-buffered HAL interrupts only wake it.
    // Request cancellation directly because the HAL's abort API busy-waits.
    pac::FDCAN2.txbcr().write(|w| {
        for slot in 0..3 {
            w.set_cr(slot, true);
        }
    });
    let deadline = Instant::now() + Duration::from_millis(TX_TIMEOUT_MS);
    while pending() {
        if Instant::now() >= deadline {
            return false;
        }
        Timer::after_micros(200).await;
    }
    true
}

async fn send(can: &mut Can<'static>, id_base: u16, data: &[u8; 8]) {
    let node = NODE_ID.load(Ordering::Relaxed);
    let result = async {
        if pending() && !cancel_pending().await {
            return false;
        }
        if !ENABLED.load(Ordering::Relaxed)
            || matches!(can.properties().bus_error_mode(), BusErrorMode::BusOff)
        {
            return false;
        }
        // One in-flight frame, FIFO mode, one producer: this is the mailbox the
        // HAL will use. A TX-complete bit, not just write(), confirms bus ACK.
        let slot = pac::FDCAN2.txfqs().read().tfqpi() as usize;
        let frame = Frame::new_standard(id_base + node as u16, data).unwrap();
        if with_timeout(Duration::from_millis(TX_TIMEOUT_MS), can.write(&frame))
            .await
            .is_err()
        {
            return false;
        }
        let deadline = Instant::now() + Duration::from_millis(TX_TIMEOUT_MS);
        loop {
            if !pac::FDCAN2.txbrp().read().trp(slot) {
                return pac::FDCAN2.txbto().read().to(slot);
            }
            if Instant::now() >= deadline
                || !ENABLED.load(Ordering::Relaxed)
                || node != NODE_ID.load(Ordering::Relaxed)
            {
                cancel_pending().await;
                return false;
            }
            Timer::after_micros(200).await;
        }
    }
    .await;
    if result {
        TX_OK.fetch_add(1, Ordering::Relaxed);
    } else {
        TX_DROP.fetch_add(1, Ordering::Relaxed);
    }
}

pub async fn run(mut can: Can<'static>) {
    let mut next_status = Instant::now();
    loop {
        let mode = match can.properties().bus_error_mode() {
            BusErrorMode::ErrorActive => 0,
            BusErrorMode::ErrorPassive => 1,
            BusErrorMode::BusOff => 2,
        };
        BUS_MODE.store(mode, Ordering::Relaxed);
        TX_ERRORS.store(can.properties().tx_error_count() as u32, Ordering::Relaxed);
        RX_ERRORS.store(can.properties().rx_error_count() as u32, Ordering::Relaxed);
        if ENABLED.load(Ordering::Relaxed) {
            if let Some(sample) = LATEST.try_take() {
                if sample.measured_at.elapsed().as_millis() <= MAX_PENDING_AGE_MS {
                    send(&mut can, protocol::RANGE_ID_BASE, &sample.data).await;
                } else {
                    TX_DROP.fetch_add(1, Ordering::Relaxed);
                }
            }
            if Instant::now() >= next_status {
                next_status = Instant::now() + Duration::from_secs(1);
                let age = LAST_VALID.lock(|last| last.get().map(|at| at.elapsed().as_millis()));
                let data = protocol::encode_status(
                    Instant::now().as_millis(),
                    age,
                    LATEST_VALID.load(Ordering::Relaxed),
                    TX_DROP.load(Ordering::Relaxed) != 0,
                    mode == 1,
                );
                send(&mut can, protocol::STATUS_ID_BASE, &data).await;
            }
        } else {
            LATEST.reset();
            if pending() {
                cancel_pending().await;
            }
        }
        Timer::after_millis(5).await;
    }
}
