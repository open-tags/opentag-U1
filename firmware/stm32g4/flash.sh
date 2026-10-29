#!/usr/bin/env bash
set -e

usage() {
  echo "Usage: ./flash.sh <bin-name> <board-number>"
  echo "       ./flash.sh twr_anchor 3"
  echo "       ./flash.sh blinky 1"
  echo "Known boards: 1 2 3 4 5"
  exit 1
}

BIN=${1:?$(usage)}
BOARD=${2:?$(usage)}

# Board serial numbers (from `dfu-util -l` in DFU mode)
case "$BOARD" in
  1) SERIAL="205231865532" ;;
  2) SERIAL="204F31945532" ;;
  3) SERIAL="205231885532" ;;
  4) SERIAL="205231845532" ;;
  5) SERIAL="2052318C5532" ;;
  *) echo "ERROR: Unknown board '$BOARD'. Known boards: 1 2 3 4 5"; exit 1 ;;
esac

echo "==> Flashing $BIN to board $BOARD (serial $SERIAL)"

echo "==> Building $BIN..."
# twr_anchor needs ANCHOR_ID=<board> at compile time
if [ "$BIN" = "twr_anchor" ]; then
  export ANCHOR_ID="$BOARD"
fi
cargo build --bin "$BIN"

echo "==> Converting to binary..."
cargo objcopy --bin "$BIN" -- -O binary "/tmp/$BIN.bin"

echo ""
echo "==> Enter DFU mode now: hold BOOT0, press Reset, release BOOT0"
read -r -p "    Press Enter when ready... "

# Verify the right device is visible
if ! dfu-util -l 2>/dev/null | grep -q "$SERIAL"; then
  echo "ERROR: Board $BOARD (serial $SERIAL) not found in DFU mode."
  echo "       Check BOOT0+Reset, or verify this is the right board."
  exit 1
fi

echo "==> Flashing $BIN..."
dfu-util -S "$SERIAL" -d 0483:df11 -a 0 --dfuse-address 0x08000000:leave -D "/tmp/$BIN.bin" || true
# note: dfu-util exits non-zero after leave/reset — that's expected

echo "==> Waiting for USB serial to appear..."
for i in $(seq 1 20); do
  PORT=$(ls /dev/tty.usbmodem* 2>/dev/null | head -1)
  if [ -n "$PORT" ]; then
    break
  fi
  sleep 0.5
done

if [ -z "$PORT" ]; then
  echo "ERROR: No /dev/tty.usbmodem* found after 10s. Is the board running?"
  exit 1
fi

echo "==> Listening on $PORT (Ctrl-A Ctrl-\ to quit screen)..."
screen "$PORT" 115200
