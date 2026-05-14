// Comprehensive memory diagnostics for the ESP32-C6 (or any ESP32 family chip).
//
// Call `report("label")` at checkpoints before the main loop to dump a full,
// structured, human-readable snapshot to the serial console.
//
// Sections emitted per call:
//   1. CHIP / RUNTIME     chip model, IDF version, uptime, reset reason
//   2. MEMORY MAP         ESP32-C6 static region table with addresses/sizes/permissions
//   3. LINKER SEGMENTS    .bss / .data sizes from linker boundary symbols
//   4. HEAP REGIONS       multi_heap_info_t per capability bucket, ASCII bar, frag %
//   5. TASK STACK         high-water mark of the calling FreeRTOS task
//   6. ANOMALY SCAN       auto-flags fragmentation, low heap, tiny blocks, stack risk

use esp_idf_svc::sys;
use log::{info, warn};

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Render a 20-char ASCII usage bar: [████████░░░░░░░░░░░░]  40%
fn bar(used: usize, total: usize) -> String {
    const W: usize = 20;
    if total == 0 {
        return format!("[{}]   n/a", " ".repeat(W));
    }
    let pct = (used * 100 / total).min(100);
    let filled = (used * W / total).min(W);
    format!(
        "[{}{}] {:>3}%",
        "\u{2588}".repeat(filled),
        "\u{2591}".repeat(W - filled),
        pct,
    )
}

fn kib(b: usize) -> String {
    format!("{:>5} KiB", b / 1024)
}

// ─── 1. chip / runtime ───────────────────────────────────────────────────────

fn print_chip(label: &str) {
    let uptime_ms = (unsafe { sys::esp_timer_get_time() } as u64) / 1_000;
    let free_int = unsafe { sys::esp_get_free_internal_heap_size() } as usize;
    // PSRAM size via heap_caps (always available; returns 0 if no PSRAM fitted).
    let psram_size = unsafe { sys::heap_caps_get_total_size(sys::MALLOC_CAP_SPIRAM) };
    let pkg_ver = unsafe { sys::esp_efuse_get_pkg_ver() };
    let reset_raw = unsafe { sys::esp_reset_reason() };

    let reset = match reset_raw {
        x if x == sys::esp_reset_reason_t_ESP_RST_POWERON => "Power-on",
        x if x == sys::esp_reset_reason_t_ESP_RST_SW => "Software reset",
        x if x == sys::esp_reset_reason_t_ESP_RST_PANIC => "Panic / exception",
        x if x == sys::esp_reset_reason_t_ESP_RST_INT_WDT => "Interrupt WDT",
        x if x == sys::esp_reset_reason_t_ESP_RST_TASK_WDT => "Task WDT",
        x if x == sys::esp_reset_reason_t_ESP_RST_WDT => "Other WDT",
        x if x == sys::esp_reset_reason_t_ESP_RST_DEEPSLEEP => "Deep-sleep wakeup",
        x if x == sys::esp_reset_reason_t_ESP_RST_BROWNOUT => "Brownout",
        x if x == sys::esp_reset_reason_t_ESP_RST_SDIO => "SDIO reset",
        _ => "Unknown",
    };

    // Chip ID is the eFuse-burned hardware identifier (2-byte value).
    // CONFIG_IDF_FIRMWARE_CHIP_ID is the *target* chip baked in at compile time.
    let chip_id = sys::CONFIG_IDF_FIRMWARE_CHIP_ID;
    let model = match chip_id {
        0 => "ESP32",
        2 => "ESP32-S2",
        5 => "ESP32-C3",
        9 => "ESP32-S3",
        12 => "ESP32-C2",
        13 => "ESP32-C6", // ← this project
        16 => "ESP32-H2",
        18 => "ESP32-P4",
        _ => "unknown",
    };

    let idf_ver = unsafe {
        core::ffi::CStr::from_ptr(sys::esp_get_idf_version())
            .to_str()
            .unwrap_or("?")
    };

    info!(
        "[{}] ┌─────────────────────────── CHIP / RUNTIME ────────────────────────────┐",
        label
    );
    info!(
        "[{}]   Model         : {} (firmware chip-id = {})",
        label, model, chip_id
    );
    info!(
        "[{}]   IDF version   : {}  (v{}.{}.{} compiled)",
        label,
        idf_ver,
        sys::ESP_IDF_VERSION_MAJOR,
        sys::ESP_IDF_VERSION_MINOR,
        sys::ESP_IDF_VERSION_PATCH
    );
    info!("[{}]   Uptime        : {} ms", label, uptime_ms);
    info!(
        "[{}]   Reset reason  : {} (raw={})",
        label, reset, reset_raw
    );
    info!("[{}]   eFuse pkg ver : {}", label, pkg_ver);
    info!(
        "[{}]   PSRAM size    : {} bytes ({} KiB)  [0 = no PSRAM]",
        label,
        psram_size,
        psram_size / 1024
    );
    info!("[{}]   Free internal : {}", label, kib(free_int));
    info!(
        "[{}] └───────────────────────────────────────────────────────────────────────┘",
        label
    );
}

