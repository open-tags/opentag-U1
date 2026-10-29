#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TAG_ELF="${1:-$SCRIPT_DIR/stm32g4/target/thumbv7em-none-eabi/debug/tdoa_tag}"
RAW_LOG="${TMPDIR:-/tmp}/opentag_tdoa_raw.log"
HOST_TARGET="$(rustc -vV | awk '/^host:/ { print $2 }')"

echo "tdoa_plot_live: reading RTT from $TAG_ELF" >&2
echo "tdoa_plot_live: teeing raw probe-rs output to $RAW_LOG" >&2
echo "tdoa_plot_live: compiling plotter for $HOST_TARGET" >&2

cargo build --manifest-path "$SCRIPT_DIR/twr_plot/Cargo.toml" \
  --target "$HOST_TARGET" \
  --bin tdoa_plot >&2

echo "tdoa_plot_live: starting probe-rs -> Rerun stream" >&2
probe-rs attach --chip STM32G491CE --non-interactive --rtt-scan-memory "$TAG_ELF" 2>&1 \
  | tee "$RAW_LOG" \
  | "$SCRIPT_DIR/twr_plot/target/$HOST_TARGET/debug/tdoa_plot" --echo-raw
