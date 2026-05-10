// Software 3-D renderer — rotating cube on the ST7735S display.
//
// Architecture:
//   - All maths runs in f32 on the CPU (no FPU on ESP32-C6, emulated in SW).
//   - A preallocated [u16; W*H] framebuffer holds RGB565 pixels.
//   - Every frame: clear fb → transform cube vertices → project → rasterize edges
//     → flush the whole fb to the display via a single SPI write_frame call.
//   - The render loop runs on a dedicated FreeRTOS thread (std::thread::spawn)
//     so it never blocks the HTTP server.
//   - An Arc<AtomicBool> lets the HTTP endpoint start/stop the loop.
//
// Display geometry (landscape, matches display::mod.rs):
//   Width=160, Height=80, offset=(0,24) — those are physical ST7735S address offsets
//   and are already handled by the driver; here we just use W=160, H=80.

use esp_idf_svc::hal::delay::Ets;
use esp_idf_svc::hal::delay::FreeRtos;
use esp_idf_svc::hal::gpio::{AnyIOPin, OutputPin, PinDriver};
use esp_idf_svc::hal::spi::config::{Config as SpiConfig, DriverConfig as SpiDriverConfig};
use esp_idf_svc::hal::spi::{SpiAnyPins, SpiDeviceDriver};
use esp_idf_svc::hal::units::FromValueType;
use esp_idf_svc::http::server::EspHttpServer;
use log::info;
use st7735_lcd::{Orientation, ST7735};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const CARD_HTML: &str = include_str!("card.html");

// ─── display size ────────────────────────────────────────────────────────────
const W: usize = 160;
const H: usize = 80;
const PIXELS: usize = W * H;

// ─── colours (RGB565 big-endian) ─────────────────────────────────────────────
const BLACK: u16 = 0x0000;
const ORANGE: u16 = rgb565(255, 133, 89); // #FF8559 accent
const DIM: u16 = rgb565(80, 40, 20); // dimmer orange for back edges

const fn rgb565(r: u8, g: u8, b: u8) -> u16 {
    (((r as u16) & 0xF8) << 8) | (((g as u16) & 0xFC) << 3) | ((b as u16) >> 3)
}

// ─── 3-D maths ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct Vec3 {
    x: f32,
    y: f32,
    z: f32,
}

#[derive(Clone, Copy)]
struct Vec4 {
    x: f32,
    y: f32,
    z: f32,
    w: f32,
}

// Column-major 4×4 matrix stored as [col0, col1, col2, col3]
#[derive(Clone, Copy)]
struct Mat4([[f32; 4]; 4]);

impl Mat4 {
    #[allow(dead_code)]
    fn identity() -> Self {
        Mat4([
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ])
    }

    fn mul(&self, other: &Mat4) -> Mat4 {
        let a = &self.0;
        let b = &other.0;
        let mut c = [[0.0f32; 4]; 4];
        for col in 0..4 {
            for row in 0..4 {
                c[col][row] = a[0][row] * b[col][0]
                    + a[1][row] * b[col][1]
                    + a[2][row] * b[col][2]
                    + a[3][row] * b[col][3];
            }
        }
        Mat4(c)
    }

    fn mul_vec4(&self, v: Vec4) -> Vec4 {
        let m = &self.0;
        Vec4 {
            x: m[0][0] * v.x + m[1][0] * v.y + m[2][0] * v.z + m[3][0] * v.w,
            y: m[0][1] * v.x + m[1][1] * v.y + m[2][1] * v.z + m[3][1] * v.w,
            z: m[0][2] * v.x + m[1][2] * v.y + m[2][2] * v.z + m[3][2] * v.w,
            w: m[0][3] * v.x + m[1][3] * v.y + m[2][3] * v.z + m[3][3] * v.w,
        }
    }