// ─── 2. static memory map ────────────────────────────────────────────────────

struct Rgn {
    name: &'static str,
    start: u32,
    end: u32, // inclusive last byte (0 = N/A)
    perms: &'static str,
    note: &'static str,
}

// ESP32-C6 Technical Reference Manual §2  "System Memory Map".
// Addresses are physical HP-CPU bus addresses.
const REGIONS: &[Rgn] = &[
    Rgn {
        name: "ROM (boot mask-ROM)",
        start: 0x4000_0000,
        end: 0x4001_FFFF,
        perms: "r-x",
        note: "128 KiB, read-only boot ROM",
    },
    Rgn {
        name: "IRAM (HP_SRAM, rwx)",
        start: 0x4080_0000,
        end: 0x4086_BFFF,
        perms: "rwx",
        note: "432 KiB, aliased with DRAM window",
    },
    Rgn {
        name: "DRAM (HP_SRAM alias)",
        start: 0x4086_C000,
        end: 0x408F_FFFF,
        perms: "rw-",
        note: "400 KiB, same silicon as IRAM",
    },
    Rgn {
        name: "LP_SRAM (RTC fast mem)",
        start: 0x5000_0000,
        end: 0x5000_3FFF,
        perms: "rw-",
        note: "16 KiB, survives deep-sleep",
    },
    Rgn {
        name: "Flash XIP (code/rodata)",
        start: 0x4200_0000,
        end: 0x43FF_FFFF,
        perms: "r-x",
        note: "32 MiB virtual window (≤ 8 MiB chip)",
    },
    Rgn {
        name: "Peripheral APB/AHB MMIO",
        start: 0x6000_0000,
        end: 0x6100_0000,
        perms: "rw-",
        note: "Memory-mapped registers",
    },
    Rgn {
        name: "External SPI Flash (raw)",
        start: 0x0000_0000,
        end: 0x0000_0000,
        perms: "---",
        note: "Accessed via SPI driver, not mapped",
    },
];

fn print_memory_map(label: &str) {
    info!(
        "[{}] ┌─────────────────────────── STATIC MEMORY MAP ─────────────────────────┐",
        label
    );
    info!(
        "[{}]   {:<30}  {:>10}  {:>10}  {:>6}  {:>7}  Notes",
        label, "Region", "Start", "End", "Perms", "Size"
    );
    info!("[{}]   {}", label, "─".repeat(80));
    for r in REGIONS {
        let size_kib = if r.start == 0 && r.end == 0 {
            0
        } else {
            ((r.end as usize + 1).saturating_sub(r.start as usize)) / 1024
        };
        info!(
            "[{}]   {:<30}  0x{:08X}  0x{:08X}  {:>6}  {:>5} KiB  {}",
            label, r.name, r.start, r.end, r.perms, size_kib, r.note
        );
    }
    info!(
        "[{}] └───────────────────────────────────────────────────────────────────────┘",
        label
    );
}

// ─── 3. linker segments ──────────────────────────────────────────────────────
//
// The ESP-IDF RISC-V linker scripts export section boundary symbols.
// We take their addresses only — we never load data through them.

