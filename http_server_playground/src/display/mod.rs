// Display module — ST7735S 80x160 SPI display.
//
// Owns the SPI hardware and exposes it to two submodules via a shared Mutex:
//   - basic:    text/clear operations triggered by HTTP requests.
//   - renderer: software 3-D rasteriser on a background thread.
//
// A mode flag (basic | renderer) determines which submodule controls the screen.
// Switching mode is done via GET /api/display/mode?set=basic|renderer.
// Submodule HTML fragments are injected into the outer card at startup.

pub mod basic;
pub mod renderer;

use embedded_hal::digital::OutputPin;
use embedded_hal::spi::SpiDevice;
use esp_idf_svc::hal::delay::FreeRtos;
use esp_idf_svc::hal::gpio::{AnyIOPin, Output, OutputPin as EspOutputPin, PinDriver};
use esp_idf_svc::hal::spi::config::{Config as SpiConfig, DriverConfig as SpiDriverConfig};
use esp_idf_svc::hal::spi::{Dma, SpiAnyPins, SpiDeviceDriver};
use esp_idf_svc::hal::units::FromValueType;
use esp_idf_svc::http::server::EspHttpServer;
use log::info;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

const CARD_HTML: &str = include_str!("card.html");

// ─── display geometry (shared with submodules) ────────────────────────────────
pub(crate) const W: usize = 160;
pub(crate) const H: usize = 80;
pub(crate) const PIXELS: usize = W * H;
pub(crate) const FB_BYTES: usize = PIXELS * 2; // big-endian RGB565: 2 bytes/pixel
pub(crate) const DISPLAY_Y_OFFSET: u16 = 24;   // physical panel Y address offset

// ─── SPI config ──────────────────────────────────────────────────────────────
const SPI_FREQ_MHZ: u32 = 40;
const SPI_DMA_BUF_BYTES: usize = FB_BYTES.next_power_of_two();

// ─── mode constants ───────────────────────────────────────────────────────────
pub(crate) const MODE_BASIC: u8    = 0;
pub(crate) const MODE_RENDERER: u8 = 1;

pub(crate) type SharedMode = Arc<AtomicU8>;

// ─── DisplayOps trait ─────────────────────────────────────────────────────────
// Trait-object interface shared between the basic and renderer submodules.
// Using dyn dispatch avoids having to name the concrete generic SPI/DC types
// across module boundaries.
pub(crate) trait DisplayOps: Send {
    fn set_pixel(&mut self, x: u16, y: u16, color: u16);
    fn fill_solid(&mut self, color: u16);
    fn flush_frame(&mut self, fb: &[u8; FB_BYTES]);
    fn flush_frame_timed(&mut self, fb: &[u8; FB_BYTES]) -> (u64, u64);
    fn set_full_frame_window(&mut self);
}

pub(crate) type SharedDisplay = Arc<Mutex<Box<dyn DisplayOps>>>;

// ─── raw ST7735S driver ───────────────────────────────────────────────────────

struct RawST7735<S, DC> {
    spi: S,
    dc:  DC,
}

impl<S: SpiDevice, DC: OutputPin> RawST7735<S, DC> {
    fn cmd(&mut self, cmd: u8) {
        let _ = self.dc.set_low();
        let _ = self.spi.write(&[cmd]);
    }
    fn data(&mut self, data: &[u8]) {
        let _ = self.dc.set_high();
        let _ = self.spi.write(data);
    }
    fn cmd_data(&mut self, cmd: u8, data: &[u8]) {
        self.cmd(cmd);
        self.data(data);
    }
    fn init(&mut self) {
        self.cmd(0x01); FreeRtos::delay_ms(200);  // SWRESET
        self.cmd(0x11); FreeRtos::delay_ms(200);  // SLPOUT
        self.cmd_data(0xB1, &[0x01, 0x2C, 0x2D]); // FRMCTR1
        self.cmd_data(0xB2, &[0x01, 0x2C, 0x2D]); // FRMCTR2
        self.cmd_data(0xB3, &[0x01, 0x2C, 0x2D, 0x01, 0x2C, 0x2D]); // FRMCTR3
        self.cmd_data(0xB4, &[0x07]);              // INVCTR
        self.cmd_data(0xC0, &[0xA2, 0x02, 0x84]); // PWCTR1
        self.cmd_data(0xC1, &[0xC5]);             // PWCTR2
        self.cmd_data(0xC2, &[0x0A, 0x00]);       // PWCTR3
        self.cmd_data(0xC3, &[0x8A, 0x2A]);       // PWCTR4
        self.cmd_data(0xC4, &[0x8A, 0xEE]);       // PWCTR5
        self.cmd_data(0xC5, &[0x0E]);             // VMCTR1
        self.cmd(0x20);                           // INVOFF
        self.cmd_data(0x3A, &[0x05]);             // COLMOD: RGB565
        self.cmd_data(0x36, &[0x68]);             // MADCTL: landscape + BGR
        self.cmd(0x29); FreeRtos::delay_ms(200);  // DISPON
    }
}

impl<S: SpiDevice + Send, DC: OutputPin + Send> DisplayOps for RawST7735<S, DC> {
    fn set_full_frame_window(&mut self) {
        const EX: u16 = (W - 1) as u16;
        const EY: u16 = (H - 1) as u16 + DISPLAY_Y_OFFSET;
        self.cmd_data(0x2A, &[0, 0, 0, EX as u8]);
        self.cmd_data(0x2B, &[0, DISPLAY_Y_OFFSET as u8, 0, EY as u8]);
    }

