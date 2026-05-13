// This file is the Cargo build script for the ESP-IDF HTTP server modules project.
// - Emits the ESP-IDF system environment variables required by esp-idf-svc at compile time.
// - Tracks the C source under `components/gamepad_bt/` so cargo re-runs the
//   build when those files change. The actual `EXTRA_COMPONENT_DIRS` env var
//   that pulls them into the IDF build is set in `.cargo/config.toml`.
// - Must run before the main crate compiles so that IDF paths and flags are resolved.
// - Depends on: embuild (esp-idf build helper crate).

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let components_dir = std::path::Path::new(&manifest_dir).join("components");
    println!(
        "cargo:rerun-if-changed={}",
        components_dir.join("gamepad_bt").join("CMakeLists.txt").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        components_dir.join("gamepad_bt").join("bt_default_cfg.c").display()
    );

    embuild::espidf::sysenv::output();
}


