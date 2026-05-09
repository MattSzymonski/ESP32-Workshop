// This file is the Cargo build script for the ESP-IDF HTTP server modules project.
// - Emits the ESP-IDF system environment variables required by esp-idf-svc at compile time.
// - Must run before the main crate compiles so that IDF paths and flags are resolved.
// - Depends on: embuild (esp-idf build helper crate).

fn main() {
    embuild::espidf::sysenv::output();
}
