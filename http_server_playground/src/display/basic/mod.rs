// Basic display submodule — text and solid-fill operations triggered by HTTP requests.
// Draws into the shared raw display driver via embedded-graphics. The mutex is held
// for the full duration of each draw call (a few ms at most), which is fine since
// the renderer thread is sleeping when the mode is set to Basic.

use embedded_graphics::mono_font::{ascii::FONT_10X20, MonoTextStyle};
use embedded_graphics::pixelcolor::{Rgb565, Rgb888};
use embedded_graphics::prelude::*;
use embedded_graphics::text::{Baseline, Text};
use esp_idf_svc::http::server::EspHttpServer;
use log::info;

use super::{DisplayOps, SharedDisplay, SharedMode, H, W};

pub(super) const CARD_HTML: &str = include_str!("card.html");

// ─── helpers ─────────────────────────────────────────────────────────────────

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

fn query_param<'a>(uri: &'a str, name: &str) -> Option<&'a str> {
    let query = uri.split_once('?')?.1;
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            if key == name {
                return Some(value);
            }
        }
    }
    None
}

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

// ─── DrawTarget wrapper ───────────────────────────────────────────────────────
// Lets embedded-graphics draw into the shared hardware.
// The lock is held for the entire draw_iter call to avoid per-pixel lock overhead.

struct BasicDisplay<'a>(std::sync::MutexGuard<'a, Box<dyn DisplayOps>>);

impl DrawTarget for BasicDisplay<'_> {
    type Color = Rgb565;
    type Error = ();

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), ()>
    where
        I: IntoIterator<Item = Pixel<Rgb565>>,
    {
        for Pixel(point, color) in pixels {
            if point.x >= 0 && (point.x as usize) < W && point.y >= 0 && (point.y as usize) < H {
                self.0
                    .set_pixel(point.x as u16, point.y as u16, color.into_storage());
            }
        }
        Ok(())
    }
}

impl OriginDimensions for BasicDisplay<'_> {
    fn size(&self) -> Size {
        Size::new(W as u32, H as u32)
    }
}

// ─── register ────────────────────────────────────────────────────────────────

pub(super) fn register(
    server: &mut EspHttpServer,
    display: SharedDisplay,
    _mode: SharedMode,
) -> anyhow::Result<()> {
    {
        let display = display.clone();
        server.fn_handler(
            "/api/display/text",
            esp_idf_svc::http::Method::Get,
            move |req| {
                let uri = req.uri().to_string();
                let msg = query_param(&uri, "msg")
                    .map(percent_decode)
                    .unwrap_or_else(|| "Hello".to_string());
                let color = query_param(&uri, "color")
                    .map(|s| parse_hex_color(s))
                    .unwrap_or(Rgb565::WHITE);

                // Clear to black, then draw text centerd vertically.
                let mut raw = display.lock().unwrap();
                raw.fill_solid(0x0000);
                let mut target = BasicDisplay(raw);
                let style = MonoTextStyle::new(&FONT_10X20, color);
                Text::with_baseline(&msg, Point::new(4, 36), style, Baseline::Middle)
                    .draw(&mut target)
                    .ok();

                let mut resp =
                    req.into_response(200, Some("OK"), &[("Content-Type", "application/json")])?;
                resp.write(br#"{"ok":true}"#)?;
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
                let uri = req.uri().to_string();
                let color = query_param(&uri, "color")
                    .map(|s| parse_hex_color(s))
                    .unwrap_or(Rgb565::BLACK);
                display.lock().unwrap().fill_solid(color.into_storage());

                let mut resp =
                    req.into_response(200, Some("OK"), &[("Content-Type", "application/json")])?;
                resp.write(br#"{"ok":true}"#)?;
                Ok::<(), anyhow::Error>(())
            },
        )?;
    }

    info!("display/basic: endpoints registered");
    Ok(())
}