extern "C" {
    static _bss_start: u8;
    static _bss_end: u8;
    static _data_start: u8;
    static _data_end: u8;
    static _text_start: u8;
    static _text_end: u8;
    static _rodata_start: u8;
    static _rodata_end: u8;
    static _heap_start: u8;
    static _heap_end: u8;
}

fn print_linker_segments(label: &str) {
    // SAFETY: addr_of! takes the address of linker-defined section boundary
    // symbols — it never dereferences them as data pointers.
    macro_rules! sym_addr {
        ($s:ident) => {
            unsafe { core::ptr::addr_of!($s) as usize }
        };
    }
    let bss_s = sym_addr!(_bss_start);
    let bss_e = sym_addr!(_bss_end);
    let data_s = sym_addr!(_data_start);
    let data_e = sym_addr!(_data_end);
    let text_s = sym_addr!(_text_start);
    let text_e = sym_addr!(_text_end);
    let rodata_s = sym_addr!(_rodata_start);
    let rodata_e = sym_addr!(_rodata_end);
    let heap_s = sym_addr!(_heap_start);
    let heap_e = sym_addr!(_heap_end);

    let bss_sz = bss_e.saturating_sub(bss_s);
    let data_sz = data_e.saturating_sub(data_s);
    let text_sz = text_e.saturating_sub(text_s);
    let rodata_sz = rodata_e.saturating_sub(rodata_s);
    let heap_sz = heap_e.saturating_sub(heap_s);

    info!(
        "[{}] ┌─────────────────────────── LINKER SEGMENTS ───────────────────────────┐",
        label
    );
    info!(
        "[{}]   {:<10}  {:>10}  {:>10}  {:>9}  Flags  Description",
        label, "Segment", "Start", "End", "Size"
    );
    info!("[{}]   {}", label, "─".repeat(72));
    info!(
        "[{}]   {:<10}  0x{:08X}  0x{:08X}  {}  r-x    code in flash (XIP executed)",
        label,
        ".text",
        text_s,
        text_e,
        kib(text_sz)
    );
    info!(
        "[{}]   {:<10}  0x{:08X}  0x{:08X}  {}  r--    read-only data in flash",
        label,
        ".rodata",
        rodata_s,
        rodata_e,
        kib(rodata_sz)
    );
    info!("[{}]   {:<10}  0x{:08X}  0x{:08X}  {}  rw-    initialised globals (SRAM, copied from flash at boot)",
        label, ".data", data_s, data_e, kib(data_sz));
    info!("[{}]   {:<10}  0x{:08X}  0x{:08X}  {}  rw-    zero-initialised globals (SRAM, zeroed at boot)",
        label, ".bss", bss_s, bss_e, kib(bss_sz));
    info!(
        "[{}]   {:<10}  0x{:08X}  0x{:08X}  {}  rw-    runtime heap (grows upward)",
        label,
        "heap",
        heap_s,
        heap_e,
        kib(heap_sz)
    );
    info!("[{}]   {}", label, "─".repeat(72));
    info!(
        "[{}]   {:<10}  {:>10}  {:>10}  {}       SRAM static footprint (.data + .bss)",
        label,
        "STATIC",
        "",
        "",
        kib(data_sz + bss_sz)
    );
    info!(
        "[{}]   {:<10}  {:>10}  {:>10}  {}       Flash footprint (.text + .rodata)",
        label,
        "FLASH",
        "",
        "",
        kib(text_sz + rodata_sz)
    );
    info!(
        "[{}] └───────────────────────────────────────────────────────────────────────┘",
        label
    );
}

// ─── 4. heap capability statistics ───────────────────────────────────────────

struct Cap {
    name: &'static str,
    short: &'static str,
    cap: u32,
}

