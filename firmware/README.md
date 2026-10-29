# opentags Firmware

Rust firmware for the opentag U1 PCB V2 board (`STM32G474CCTx`) using the [Embassy](https://embassy.dev) async framework.

## Hardware

- **MCU**: STM32G474CCTx (Cortex-M4F)
- **Debugger**: ST-Link V2 via SWD
- **UWB**: DWM3000 via SPI
- **Power**: USB-C

### Pinout

See the KiCad schematic in [`../hardware/`](../hardware/opentag.kicad_sch).

---

## Setup

### 1. Clone with submodules

```sh
git clone --recurse-submodules <repo-url>
# or, if already cloned:
git submodule update --init
```

Cargo fetches the Embassy HAL from GitHub at a pinned commit (see `stm32g4/Cargo.toml`); no separate checkout is needed.

### 2. Install Rust

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup target add thumbv7em-none-eabi
```

### 3. Install probe-rs (flash & debug)

```sh
brew install probe-rs/probe-rs/probe-rs
```

### 4. Install Rerun (host tools only)

Only needed if you want to run the `twr_plot` visualisation tools — not required for building firmware.
See the [Rerun install guide](https://rerun.io/docs/overview/installing-rerun/rust).

---

## Building & Flashing

All firmware targets live in `stm32g4/src/bin/`.

### v1 vs v2 MCU targets

opentags v1 used `STM32G491CET6`; opentags v2 uses `STM32G474CCTx`.
Firmware is not binary-compatible between those MCUs. Build and flash with the
matching `embassy-stm32` chip feature and `probe-rs --chip` value:

| Board | MCU | Embassy feature | probe-rs chip |
|-------|-----|-----------------|---------------|
| v1 | `STM32G491CET6` | `stm32g491ce` | `STM32G491CE` |
| v2 | `STM32G474CCTx` | `stm32g474cc` | `STM32G474CC` |

The Rust source can often be shared between v1 and v2 when it only uses
peripherals and pins present on both chips, but the compiled binary must match
the exact MCU memory map and peripheral definitions.

**Build:**
```sh
cd stm32g4
cargo build --bin blinky
```

**Flash & run** (ST-Link must be connected):
```sh
cargo run --bin blinky
```

`DEFMT_LOG=trace` is set by default in `.cargo/config.toml`, so defmt logs stream over RTT automatically.

### Available firmware targets

| Binary | Description |
|--------|-------------|
| `blinky` | LED blink sanity check |
| `button` / `button_exti` | GPIO input |
| `can` / `can_rx` / `can_tx` / `can_fd_rx` / `can_fd_tx` | CAN bus |
| `twr_initiator` / `twr_responder` / `twr_calib_responder` | Two-way ranging |
| `tdoa_anchor` / `tdoa_tag` | TDOA localisation |
| `usb_serial` / `usb_c_pd` | USB |
| `adc` / `adc_dma` / `adc_differential` / `adc_oversampling` | ADC |
| `i2c_slave` / `i2s` / `neopixel` / `pll` | Peripherals |

---

## Host Tools (`twr_plot`)

Visualisation and analysis tools that run on your Mac, consuming data logged from the firmware over CAN/serial.

```sh
cd twr_plot
cargo run --bin twr_plot
cargo run --bin can_plot
cargo run --bin twr_locate
cargo run --bin tdoa_plot
cargo run --bin tdoa_calibrate
```

Live TDOA plot helper:
```sh
./tdoa_plot_live.sh
```

---

## Notes

### External crystal

The board uses an 8 MHz crystal on PF0/PF1 with 14 pF load caps. Use `HseMode::Oscillator` instead of the default HSI:

```rust
use embassy_stm32::rcc::{Hse, HseMode};
use embassy_stm32::Config;

let mut config = Config::default();
config.rcc.hse = Some(Hse {
    freq: embassy_stm32::time::Hertz(8_000_000),
    mode: HseMode::Oscillator,
});
config.rcc.sys = embassy_stm32::rcc::Sysclk::HSE;
let p = embassy_stm32::init(config);
```