    // Rotation around X axis
    fn rotation_x(angle: f32) -> Self {
        let (s, c) = (libm::sinf(angle), libm::cosf(angle));
        Mat4([
            [1.0, 0.0, 0.0, 0.0],
            [0.0, c, s, 0.0],
            [0.0, -s, c, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ])
    }

    // Rotation around Y axis
    fn rotation_y(angle: f32) -> Self {
        let (s, c) = (libm::sinf(angle), libm::cosf(angle));
        Mat4([
            [c, 0.0, -s, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [s, 0.0, c, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ])
    }

    // Rotation around Z axis
    fn rotation_z(angle: f32) -> Self {
        let (s, c) = (libm::sinf(angle), libm::cosf(angle));
        Mat4([
            [c, s, 0.0, 0.0],
            [-s, c, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ])
    }

    // Translation
    fn translation(tx: f32, ty: f32, tz: f32) -> Self {
        Mat4([
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [tx, ty, tz, 1.0],
        ])
    }

    // Symmetric perspective projection
    // fov_y_rad: vertical FOV in radians
    // aspect: width / height
    // near / far: clip planes
    fn perspective(fov_y_rad: f32, aspect: f32, near: f32, far: f32) -> Self {
        let f = 1.0 / libm::tanf(fov_y_rad * 0.5);
        let range_inv = 1.0 / (near - far);
        Mat4([
            [f / aspect, 0.0, 0.0, 0.0],
            [0.0, f, 0.0, 0.0],
            [0.0, 0.0, (near + far) * range_inv, -1.0],
            [0.0, 0.0, 2.0 * near * far * range_inv, 0.0],
        ])
    }
}

// ─── cube geometry ───────────────────────────────────────────────────────────

// 8 corners of a unit cube centred at the origin
const CUBE_VERTS: [Vec3; 8] = [
    Vec3 {
        x: -0.5,
        y: -0.5,
        z: -0.5,
    },
    Vec3 {
        x: 0.5,
        y: -0.5,
        z: -0.5,
    },
    Vec3 {
        x: 0.5,
        y: 0.5,
        z: -0.5,
    },
    Vec3 {
        x: -0.5,
        y: 0.5,
        z: -0.5,
    },
    Vec3 {
        x: -0.5,
        y: -0.5,
        z: 0.5,
    },
    Vec3 {
        x: 0.5,
        y: -0.5,
        z: 0.5,
    },
    Vec3 {
        x: 0.5,
        y: 0.5,
        z: 0.5,
    },
    Vec3 {
        x: -0.5,
        y: 0.5,
        z: 0.5,
    },
];

// 12 edges as pairs of vertex indices
const CUBE_EDGES: [(usize, usize); 12] = [
    (0, 1),
    (1, 2),
    (2, 3),
    (3, 0), // back face
    (4, 5),
    (5, 6),
    (6, 7),
    (7, 4), // front face
    (0, 4),
    (1, 5),
    (2, 6),
    (3, 7), // connecting edges
];

// ─── software rasterizer ─────────────────────────────────────────────────────

// Bresenham line into a flat RGB565 framebuffer
fn draw_line(fb: &mut [u16; PIXELS], mut x0: i32, mut y0: i32, x1: i32, y1: i32, color: u16) {
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx: i32 = if x0 < x1 { 1 } else { -1 };
    let sy: i32 = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;

    loop {
        if x0 >= 0 && x0 < W as i32 && y0 >= 0 && y0 < H as i32 {
            fb[y0 as usize * W + x0 as usize] = color;
        }
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

// ─── render one frame ─────────────────────────────────────────────────────────

fn render_frame(fb: &mut [u16; PIXELS], angle_x: f32, angle_y: f32, angle_z: f32) {
    // 1. Clear framebuffer
    fb.fill(BLACK);

    // 2. Build MVP matrix
    //    Model: rotate
    let model = Mat4::rotation_x(angle_x)
        .mul(&Mat4::rotation_y(angle_y))
        .mul(&Mat4::rotation_z(angle_z));

    //    View: push the cube 2.0 units into the screen
    let view = Mat4::translation(0.0, 0.0, -2.0);

    //    Projection: 60° FOV, landscape aspect ratio
    let proj = Mat4::perspective(
        core::f32::consts::PI / 3.0, // 60°
        W as f32 / H as f32,
        0.1,
        100.0,
    );

    let mvp = proj.mul(&view).mul(&model);

    // 3. Transform all 8 vertices through MVP → NDC → screen space
    let half_w = (W as f32) * 0.5;
    let half_h = (H as f32) * 0.5;

    let screen: [(i32, i32, f32); 8] = core::array::from_fn(|i| {
        let v = &CUBE_VERTS[i];
        let clip = mvp.mul_vec4(Vec4 {
            x: v.x,
            y: v.y,
            z: v.z,
            w: 1.0,
        });

        // Perspective divide → NDC
        let ndc_x = clip.x / clip.w;
        let ndc_y = clip.y / clip.w;

        // Map NDC [-1,1] to pixel coords
        let px = ((ndc_x + 1.0) * half_w) as i32;
        let py = ((-ndc_y + 1.0) * half_h) as i32;

        (px, py, clip.w)
    });

    // 4. Draw edges — front (z>0) in bright orange, back in dim orange
    for &(a, b) in &CUBE_EDGES {
        // Use average clip-space w to decide front/back
        let avg_w = (screen[a].2 + screen[b].2) * 0.5;
        let color = if avg_w > 0.0 { ORANGE } else { DIM };

        draw_line(
            fb,
            screen[a].0,
            screen[a].1,
            screen[b].0,
            screen[b].1,
            color,
        );
    }
}

// ─── public entry point ───────────────────────────────────────────────────────

/// Initialises the ST7735S on SPI2, starts a background render loop, and registers
/// HTTP endpoints to start/stop the renderer.
/// Returns the HTML card string to embed in the dashboard.
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
    info!("renderer: configuring BL pin");
    let mut bl_drv = PinDriver::output(bl)?;
    bl_drv.set_high()?;
    core::mem::forget(bl_drv);

    info!("renderer: configuring DC and RST pins");
    let dc_drv = PinDriver::output(dc)?;
    let mut rst_drv = PinDriver::output(rst)?;

    info!("renderer: hardware reset");
    rst_drv.set_high()?;
    FreeRtos::delay_ms(20);
    rst_drv.set_low()?;
    FreeRtos::delay_ms(20);
    rst_drv.set_high()?;
    FreeRtos::delay_ms(150);

    info!("renderer: starting SPI");
    let spi_dev = SpiDeviceDriver::new_single(
        spi,
        sclk,
        mosi,
        Option::<AnyIOPin>::None,
        Some(cs),
        &SpiDriverConfig::new(),
        &SpiConfig::new().baudrate(16.MHz().into()),
    )?;

    info!("renderer: constructing ST7735");
    let mut display = ST7735::new(spi_dev, dc_drv, rst_drv, false, false, W as u32, H as u32);

    let mut delay = Ets;
    display
        .init(&mut delay)
        .map_err(|_| anyhow::anyhow!("ST7735S init failed"))?;
    display
        .set_orientation(&Orientation::Landscape)
        .map_err(|_| anyhow::anyhow!("orientation failed"))?;
    display.set_offset(0, 24);

    info!("renderer: spawning render thread");

    // running flag: true = keep rendering, false = stop
    let running = Arc::new(AtomicBool::new(true));
    let running_thread = running.clone();
    let running_start = running.clone();
    let running_stop = running.clone();

    std::thread::Builder::new()
        .stack_size(8192)
        .spawn(move || {
            // Allocate the framebuffer on the heap to avoid blowing the stack.
            let mut fb: Box<[u16; PIXELS]> = Box::new([BLACK; PIXELS]);
            let mut angle_x: f32 = 0.0;
            let mut angle_y: f32 = 0.0;
            let mut angle_z: f32 = 0.0;

            info!("renderer: render loop started");

            let mut frame_count: u32 = 0;
            let mut fps_timer = std::time::Instant::now();

            // Accumulators for per-section timing (microseconds)
            let mut t_render_us: u64 = 0;
            let mut t_flush_us: u64 = 0;
            let mut t_total_us: u64 = 0;

            // Set the address window once for the full screen — write_pixels_buffered
            // reuses it every frame, skipping repeated set_address_window SPI commands.
            let _ = display.set_address_window(0, 0, (W - 1) as u16, (H - 1) as u16);

            loop {
                if !running_thread.load(Ordering::Relaxed) {
                    // Clear screen and wait
                    fb.fill(BLACK);
                    let _ = display.write_pixels_buffered(fb.iter().copied());
                    frame_count = 0;
                    fps_timer = std::time::Instant::now();
                    t_render_us = 0;
                    t_flush_us = 0;
                    t_total_us = 0;
                    // Sleep until started again
                    while !running_thread.load(Ordering::Relaxed) {
                        FreeRtos::delay_ms(100);
                    }
                    // Restore address window after stop/start cycle
                    let _ = display.set_address_window(0, 0, (W - 1) as u16, (H - 1) as u16);
                }

                let t_frame_start = std::time::Instant::now();

                // ── render (CPU: clear fb + MVP + rasterize) ──────────────────
                let t0 = std::time::Instant::now();
                render_frame(&mut fb, angle_x, angle_y, angle_z);
                t_render_us += t0.elapsed().as_micros() as u64;

                // ── flush (SPI transfer to display) ───────────────────────────
                let t1 = std::time::Instant::now();
                let _ = display.write_pixels_buffered(fb.iter().copied());
                t_flush_us += t1.elapsed().as_micros() as u64;

                // Yield to the FreeRTOS scheduler so the IDLE task can run and
                // reset the Task Watchdog Timer. Without this, the polling SPI
                // busy-loop starves IDLE for ~40ms and triggers the TWDT.
                FreeRtos::delay_ms(1);

                t_total_us += t_frame_start.elapsed().as_micros() as u64;
                frame_count += 1;

                let elapsed = fps_timer.elapsed();
                if elapsed.as_secs() >= 1 {
                    let fps = frame_count as f32 / elapsed.as_secs_f32();
                    let n = frame_count.max(1) as u64;
                    info!(
                        "renderer: {:.1} fps | render={:.1}ms  flush={:.1}ms  total={:.1}ms",
                        fps,
                        (t_render_us / n) as f32 / 1000.0,
                        (t_flush_us / n) as f32 / 1000.0,
                        (t_total_us / n) as f32 / 1000.0,
                    );
                    frame_count = 0;
                    fps_timer = std::time::Instant::now();
                    t_render_us = 0;
                    t_flush_us = 0;
                    t_total_us = 0;
                }

                // Advance rotation angles
                angle_x += 0.03;
                angle_y += 0.05;
                angle_z += 0.02;

                // Wrap to avoid float drift
                let two_pi = core::f32::consts::PI * 2.0;
                if angle_x > two_pi {
                    angle_x -= two_pi;
                }
                if angle_y > two_pi {
                    angle_y -= two_pi;
                }
                if angle_z > two_pi {
                    angle_z -= two_pi;
                }
            }
        })
        .map_err(|e| anyhow::anyhow!("spawn failed: {:?}", e))?;

    // HTTP: GET /api/renderer/start
    server.fn_handler(
        "/api/renderer/start",
        esp_idf_svc::http::Method::Get,
        move |req| {
            running_start.store(true, Ordering::Relaxed);
            let mut resp =
                req.into_response(200, Some("OK"), &[("Content-Type", "application/json")])?;
            resp.write(br#"{"running":true}"#)?;
            Ok::<(), anyhow::Error>(())
        },
    )?;

    // HTTP: GET /api/renderer/stop
    server.fn_handler(
        "/api/renderer/stop",
        esp_idf_svc::http::Method::Get,
        move |req| {
            running_stop.store(false, Ordering::Relaxed);
            let mut resp =
                req.into_response(200, Some("OK"), &[("Content-Type", "application/json")])?;
            resp.write(br#"{"running":false}"#)?;
            Ok::<(), anyhow::Error>(())
        },
    )?;

    info!("renderer module ready");

    Ok(CARD_HTML.to_string())
}
