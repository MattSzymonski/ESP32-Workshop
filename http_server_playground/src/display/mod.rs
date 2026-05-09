// This file implements the ST7735S SPI display module for the HTTP server.
// - Initialises a 128x160 ST7735S display over SPI2 with hardware reset and backlight control.
// - Registers GET /api/display/text?msg=<text>&color=<hex> to render a text message on screen.
// - Registers GET /api/display/clear?color=<hex> to fill the screen with a solid color.
// - Provides URL helpers: percent_decode for encoded text, parse_hex_color for CSS hex colors,
//   and query_param for extracting named values from a request URI.
// - Depends on: embedded-graphics, st7735-lcd, esp-idf-svc (SPI2, GPIO, HTTP server).

use embedded_graphics::mono_font::{ascii::FONT_10X20, MonoTextStyle};
use embedded_graphics::pixelcolor::{Rgb565, Rgb888};
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};
use embedded_graphics::text::{Baseline, Text};
use esp_idf_svc::hal::delay::{Ets, FreeRtos};
use esp_idf_svc::hal::gpio::{AnyIOPin, OutputPin, PinDriver};
use esp_idf_svc::hal::spi::config::{Config as SpiConfig, DriverConfig as SpiDriverConfig};
use esp_idf_svc::hal::spi::{SpiAnyPins, SpiDeviceDriver};
use esp_idf_svc::hal::units::FromValueType;
use esp_idf_svc::http::server::EspHttpServer;
use log::info;
use st7735_lcd::{Orientation, ST7735};
use std::sync::{Arc, Mutex};

const CARD_HTML: &str = include_str!("card.html");

const PANEL_WIDTH: u16 = 160;
const PANEL_HEIGHT: u16 = 80;
const OFFSET_X: u16 = 0;
const OFFSET_Y: u16 = 24;

// Parses a 6-digit hex color string (with or without '#' prefix) into an Rgb565 value.
// Returns white on any parse failure (invalid length or non-hex characters).
fn parse_hex_color(hex: &str) -> Rgb565 {
    let hex = hex.trim_start_matches('#');

    if hex.len() != 6 {
        return Rgb565::WHITE;
    }

    let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(255);
    let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(255);
    let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(255);

    Rgb888::new(r, g, b).into()
}

// Extracts the value of a named query parameter from a request URI.
// Returns None if the query string is absent or the parameter is not found.
fn query_param<'a>(uri: &'a str, name: &str) -> Option<&'a str> {
    let query = uri.split_once('?')?.1;

    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        if key == name {
            return Some(value);
        }
    }

    None
}

// Decodes a percent-encoded URL string (e.g. "%20" → " ") into a plain UTF-8 String.
// Non-encoded bytes and malformed sequences are passed through as-is.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::new();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = core::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(value) = u8::from_str_radix(hex, 16) {
                    out.push(value as char);
                    i += 3;
                    continue;
                }
            }
        }

        out.push(bytes[i] as char);
        i += 1;
    }

    out
}

/// Initialises the ST7735S display over SPI and registers two HTTP endpoints:
/// - `GET /api/display/text?msg=<text>&color=<hex>` — clears the screen and renders a text message.
/// - `GET /api/display/clear?color=<hex>` — fills the screen with the given color (default black).
/// Returns the HTML card string to be embedded in the main page.
pub fn register<SPI>(
    server: &mut EspHttpServer,
    spi: SPI,
    sclk: impl OutputPin + 'static,
    mosi: impl OutputPin + 'static,
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

    info!("display: configuring DC and RST pins");
    let dc_drv = PinDriver::output(dc)?;
    let mut rst_drv = PinDriver::output(rst)?;

    info!("display: hardware reset");
    rst_drv.set_high()?;
    FreeRtos::delay_ms(20);
    rst_drv.set_low()?;
    FreeRtos::delay_ms(20);
    rst_drv.set_high()?;
    FreeRtos::delay_ms(150);

    info!("display: starting SPI");
    let spi_dev = SpiDeviceDriver::new_single(
        spi,
        sclk,
        mosi,
        Option::<AnyIOPin>::None,
        Some(cs),
        &SpiDriverConfig::new(),
        &SpiConfig::new().baudrate(4.MHz().into()),
    )?;

    info!("display: constructing ST7735");
    let mut display = ST7735::new(
        spi_dev,
        dc_drv,
        rst_drv,
        false,
        false,
        PANEL_WIDTH as u32,
        PANEL_HEIGHT as u32,
    );

    let mut delay = Ets;

    display
        .init(&mut delay)
        .map_err(|_| anyhow::anyhow!("ST7735S init failed"))?;

    display
        .set_orientation(&Orientation::Landscape)
        .map_err(|_| anyhow::anyhow!("ST7735S set_orientation failed"))?;

    display.set_offset(OFFSET_X, OFFSET_Y);
    display
        .clear(Rgb565::BLACK)
        .map_err(|_| anyhow::anyhow!("display clear failed"))?;

    let display = Arc::new(Mutex::new(display));

    {
        let display = display.clone();

        server.fn_handler(
            "/api/display/text",
            esp_idf_svc::http::Method::Get,
            move |req| {
                // Parse `msg` and `color` query params, clear the display, and draw the
                // requested text centered vertically using FONT_10X20.
                let uri = req.uri().to_string();

                let msg = query_param(&uri, "msg")
                    .map(percent_decode)
                    .unwrap_or_else(|| "Hello".to_string());

                let color = query_param(&uri, "color")
                    .map(parse_hex_color)
                    .unwrap_or(Rgb565::WHITE);

                let mut display = display.lock().unwrap();

                display
                    .clear(Rgb565::BLACK)
                    .map_err(|_| anyhow::anyhow!("display clear failed"))?;

                let style = MonoTextStyle::new(&FONT_10X20, color);

                Text::with_baseline(&msg, Point::new(4, 36), style, Baseline::Middle)
                    .draw(&mut *display)
                    .map_err(|_| anyhow::anyhow!("display text draw failed"))?;

                let mut response =
                    req.into_response(200, Some("OK"), &[("Content-Type", "application/json")])?;
                response.write(br#"{"ok":true}"#)?;

                Ok::<(), anyhow::Error>(())
            },
        )?;
    }

    {
        let display = display.clone();

        server.fn_handler(
            "/api/display/clear",
            esp_idf_svc::http::Method::Get,
            move |req| {
                // Parse optional `color` query param (defaults to black) and fill
                // the entire display with that color.
                let uri = req.uri().to_string();

                let color = query_param(&uri, "color")
                    .map(parse_hex_color)
                    .unwrap_or(Rgb565::BLACK);

                let mut display = display.lock().unwrap();

                display
                    .clear(color)
                    .map_err(|_| anyhow::anyhow!("display clear failed"))?;

                let mut response =
                    req.into_response(200, Some("OK"), &[("Content-Type", "application/json")])?;
                response.write(br#"{"ok":true}"#)?;

                Ok::<(), anyhow::Error>(())
            },
        )?;
    }

    info!("ST7735S display initialised");

    Ok(CARD_HTML.to_string())
}