const CAPS: &[Cap] = &[
    Cap {
        name: "INTERNAL (on-chip SRAM)",
        short: "INTERNAL",
        cap: sys::MALLOC_CAP_INTERNAL,
    },
    Cap {
        name: "8BIT  (byte-addressable)",
        short: "8BIT",
        cap: sys::MALLOC_CAP_8BIT,
    },
    Cap {
        name: "32BIT (word-aligned / IRAM)",
        short: "32BIT",
        cap: sys::MALLOC_CAP_32BIT,
    },
    Cap {
        name: "DMA-capable",
        short: "DMA",
        cap: sys::MALLOC_CAP_DMA,
    },
    Cap {
        name: "EXEC  (IRAM, executable)",
        short: "EXEC",
        cap: sys::MALLOC_CAP_EXEC,
    },
    Cap {
        name: "SPIRAM (external PSRAM)",
        short: "SPIRAM",
        cap: sys::MALLOC_CAP_SPIRAM,
    },
    Cap {
        name: "DEFAULT (general purpose)",
        short: "DEFAULT",
        cap: sys::MALLOC_CAP_DEFAULT,
    },
];

fn print_heap(label: &str) -> Vec<String> {
    let mut anomalies: Vec<String> = Vec::new();

    info!(
        "[{}] ┌─────────────────────────── HEAP CAPABILITY STATISTICS ────────────────┐",
        label
    );
    info!(
        "[{}]   {:<36}  {:>6}  {:>6}  {:>6}  {:>6}  {:>5}  {:>5}  {}",
        label, "Capability", "Total", "Used", "Free", "LargeB", "Blks", "Frag%", "Usage"
    );
    info!(
        "[{}]   {:<36}  {:>6}  {:>6}  {:>6}  {:>6}  {:>5}  {:>5}  {}",
        label, "", "KiB", "KiB", "KiB", "KiB", "", "", "[20 chars]"
    );
    info!("[{}]   {}", label, "─".repeat(105));

    for c in CAPS {
        let mut s: sys::multi_heap_info_t = unsafe { core::mem::zeroed() };
        unsafe { sys::heap_caps_get_info(&mut s, c.cap) };

        let total = (s.total_free_bytes + s.total_allocated_bytes) as usize;
        let free = s.total_free_bytes as usize;
        let used = s.total_allocated_bytes as usize;
        let largest = s.largest_free_block as usize;
        let min_ev = s.minimum_free_bytes as usize;
        let f_blks = s.free_blocks as usize;
        let a_blks = s.allocated_blocks as usize;
        let t_blks = s.total_blocks as usize;

        let frag_pct = if free == 0 {
            0u32
        } else {
            100 - ((largest * 100 / free) as u32).min(100)
        };

        info!(
            "[{}]   {:<36}  {:>6}  {:>6}  {:>6}  {:>6}  {:>5}  {:>4}%  {}",
            label,
            c.name,
            total / 1024,
            used / 1024,
            free / 1024,
            largest / 1024,
            t_blks,
            frag_pct,
            bar(used, total)
        );
        info!(
            "[{}]     {:>36}  low-water {:>4} KiB  alloc-blks {:>4}  free-blks {:>4}",
            label,
            "",
            min_ev / 1024,
            a_blks,
            f_blks
        );

        // Anomaly detection
        if frag_pct >= 50 {
            anomalies.push(format!(
                "FRAG HIGH  : {} — {}% fragmented  (free {} KiB, largest block {} KiB)",
                c.short,
                frag_pct,
                free / 1024,
                largest / 1024
            ));
        } else if frag_pct >= 30 {
            anomalies.push(format!(
                "FRAG MOD   : {} — {}% fragmented",
                c.short, frag_pct
            ));
        }
        if free < 8 * 1024 && total > 0 {
            anomalies.push(format!(
                "LOW HEAP   : {} — only {} KiB remaining",
                c.short,
                free / 1024
            ));
        }
        if largest < 4 * 1024 && free > 16 * 1024 {
            anomalies.push(format!(
                "TINY BLOCK : {} — largest free block {} KiB despite {} KiB total free \
                 (severe fragmentation)",
                c.short,
                largest / 1024,
                free / 1024
            ));
        }
    }

    // Global summary
    let total_free = unsafe { sys::esp_get_free_heap_size() } as usize;
    let min_ever = unsafe { sys::esp_get_minimum_free_heap_size() } as usize;
    let largest_any =
        unsafe { sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_8BIT) } as usize;
    info!("[{}]   {}", label, "─".repeat(105));
    info!(
        "[{}]   TOTAL free (default caps) : {}  lifetime-min : {}  largest-block : {}",
        label,
        kib(total_free),
        kib(min_ever),
        kib(largest_any)
    );
    info!(
        "[{}] └───────────────────────────────────────────────────────────────────────┘",
        label
    );

    anomalies
}