    /// Write a single pixel via CASET/RASET/RAMWR.
    fn set_pixel(&mut self, x: u16, y: u16, color: u16) {
        let py = y + DISPLAY_Y_OFFSET;
        self.cmd_data(0x2A, &[0, x as u8, 0, x as u8]);
        self.cmd_data(0x2B, &[0, py as u8, 0, py as u8]);
        let [hi, lo] = color.to_be_bytes();
        self.cmd_data(0x2C, &[hi, lo]);
    }

    /// Fill the entire screen with one colour — one scanline buffer, H writes.
    fn fill_solid(&mut self, color: u16) {
        self.set_full_frame_window();
        let [hi, lo] = color.to_be_bytes();
        let mut row = [0u8; W * 2];
        for i in 0..W { row[i*2] = hi; row[i*2+1] = lo; }
        self.cmd(0x2C);
        let _ = self.dc.set_high();
        for _ in 0..H { let _ = self.spi.write(&row); }
    }

    fn flush_frame(&mut self, fb: &[u8; FB_BYTES]) {
        self.cmd(0x2C);
        let _ = self.dc.set_high();
        let _ = self.spi.write(fb);
    }

    fn flush_frame_timed(&mut self, fb: &[u8; FB_BYTES]) -> (u64, u64) {
        let t0 = std::time::Instant::now();
        let _ = self.dc.set_low();
        let _ = self.spi.write(&[0x2C]);
        let cmd_us = t0.elapsed().as_micros() as u64;
        let t1 = std::time::Instant::now();
        let _ = self.dc.set_high();
        let _ = self.spi.write(fb);
        (cmd_us, t1.elapsed().as_micros() as u64)
    }
}

// ─── public entry point ───────────────────────────────────────────────────────

/// Initialises the ST7735S, hands the shared driver to both submodules, registers
/// the mode-switch endpoint, and returns the assembled HTML card.
pub fn register<SPI>(
    server: &mut EspHttpServer,
    spi: SPI,
    sclk: impl EspOutputPin + 'static,
    mosi: impl EspOutputPin + 'static,
    cs: AnyIOPin<'static>,
    dc: AnyIOPin<'static>,
    rst: AnyIOPin<'static>,
    bl: AnyIOPin<'static>,
) -> anyhow::Result<String>
where
    SPI: SpiAnyPins + 'static,
{
    info!("display: configuring BL pin");
    let mut bl_drv = PinDriver::output(bl)?;
    bl_drv.set_high()?;
    core::mem::forget(bl_drv);

    info!("display: hardware reset");
    let mut rst_drv = PinDriver::output(rst)?;
    rst_drv.set_high()?; FreeRtos::delay_ms(20);
    rst_drv.set_low()?;  FreeRtos::delay_ms(20);
    rst_drv.set_high()?; FreeRtos::delay_ms(150);
    core::mem::forget(rst_drv);

    info!("display: starting SPI");
    let spi_dev = SpiDeviceDriver::new_single(
        spi, sclk, mosi,
        Option::<AnyIOPin>::None,
        Some(cs),
        &SpiDriverConfig::new().dma(Dma::Auto(SPI_DMA_BUF_BYTES)),
        // polling(false) = interrupt-driven: thread sleeps during 5ms DMA flush,
        // allowing FreeRTOS IDLE to run and reset the Task Watchdog Timer.
        &SpiConfig::new().baudrate(SPI_FREQ_MHZ.MHz().into()).polling(false),
    )?;
    let dc_drv = PinDriver::output(dc)?;

    info!("display: initialising panel");
    let mut raw = RawST7735 { spi: spi_dev, dc: dc_drv };
    raw.init();
    raw.set_full_frame_window();
    raw.fill_solid(0x0000); // clear to black

    let shared: SharedDisplay = Arc::new(Mutex::new(Box::new(raw)));
    let mode: SharedMode = Arc::new(AtomicU8::new(MODE_BASIC));

    basic::register(server, shared.clone(), mode.clone())?;
    renderer::register(server, shared.clone(), mode.clone())?;

    // Mode switch: GET /api/display/mode?set=basic|renderer
    {
        let mode = mode.clone();
        server.fn_handler(
            "/api/display/mode",
            esp_idf_svc::http::Method::Get,
            move |req| {
                let uri = req.uri().to_string();
                let set = uri.split('?').nth(1)
                    .and_then(|q| q.split('&').find(|p| p.starts_with("set=")))
                    .and_then(|p| p.strip_prefix("set="))
                    .unwrap_or("basic");
                let new_mode = if set == "renderer" { MODE_RENDERER } else { MODE_BASIC };
                mode.store(new_mode, Ordering::Relaxed);
                let json: &[u8] = if new_mode == MODE_RENDERER {
                    br#"{"mode":"renderer"}"#
                } else {
                    br#"{"mode":"basic"}"#
                };
                let mut resp = req.into_response(
                    200, Some("OK"), &[("Content-Type", "application/json")],
                )?;
                resp.write(json)?;
                Ok::<(), anyhow::Error>(())
            },
        )?;
    }

    info!("display module ready");

    // Inject submodule HTML fragments at compile time.
    let card = CARD_HTML
        .replace("{{BASIC_CARD}}", basic::CARD_HTML)
        .replace("{{RENDERER_CARD}}", renderer::CARD_HTML);
    Ok(card)
}
