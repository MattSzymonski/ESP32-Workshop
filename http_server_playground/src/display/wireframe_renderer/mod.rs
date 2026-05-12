// Renderer submodule — software 3-D rasteriser drawing a rotating cube.
//
// Uses the shared display hardware via a Mutex, holding the lock only during the
// ~5ms SPI flush. The ~0.4ms CPU render runs without the lock. The thread sleeps
// when the parent display module''s mode is not MODE_RENDERER.

use esp_idf_svc::hal::delay::FreeRtos;
use esp_idf_svc::http::server::EspHttpServer;
use log::info;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::{SharedDisplay, SharedMode, FB_BYTES, H, MODE_RENDERER, W};

pub(super) const CARD_HTML: &str = include_str!("card.html");

// ─── colours (RGB565) ────────────────────────────────────────────────────────

/// Packs 8-bit R, G, B components into a 16-bit RGB565 word.
const fn rgb565(r: u8, g: u8, b: u8) -> u16 {
    (((r as u16) & 0xF8) << 8) | (((g as u16) & 0xFC) << 3) | ((b as u16) >> 3)
}
const ORANGE: u16 = rgb565(255, 133, 89);
const DIM: u16 = rgb565(80, 40, 20);

// ─── scene / animation config ─────────────────────────────────────────────────

const FOV_RAD: f32 = core::f32::consts::PI / 3.0;
const CAMERA_Z: f32 = 2.0;
const NEAR_PLANE: f32 = 0.1;
const FAR_PLANE: f32 = 100.0;
const ANGLE_SPEED_X: f32 = 0.03;
const ANGLE_SPEED_Y: f32 = 0.05;
const ANGLE_SPEED_Z: f32 = 0.02;

// ─── 3-D maths ────────────────────────────────────────────────────────────────

/// 3-component float vector used for cube vertex positions.
#[derive(Clone, Copy)]
struct Vec3 {
    x: f32,
    y: f32,
    z: f32,
}

/// 4-component float vector used for homogeneous clip-space coordinates.
#[derive(Clone, Copy)]
struct Vec4 {
    x: f32,
    y: f32,
    z: f32,
    w: f32,
}

/// Column-major 4×4 float matrix. Columns are stored as `mat[col][row]`.
#[derive(Clone, Copy)]
struct Mat4([[f32; 4]; 4]);