// ─── 5. task stack high-water mark ───────────────────────────────────────────
//
// `uxTaskGetStackHighWaterMark(NULL)` returns the minimum free stack (in
// *words* on most ports; verify against `portSTACK_TYPE`). On IDF RISC-V
// the stack grows downward and the value is in 4-byte words.

fn print_stack(label: &str) -> Option<String> {
    // SAFETY: NULL = current task; return value is a plain integer.
    let hwm_words = unsafe { sys::uxTaskGetStackHighWaterMark(core::ptr::null_mut()) } as usize;
    // Each word is 4 bytes on 32-bit RISC-V.
    let hwm_bytes = hwm_words * 4;

    let status = if hwm_bytes < 512 {
        "!! CRITICAL"
    } else if hwm_bytes < 1024 {
        "!  WARNING  "
    } else {
        "   OK       "
    };

    info!(
        "[{}] ┌─────────────────────────── TASK STACK ────────────────────────────────┐",
        label
    );
    info!(
        "[{}]   Main task stack high-water mark : {} words = {} bytes ({} KiB)  {}",
        label,
        hwm_words,
        hwm_bytes,
        hwm_bytes / 1024,
        status
    );
    info!(
        "[{}]   (high-water mark = minimum remaining stack since task started)",
        label
    );
    info!(
        "[{}] └───────────────────────────────────────────────────────────────────────┘",
        label
    );

    if hwm_bytes < 1024 {
        Some(format!(
            "STACK LOW  : main task — {} bytes remaining (< 1 KiB)",
            hwm_bytes
        ))
    } else {
        None
    }
}

// ─── 6. anomaly scan ─────────────────────────────────────────────────────────

fn print_anomalies(label: &str, anomalies: &[String]) {
    info!(
        "[{}] ┌─────────────────────────── ANOMALY SCAN ──────────────────────────────┐",
        label
    );
    if anomalies.is_empty() {
        info!("[{}]   No anomalies detected.", label);
    } else {
        info!("[{}]   {} issue(s) detected:", label, anomalies.len());
        for a in anomalies {
            warn!("[{}]   *** {}", label, a);
        }
    }
    info!(
        "[{}] └───────────────────────────────────────────────────────────────────────┘",
        label
    );
}

// ─── public API ──────────────────────────────────────────────────────────────

/// Print a full, structured memory diagnostics report labelled with `label`.
///
/// Output goes to the ESP-IDF INFO log (anomalies at WARN), so it appears on
/// the serial console regardless of the per-module log filter. Intended to be
/// called at key startup checkpoints *before* the main execution loop.
pub fn report(label: &str) {
    let sep = "═".repeat(74);
    info!("");
    info!("{}", sep);
    info!("  MEMORY DIAGNOSTICS  ─  checkpoint: \"{}\"", label);
    info!("{}", sep);

    print_chip(label);
    print_memory_map(label);
    print_linker_segments(label);
    let mut anomalies = print_heap(label);
    if let Some(stack_warn) = print_stack(label) {
        anomalies.push(stack_warn);
    }
    print_anomalies(label, &anomalies);

    info!("{}", sep);
    info!("");
}

/// Returns the largest contiguous free block from the default 8-bit heap.
/// Use as a precondition guard before any large allocation to avoid OOM panics.
pub fn largest_free() -> usize {
    unsafe { sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_8BIT) as usize }
}

/// Dumps the raw per-block heap layout via the IDF's own diagnostic printer.
/// Very verbose — one line per block — but shows exact addresses and sizes.
pub fn dump_full() {
    info!("═══ full heap_caps dump (MALLOC_CAP_8BIT) ═══");
    unsafe { sys::heap_caps_print_heap_info(sys::MALLOC_CAP_8BIT) };
    info!("═══ end full dump ═══");
}
