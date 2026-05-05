# HTTPS server

This is a simple WIFI http server BAREMETAL (no_std) implementation.
It serves full CSS-styled HTML page.
This setup is multithreaded thanks to embassy.
While server runs independently other threads can execute other operations.

## Server implementation

Implements a server “manually” on top of a raw TCP socket.

- Uses embassy-net::tcp::TcpSocket
- Listens on port 8080
- Manually reads bytes from the socket
- Manually parses the HTTP request path
- Manually writes HTTP response headers and body
- Runs in no_std
- Is fully async using Embassy
- Handles one connection at a time in the shown task
- Has very limited HTTP support

Lightweight and fits no_std, but it is not a complete HTTP server.

## Hardware

Working with Seeed XIAO ESP32-C6 board

## Development
Running (Bash):
1. Just connect via data passing cable. If not working try connecting while holding "B" button located on the board
2. `cargo build --release && espflash flash --monitor --port COM3 .\target\riscv32imac-unknown-none-elf\release\http_server`
3. Enter: `http://192.168.33.100:8080/`