impl Mat4 {
    /// Multiplies this matrix by `other` (self × other).
    fn mul(&self, other: &Mat4) -> Mat4 {
        let left = &self.0;
        let right = &other.0;
        let mut result = [[0.0f32; 4]; 4];
        for col in 0..4 {
            for row in 0..4 {
                result[col][row] = left[0][row] * right[col][0]
                    + left[1][row] * right[col][1]
                    + left[2][row] * right[col][2]
                    + left[3][row] * right[col][3];
            }
        }
        Mat4(result)
    }
    /// Multiplies this matrix by a column vector, returning a transformed Vec4.
    fn mul_vec4(&self, vec: Vec4) -> Vec4 {
        let matrix = &self.0;
        Vec4 {
            x: matrix[0][0] * vec.x
                + matrix[1][0] * vec.y
                + matrix[2][0] * vec.z
                + matrix[3][0] * vec.w,
            y: matrix[0][1] * vec.x
                + matrix[1][1] * vec.y
                + matrix[2][1] * vec.z
                + matrix[3][1] * vec.w,
            z: matrix[0][2] * vec.x
                + matrix[1][2] * vec.y
                + matrix[2][2] * vec.z
                + matrix[3][2] * vec.w,
            w: matrix[0][3] * vec.x
                + matrix[1][3] * vec.y
                + matrix[2][3] * vec.z
                + matrix[3][3] * vec.w,
        }
    }
    /// Builds a rotation matrix around the X axis by `angle` radians.
    fn rotation_x(angle: f32) -> Self {
        let (sin_a, cos_a) = (libm::sinf(angle), libm::cosf(angle));
        Mat4([
            [1., 0., 0., 0.],
            [0., cos_a, sin_a, 0.],
            [0., -sin_a, cos_a, 0.],
            [0., 0., 0., 1.],
        ])
    }
    /// Builds a rotation matrix around the Y axis by `angle` radians.
    fn rotation_y(angle: f32) -> Self {
        let (sin_a, cos_a) = (libm::sinf(angle), libm::cosf(angle));
        Mat4([
            [cos_a, 0., -sin_a, 0.],
            [0., 1., 0., 0.],
            [sin_a, 0., cos_a, 0.],
            [0., 0., 0., 1.],
        ])
    }
    /// Builds a rotation matrix around the Z axis by `angle` radians.
    fn rotation_z(angle: f32) -> Self {
        let (sin_a, cos_a) = (libm::sinf(angle), libm::cosf(angle));
        Mat4([
            [cos_a, sin_a, 0., 0.],
            [-sin_a, cos_a, 0., 0.],
            [0., 0., 1., 0.],
            [0., 0., 0., 1.],
        ])
    }
    /// Builds a translation matrix for the given (tx, ty, tz) offset.
    fn translation(tx: f32, ty: f32, tz: f32) -> Self {
        Mat4([
            [1., 0., 0., 0.],
            [0., 1., 0., 0.],
            [0., 0., 1., 0.],
            [tx, ty, tz, 1.],
        ])
    }
    /// Builds an OpenGL-style perspective projection matrix.
    /// `fov_y_rad` is the vertical field of view in radians.
    fn perspective(fov_y_rad: f32, aspect: f32, near: f32, far: f32) -> Self {
        let focal_length = 1.0 / libm::tanf(fov_y_rad * 0.5);
        let inv_depth_range = 1.0 / (near - far);
        Mat4([
            [focal_length / aspect, 0., 0., 0.],
            [0., focal_length, 0., 0.],
            [0., 0., (near + far) * inv_depth_range, -1.],
            [0., 0., 2. * near * far * inv_depth_range, 0.],
        ])
    }
}

// ─── cube geometry ────────────────────────────────────────────────────────────

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
const CUBE_EDGES: [(usize, usize); 12] = [
    (0, 1),
    (1, 2),
    (2, 3),
    (3, 0),
    (4, 5),
    (5, 6),
    (6, 7),
    (7, 4),
    (0, 4),
    (1, 5),
    (2, 6),
    (3, 7),
];

// ─── software rasterizer ─────────────────────────────────────────────────────

/// Writes a single RGB565 pixel into the flat RGB565 framebuffer at (x, y).
/// Each pixel occupies 2 bytes, high byte first (big-endian).
#[inline(always)]
fn put_pixel(framebuffer: &mut [u8; FB_BYTES], x: usize, y: usize, color: u16) {
    let idx = (y * W + x) * 2;
    framebuffer[idx] = (color >> 8) as u8;
    framebuffer[idx + 1] = color as u8;
}

/// Draws an anti-aliasing-free line from (x_curr, y_curr) to (x_end, y_end)
/// using Bresenham's line algorithm. Pixels outside the framebuffer are silently clipped.
fn draw_line(
    framebuffer: &mut [u8; FB_BYTES],
    mut x_curr: i32,
    mut y_curr: i32,
    x_end: i32,
    y_end: i32,
    color: u16,
) {
    let delta_x = (x_end - x_curr).abs();
    let delta_y = -(y_end - y_curr).abs();
    let step_x: i32 = if x_curr < x_end { 1 } else { -1 };
    let step_y: i32 = if y_curr < y_end { 1 } else { -1 };
    let mut error = delta_x + delta_y;
    loop {
        if x_curr >= 0 && x_curr < W as i32 && y_curr >= 0 && y_curr < H as i32 {
            put_pixel(framebuffer, x_curr as usize, y_curr as usize, color);
        }
        if x_curr == x_end && y_curr == y_end {
            break;
        }
        let error_doubled = 2 * error;
        if error_doubled >= delta_y {
            error += delta_y;
            x_curr += step_x;
        }
        if error_doubled <= delta_x {
            error += delta_x;
            y_curr += step_y;
        }
    }
}

