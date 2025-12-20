# HTTPS server

This is a simple WIFI http server BAREMETAL (no_std) implementation.
It serves full CSS-styled HTML page.
This setup is multithreaded thanks to embassy.
While server runs independently other threads can execute other operations.

## Hardware

Working with Seeed XIAO ESP32-C6 board

## Toolchain Setup

**rust-toolchain.toml:**
- Channel: `stable`
- Target: `riscv32imac-unknown-none-elf`
- Components: `rust-src`

**Key Dependencies:**
- `esp-hal` = 1.0.0-rc.1
- `esp-radio` = 0.16.0
- `esp-rtos` = 0.1.1
- `embassy-executor` = 0.9.0
- `embassy-net` = 0.7.0
- `embassy-time` = 0.5.0

**Rust Edition:** 2024

## Development
Running (Bash):
1. Just connect via data passing cable. If not working try connecting while holding "B" button located on the board
2. `cargo build --release && espflash flash --monitor --port COM3 .\target\riscv32imac-unknown-none-elf\release\mute`
3. Enter: `http://192.168.33.100:8080/`