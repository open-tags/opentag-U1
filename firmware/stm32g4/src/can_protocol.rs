//! Range-over-CAN v1. Pure encoding shared by the firmware and native tests.

pub const BITRATE: u32 = 500_000;
pub const RANGE_ID_BASE: u16 = 0x500;
pub const STATUS_ID_BASE: u16 = 0x540;
pub const VERSION: u8 = 1;
pub const FRESH_MS: u64 = 250;
pub const INVALID_DISTANCE: u32 = u32::MAX;
pub const INVALID_RSSI: i16 = i16::MAX;

pub const fn parse_node_id(value: &str) -> Option<u8> {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > 2 {
        return None;
    }
    let mut node = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] < b'0' || bytes[i] > b'9' {
            return None;
        }
        node = node * 10 + bytes[i] - b'0';
        i += 1;
    }
    if node >= 1 && node <= 63 {
        Some(node)
    } else {
        None
    }
}

pub fn encode_range(sequence: u8, distance_mm: Option<u64>, rssi_tenths: Option<i16>) -> [u8; 8] {
    let distance = distance_mm.filter(|&mm| mm < INVALID_DISTANCE as u64);
    let rssi = rssi_tenths.filter(|&rssi| distance.is_some() && rssi != INVALID_RSSI);
    let mut data = [0; 8];
    data[0] = (VERSION << 4) | u8::from(distance.is_some()) | (u8::from(rssi.is_some()) << 1);
    data[1] = sequence;
    data[2..6].copy_from_slice(&(distance.map_or(INVALID_DISTANCE, |mm| mm as u32)).to_le_bytes());
    data[6..8].copy_from_slice(&rssi.unwrap_or(INVALID_RSSI).to_le_bytes());
    data
}

pub fn encode_status(
    uptime_ms: u64,
    last_valid_age_ms: Option<u64>,
    latest_valid: bool,
    had_tx_drop: bool,
    error_passive: bool,
) -> [u8; 8] {
    let fresh = latest_valid && last_valid_age_ms.is_some_and(|age| age <= FRESH_MS);
    let age = last_valid_age_ms.map_or(u16::MAX, |ms| ms.min(u16::MAX as u64) as u16);
    let mut data = [0; 8];
    data[0] = VERSION;
    data[1] = u8::from(fresh) | (u8::from(had_tx_drop) << 1) | (u8::from(error_passive) << 2);
    data[2..4].copy_from_slice(&age.to_le_bytes());
    data[4..8].copy_from_slice(&(uptime_ms as u32).to_le_bytes());
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_range_golden_vector() {
        assert_eq!(
            encode_range(42, Some(3046), Some(-748)),
            [0x13, 42, 0xe6, 0x0b, 0, 0, 0x14, 0xfd]
        );
    }

    #[test]
    fn invalid_reading_is_not_zero_distance() {
        assert_eq!(
            encode_range(255, None, None),
            [0x10, 255, 255, 255, 255, 255, 255, 127]
        );
        assert_eq!(
            encode_range(0, Some(0), None),
            [0x11, 0, 0, 0, 0, 0, 255, 127]
        );
    }

    #[test]
    fn overflow_and_rssi_sentinel_are_not_valid() {
        assert_eq!(encode_range(1, Some(u64::MAX), Some(-10))[0], 0x10);
        assert_eq!(encode_range(1, Some(u32::MAX as u64), Some(-10))[0], 0x10);
        assert_eq!(encode_range(1, Some(1000), Some(i16::MAX))[0], 0x11);
        assert_eq!(
            encode_range(1, Some(u32::MAX as u64 - 1), Some(i16::MIN))[0],
            0x13
        );
    }

    #[test]
    fn status_golden_vector_and_expiry() {
        assert_eq!(
            encode_status(1000, Some(25), true, false, false),
            [1, 1, 25, 0, 0xe8, 3, 0, 0]
        );
        assert_eq!(encode_status(0, Some(250), true, true, true)[1], 7);
        assert_eq!(encode_status(0, Some(251), true, false, false)[1], 0);
        assert_eq!(encode_status(0, Some(1), false, false, false)[1], 0);
    }

    #[test]
    fn status_never_seen_saturates_and_uptime_wraps() {
        assert_eq!(
            encode_status(0, None, true, false, false),
            [1, 0, 255, 255, 0, 0, 0, 0]
        );
        assert_eq!(
            encode_status(u32::MAX as u64 + 2, Some(100_000), true, false, false),
            [1, 0, 255, 255, 1, 0, 0, 0]
        );
    }

    #[test]
    fn node_ids_are_bounded_and_message_ranges_do_not_overlap() {
        for node in 1..=63 {
            assert_eq!(parse_node_id(&node.to_string()), Some(node));
            assert!(RANGE_ID_BASE + (node as u16) < STATUS_ID_BASE);
            assert!(STATUS_ID_BASE + node as u16 <= 0x7ff);
        }
        for text in ["", "0", "64", "99", "256", "-1", "+1", "1x", " 1", "é"] {
            assert_eq!(parse_node_id(text), None, "{text}");
        }
    }
}