/// Renders one frame of the rotating cube into `framebuffer`.
/// Returns timing breakdowns as (clear_us, mvp_build_us, raster_us).
fn render_frame(
    framebuffer: &mut [u8; FB_BYTES],
    angle_x: f32,
    angle_y: f32,
    angle_z: f32,
) -> (u64, u64, u64) {
    // Clear the framebuffer to black and record how long it took.
    let render_timer = std::time::Instant::now();
    framebuffer.fill(0);
    let clear_us = render_timer.elapsed().as_micros() as u64;

    // Build the model-view-projection matrix: perspective × translate × rotate.
    let mvp = Mat4::perspective(FOV_RAD, W as f32 / H as f32, NEAR_PLANE, FAR_PLANE)
        .mul(&Mat4::translation(0., 0., -CAMERA_Z))
        .mul(
            &Mat4::rotation_x(angle_x)
                .mul(&Mat4::rotation_y(angle_y))
                .mul(&Mat4::rotation_z(angle_z)),
        );
    let mvp_us = render_timer.elapsed().as_micros() as u64 - clear_us;

    // Project each cube vertex from 3-D world space to 2-D screen pixels.
    let half_width = W as f32 * 0.5;
    let half_height = H as f32 * 0.5;
    let screen: [(i32, i32, f32); 8] = core::array::from_fn(|i| {
        let vertex = &CUBE_VERTS[i];
        let clip_pos = mvp.mul_vec4(Vec4 {
            x: vertex.x,
            y: vertex.y,
            z: vertex.z,
            w: 1.,
        });
        (
            ((clip_pos.x / clip_pos.w + 1.) * half_width) as i32,
            ((-clip_pos.y / clip_pos.w + 1.) * half_height) as i32,
            clip_pos.w,
        )
    });

    // Rasterise each edge as a line. Edges whose average clip-space W is positive
    // are in front of the camera and drawn bright; others are drawn dim.
    let raster_timer = std::time::Instant::now();
    for &(start_idx, end_idx) in &CUBE_EDGES {
        let color = if (screen[start_idx].2 + screen[end_idx].2) * 0.5 > 0. {
            ORANGE
        } else {
            DIM
        };
        draw_line(
            framebuffer,
            screen[start_idx].0,
            screen[start_idx].1,
            screen[end_idx].0,
            screen[end_idx].1,
            color,
        );
    }
    (clear_us, mvp_us, raster_timer.elapsed().as_micros() as u64)
}

// ─── register ─────────────────────────────────────────────────────────────────

