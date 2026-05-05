Check the implementation of [Red-Alert](https://github.com/MattSzymonski/Red-Alert) project.

## Server implementation

Uses ESP-IDF’s built-in HTTP server abstraction.

- Uses esp_idf_svc::http::server::EspHttpServer
- Runs on ESP-IDF services
- Uses std (ESP-IDF requires this)
- Registers route handlers
- Lets ESP-IDF handle HTTP parsing
- Lets ESP-IDF handle request/response structure
- Is blocking/threaded rather than Embassy async
- Usually listens on default HTTP port 80, unless configured otherwise
- Provides a higher-level API

The HTTP server framework handles the lower-level HTTP protocol details.