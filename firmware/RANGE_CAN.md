# Range-over-CAN v1

The opt-in `range-can` feature adds CAN publication to `twr_resp` without changing
the USB-only build or the initiator. `src/range_can.rs` owns FDCAN2 on PB12/PB13;
the UWB task only signals a latest-value mailbox and never awaits CAN.

## Build the bench image

From `firmware/stm32g4`:

```sh
mkdir -p ../../dist
DEFMT_LOG=off cargo objcopy --profile can-release --features range-can --bin twr_resp -- \
  -O binary ../../dist/twr_resp_can.bin
```


The size-optimized profile preserves the board's existing 128 KiB bank-1 link
limit. RTT logging (including panic text) is disabled in this image to fit; USB
streaming and INFO/DIAG counters remain available. It does not enlarge the memory
map, write through the gap before bank 2, change option bytes, or change the regular release profile.
The CAN clock is PLL1_Q at 48 MHz; the MCU/radio timing stays at 144 MHz.
The hardware is not automatically flashed by this build.

Default node ID is 1. `OPENTAGS_CAN_NODE_ID=2` at build time changes the default;
alternatively [`set_can_node.py`](https://open-tags.com/docs/can/) can patch the unique
`OTAG-CAN-NODE\0\0\0` + little-endian u32 block before flashing. Volatile reads
ensure the patched value is used. `CAN_NODE n` changes only RAM, while `CAN ON`
and `CAN OFF` control publication, not electrical isolation or transceiver power.
As with the existing responder image, reapply/verify the pair's calibration.

The public protocol, reader, and node-1 DBC are in the website's `docs/can/`.
Node IDs identify the publishing responder, not a UWB peer/session. Default
IDs are 0x501 (range) and 0x541 (status); all nodes on one bus must have unique
IDs and the rest of the robot must reserve this application's ID ranges.

## Transmission policy

- Classical CAN, 500 kbit/s, standard identifiers, eight-byte payloads.
- One range record per successful/failed exchange; one heartbeat per second.
- No automatic retries; use the next range sample after a transmission failure.
- FIFO mode avoids the HAL's unbounded priority-mailbox replacement abort path.
- One producer and one in-flight mailbox; poll TX-complete, not enqueue success.
- Transmit and cancel waits are bounded at 3 ms each. A failed cancellation is
  retried before another enqueue; the task yields during every wait.
- Pending samples older than 100 ms are dropped. Receivers must enforce their
  own 250 ms measurement timeout; heartbeat reception does not refresh a range.
- Hardware bus-off auto-recovery is enabled. No source changes to Embassy are needed.
- Incoming data/remote frames are filtered out; no actuation or CAN commands are accepted.

## Software checks

```sh
rustc --edition=2024 --test src/can_protocol.rs -o /tmp/opentags-can-protocol-tests
/tmp/opentags-can-protocol-tests
cargo check --features range-can --bin twr_resp
cargo check --bin twr_resp --bin twr_init
```

The website test suite verifies decoder vectors, malformed records, invalid
sentinels, staleness, binary node personalization, and download links.

## Required bench acceptance (not yet performed)

Software-checked image (9 September 2026): 110,584 bytes, SHA-256
`7cc3f4e46facb72b7dab2d6e4c5f0fc13ee081e75db05502b80be88e15907750`.
This fingerprint is for the node-1 image before any node/calibration personalization.

1. Power an initiator and CAN-enabled responder. Use a correctly terminated
   dedicated bus and an active 500 kbit/s adapter/controller. Connect CANH,
   CANL, and ground; do not join separate positive supplies.
2. Record INFO, node IDs, build hash, OS/adapter, calibration, and both streams.
3. Match UWB sequence, millimetres/metres, and first-path power over USB and CAN.
   Verify a failed exchange emits an invalid frame rather than a zero value.
4. Turn off the UWB peer: heartbeat continues, fresh clears, receiver goes stale.
5. Remove the CAN adapter or ACK, overload the bus, and test bus-off/recovery.
   USB/UWB must continue; no old measurement backlog may replay on reconnection.
6. Power-cycle an image personalized to node 2; verify IDs 0x502/0x542 before
   joining other nodes. Test CAN OFF/ON, runtime IDs, and RESET_STATS.
7. Compare full-run ranging delivery/timing against the USB-only image. The
   CAN size optimization is not a substitute for a real performance measurement.

Keep the default website console binaries unchanged until the above is recorded.