/// Spawns the render thread and registers `/api/display/renderer/start` and
/// `/api/display/renderer/stop` endpoints on the HTTP server.
pub(super) fn register(
    server: &mut EspHttpServer,
    display: SharedDisplay,
    mode: SharedMode,
) -> anyhow::Result<()> {
    // Shared flag lets the HTTP handlers start/stop rendering without stopping the thread.
    let running = Arc::new(AtomicBool::new(true));
    let running_start = running.clone();
    let running_stop = running.clone();
    let running_thread = running;

    std::thread::Builder::new()
        .stack_size(8192)
        .spawn(move || {
            // Allocate the 25 KB framebuffer on the heap via Vec to avoid the
            // `Box::new([_; N])` stack copy (would overflow this thread's stack).
            let mut framebuffer: Box<[u8; FB_BYTES]> = vec![0u8; FB_BYTES]
                .into_boxed_slice()
                .try_into()
                .expect("framebuffer length mismatch");
            let mut angle_x = 0.0f32;
            let mut angle_y = 0.0f32;
            let mut angle_z = 0.0f32;
            let mut was_inactive = true;

            info!("display/renderer: render thread started");

            // Per-second FPS and per-stage timing accumulators.
            let mut frames: u32 = 0;
            let mut fps_timer = std::time::Instant::now();
            let mut time_clear_us = 0u64;
            let mut time_mvp_us = 0u64;
            let mut time_raster_us = 0u64;
            let mut time_flush_cmd_us = 0u64;
            let mut time_flush_data_us = 0u64;
            let mut time_total_us = 0u64;

            loop {
                // Sleep at low cost when this mode is not active.
                let active = mode.load(Ordering::Relaxed) == MODE_RENDERER
                    && running_thread.load(Ordering::Relaxed);

                if !active {
                    // On the first inactive tick, blank the display and reset accumulators.
                    if !was_inactive {
                        display.lock().unwrap().fill_solid(0x0000);
                        was_inactive = true;
                        frames = 0;
                        fps_timer = std::time::Instant::now();
                        time_clear_us = 0;
                        time_mvp_us = 0;
                        time_raster_us = 0;
                        time_flush_cmd_us = 0;
                        time_flush_data_us = 0;
                        time_total_us = 0;
                    }
                    FreeRtos::delay_ms(50);
                    continue;
                }
                if was_inactive {
                    // Reconfigure the display window before the first frame after activation.
                    display.lock().unwrap().set_full_frame_window();
                    was_inactive = false;
                }

                let frame_timer = std::time::Instant::now();
                let (clear_us, mvp_us, raster_us) =
                    render_frame(&mut framebuffer, angle_x, angle_y, angle_z);
                time_clear_us += clear_us;
                time_mvp_us += mvp_us;
                time_raster_us += raster_us;

                let (flush_cmd_us, flush_data_us) =
                    display.lock().unwrap().flush_frame_timed(&framebuffer);
                time_flush_cmd_us += flush_cmd_us;
                time_flush_data_us += flush_data_us;
                time_total_us += frame_timer.elapsed().as_micros() as u64;
                frames += 1;

                if fps_timer.elapsed().as_secs() >= 1 {
                    // Log per-stage averages once per second then reset accumulators.
                    let fps = frames as f32 / fps_timer.elapsed().as_secs_f32();
                    let frame_count = frames.max(1) as u64;
                    info!(
                        "display/renderer: {:.1} fps total={:.2}ms | \
                        clear={:.2}ms mvp={:.2}ms raster={:.2}ms | \
                        flush_cmd={:.2}ms flush_data={:.2}ms flush={:.2}ms",
                        fps,
                        (time_total_us / frame_count) as f32 / 1000.,
                        (time_clear_us / frame_count) as f32 / 1000.,
                        (time_mvp_us / frame_count) as f32 / 1000.,
                        (time_raster_us / frame_count) as f32 / 1000.,
                        (time_flush_cmd_us / frame_count) as f32 / 1000.,
                        (time_flush_data_us / frame_count) as f32 / 1000.,
                        (time_flush_cmd_us + time_flush_data_us) as f32
                            / frame_count as f32
                            / 1000.,
                    );
                    frames = 0;
                    fps_timer = std::time::Instant::now();
                    time_clear_us = 0;
                    time_mvp_us = 0;
                    time_raster_us = 0;
                    time_flush_cmd_us = 0;
                    time_flush_data_us = 0;
                    time_total_us = 0;
                }

                // Advance rotation angles and wrap at 2π to avoid float drift.
                angle_x += ANGLE_SPEED_X;
                angle_y += ANGLE_SPEED_Y;
                angle_z += ANGLE_SPEED_Z;
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

    // GET /api/display/renderer/start — sets the running flag so the render thread produces frames.
    {
        let running_flag = running_start;
        server.fn_handler(
            "/api/display/renderer/start",
            esp_idf_svc::http::Method::Get,
            move |req| {
                running_flag.store(true, Ordering::Relaxed);
                let mut response =
                    req.into_response(200, Some("OK"), &[("Content-Type", "application/json")])?;
                response.write(br#"{"running":true}"#)?;
                Ok::<(), anyhow::Error>(())
            },
        )?;
    }
    // GET /api/display/renderer/stop — clears the running flag; the thread blanks the display.
    {
        let running_flag = running_stop;
        server.fn_handler(
            "/api/display/renderer/stop",
            esp_idf_svc::http::Method::Get,
            move |req| {
                running_flag.store(false, Ordering::Relaxed);
                let mut response =
                    req.into_response(200, Some("OK"), &[("Content-Type", "application/json")])?;
                response.write(br#"{"running":false}"#)?;
                Ok::<(), anyhow::Error>(())
            },
        )?;
    }

    info!("display/renderer: endpoints registered");
    Ok(())
}
