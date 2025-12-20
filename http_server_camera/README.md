# ESP32-S3 Camera Web Server

A Rust example for the **XIAO ESP32-S3 Sense** board that captures images from the OV3660 camera and serves them via an HTTP web interface.

## Hardware

- **Board**: Seeed Studio XIAO ESP32-S3 Sense
- **Camera**: OV3660 (auto-detected)
- **PSRAM**: 8MB (required for camera frame buffers)

## Prerequisites

1. Install Rust and the ESP32 toolchain:
   ```bash
   rustup install nightly
   rustup component add rust-src --toolchain nightly
   cargo install espup
   espup install
   ```

2. Install espflash (use v3.2.0, v4.x has bugs):
   ```bash
   cargo install espflash@3.2.0
   ```

3. On Windows, source the ESP-IDF export script or set up environment variables.

## Building

Due to Windows path length limitations, use short paths:

```powershell
# Create a junction to the project (run once as admin)
cmd /c mklink /J C:\x D:\Programming\ESP32-Workshop\http_server_camera

# Set short target directory and build
$env:CARGO_TARGET_DIR="C:\t"
cd C:\x
cargo build --release
```

On Linux/macOS:
```bash
cargo build --release
```

## Flashing

The binary is larger than the default 1MB partition, so use the custom partition table:

```powershell
espflash flash --monitor --partition-table C:\x\partitions.csv C:\t\xtensa-esp32s3-espidf\release\camera-example
```

On Linux/macOS:
```bash
espflash flash --monitor --partition-table partitions.csv target/xtensa-esp32s3-espidf/release/camera-example
```

If flashing fails, try erasing flash first:
```bash
espflash erase-flash
```

## Configuration

Edit `src/main.rs` to set your WiFi credentials:
```rust
const WIFI_SSID: &str = "your_wifi_name";
const WIFI_PASSWORD: &str = "your_wifi_password";
```

## Usage

1. Flash the firmware to the board
2. Open the serial monitor to see the assigned IP address
3. Open `http://<IP_ADDRESS>` in your browser
4. Click "Capture Photo" to take and view a picture

## Features

- **Camera**: OV3660 at 1600x1200 JPEG
- **WiFi**: Connects to configured network
- **HTTP Server**: Serves web UI on port 80
- **Endpoints**:
  - `/` - HTML page with capture button
  - `/capture` - Returns JPEG image from camera

## Environment Setup

### Rust Toolchain

This project uses the Xtensa Rust toolchain for ESP32-S3. The toolchain is configured in `rust-toolchain.toml`:

```toml
[toolchain]
channel = "esp"
```

Install the ESP Rust toolchain:
```bash
# Install espup (ESP Rust toolchain manager)
cargo install espup

# Install the ESP toolchain (includes Xtensa LLVM, GCC, etc.)
espup install

# On Windows, run the export script in each new terminal:
%USERPROFILE%\.espup\esp-idf-export.ps1

# On Linux/macOS:
source $HOME/.espup/esp-idf-export.sh
```

### ESP-IDF Version

This project uses ESP-IDF v5.1.3, configured in `.cargo/config.toml`:
```toml
ESP_IDF_VERSION = "v5.1.3"
```

### Local Patches

The `patches/` directory contains local patches for dependencies that fix compatibility issues:

#### `patches/esp-idf-svc/`
Fixes a **char signedness mismatch** between Xtensa GCC (uses `unsigned char` by default) and the `esp-idf-svc` crate (hardcodes `i8` casts). The patch changes `i8` to `c_char` in SNTP and ping modules.

**Affected files:**
- `src/sntp.rs` - SNTP server name handling
- `src/ping.rs` - Ping target/host handling

**Root cause:** Xtensa GCC defines `char` as unsigned, but `esp-idf-svc` assumes signed char.

#### `patches/esp-camera-rs/`
Local camera wrapper module for the ESP32-S3 camera interface.

### Cargo Configuration

The patches are applied via `Cargo.toml`:
```toml
[patch.crates-io]
esp-idf-svc = { path = "patches/esp-idf-svc" }
```

### Camera Component

The ESP32 camera driver is pulled from the ESP Component Registry:
```toml
[[package.metadata.esp-idf-sys.extra_components]]
remote_component = { name = "espressif/esp32-camera", version = "2.0" }
```

### Key Configuration Files

| File                  | Purpose                                         |
| --------------------- | ----------------------------------------------- |
| `.cargo/config.toml`  | Target, ESP-IDF version, build flags            |
| `rust-toolchain.toml` | Rust toolchain channel (esp)                    |
| `sdkconfig.defaults`  | ESP-IDF configuration (PSRAM, flash size, etc.) |
| `partitions.csv`      | Custom partition table (3MB app partition)      |
| `build.rs`            | Build script for camera bindings                |

## Troubleshooting

### "Image too big" error
Use the custom partition table with `--partition-table partitions.csv`

### Device stays in download mode
Press the RESET button on the board after flashing

### espflash 4.x "appdesc segment not found"
Downgrade to espflash 3.2.0: `cargo install espflash@3.2.0`

### Path too long on Windows
Use junctions and `CARGO_TARGET_DIR` as shown in the build instructions

### Char signedness errors (`*const u8` vs `*const i8`)
The Xtensa GCC toolchain uses unsigned char by default. This is fixed by the local patch in `patches/esp-idf-svc/`. If you see these errors, ensure the patch is being applied (check `Cargo.toml` has the `[patch.crates-io]` section).

### Camera not detected
- Check GPIO pin configuration in `src/camera.rs` matches your board
- XIAO ESP32-S3 Sense uses different pins than ESP32-CAM
- Ensure PSRAM is enabled in `sdkconfig.defaults`