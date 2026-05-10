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

#[derive(Clone, Copy)]
struct Mat4([[f32; 4]; 4]);

impl Mat4 {
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
    fn rotation_x(a: f32) -> Self {
        let (s, c) = (libm::sinf(a), libm::cosf(a));
        Mat4([
            [1., 0., 0., 0.],
            [0., c, s, 0.],
            [0., -s, c, 0.],
            [0., 0., 0., 1.],
        ])
    }
    fn rotation_y(a: f32) -> Self {
        let (s, c) = (libm::sinf(a), libm::cosf(a));
        Mat4([
            [c, 0., -s, 0.],
            [0., 1., 0., 0.],
            [s, 0., c, 0.],
            [0., 0., 0., 1.],
        ])
    }
    fn rotation_z(a: f32) -> Self {
        let (s, c) = (libm::sinf(a), libm::cosf(a));
        Mat4([
            [c, s, 0., 0.],
            [-s, c, 0., 0.],
            [0., 0., 1., 0.],
            [0., 0., 0., 1.],
        ])
    }
    fn translation(tx: f32, ty: f32, tz: f32) -> Self {
        Mat4([
            [1., 0., 0., 0.],
            [0., 1., 0., 0.],
            [0., 0., 1., 0.],
            [tx, ty, tz, 1.],
        ])
    }
    fn perspective(fov_y_rad: f32, aspect: f32, near: f32, far: f32) -> Self {
        let f = 1.0 / libm::tanf(fov_y_rad * 0.5);
        let ri = 1.0 / (near - far);
        Mat4([
            [f / aspect, 0., 0., 0.],
            [0., f, 0., 0.],
            [0., 0., (near + far) * ri, -1.],
            [0., 0., 2. * near * far * ri, 0.],
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

#[inline(always)]
fn put_pixel(fb: &mut [u8; FB_BYTES], x: usize, y: usize, color: u16) {
    let idx = (y * W + x) * 2;
    fb[idx] = (color >> 8) as u8;
    fb[idx + 1] = color as u8;
}

fn draw_line(fb: &mut [u8; FB_BYTES], mut x0: i32, mut y0: i32, x1: i32, y1: i32, color: u16) {
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx: i32 = if x0 < x1 { 1 } else { -1 };
    let sy: i32 = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        if x0 >= 0 && x0 < W as i32 && y0 >= 0 && y0 < H as i32 {
            put_pixel(fb, x0 as usize, y0 as usize, color);
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

fn render_frame(fb: &mut [u8; FB_BYTES], ax: f32, ay: f32, az: f32) -> (u64, u64, u64) {
    let t0 = std::time::Instant::now();
    fb.fill(0);
    let clear_us = t0.elapsed().as_micros() as u64;

    let mvp = Mat4::perspective(FOV_RAD, W as f32 / H as f32, NEAR_PLANE, FAR_PLANE)
        .mul(&Mat4::translation(0., 0., -CAMERA_Z))
        .mul(
            &Mat4::rotation_x(ax)
                .mul(&Mat4::rotation_y(ay))
                .mul(&Mat4::rotation_z(az)),
        );
    let mvp_us = t0.elapsed().as_micros() as u64 - clear_us;

    let hw = W as f32 * 0.5;
    let hh = H as f32 * 0.5;
    let screen: [(i32, i32, f32); 8] = core::array::from_fn(|i| {
        let v = &CUBE_VERTS[i];
        let c = mvp.mul_vec4(Vec4 {
            x: v.x,
            y: v.y,
            z: v.z,
            w: 1.,
        });
        (
            ((c.x / c.w + 1.) * hw) as i32,
            ((-c.y / c.w + 1.) * hh) as i32,
            c.w,
        )
    });

    let t1 = std::time::Instant::now();
    for &(a, b) in &CUBE_EDGES {
        let color = if (screen[a].2 + screen[b].2) * 0.5 > 0. {
            ORANGE
        } else {
            DIM
        };
        draw_line(
            fb,
            screen[a].0,
            screen[a].1,
            screen[b].0,
            screen[b].1,
            color,
        );
    }
    (clear_us, mvp_us, t1.elapsed().as_micros() as u64)
}

// ─── register ─────────────────────────────────────────────────────────────────

pub(super) fn register(
    server: &mut EspHttpServer,
    display: SharedDisplay,
    mode: SharedMode,
) -> anyhow::Result<()> {
    let running = Arc::new(AtomicBool::new(true));
    let running_start = running.clone();
    let running_stop = running.clone();
    let running_thread = running;

    std::thread::Builder::new()
        .stack_size(8192)
        .spawn(move || {
            let mut fb: Box<[u8; FB_BYTES]> = Box::new([0u8; FB_BYTES]);
            let mut ax = 0.0f32;
            let mut ay = 0.0f32;
            let mut az = 0.0f32;
            let mut was_inactive = true;

            info!("display/renderer: render thread started");

            let mut frames: u32 = 0;
            let mut fps_t = std::time::Instant::now();
            let mut t_cl = 0u64;
            let mut t_mv = 0u64;
            let mut t_rs = 0u64;
            let mut t_cmd = 0u64;
            let mut t_dat = 0u64;
            let mut t_tot = 0u64;

            loop {
                let active = mode.load(Ordering::Relaxed) == MODE_RENDERER
                    && running_thread.load(Ordering::Relaxed);

                if !active {
                    if !was_inactive {
                        display.lock().unwrap().fill_solid(0x0000);
                        was_inactive = true;
                        frames = 0;
                        fps_t = std::time::Instant::now();
                        t_cl = 0;
                        t_mv = 0;
                        t_rs = 0;
                        t_cmd = 0;
                        t_dat = 0;
                        t_tot = 0;
                    }
                    FreeRtos::delay_ms(50);
                    continue;
                }
                if was_inactive {
                    display.lock().unwrap().set_full_frame_window();
                    was_inactive = false;
                }

                let tf = std::time::Instant::now();
                let (cl, mv, rs) = render_frame(&mut fb, ax, ay, az);
                t_cl += cl;
                t_mv += mv;
                t_rs += rs;

                let (cmd, dat) = display.lock().unwrap().flush_frame_timed(&fb);
                t_cmd += cmd;
                t_dat += dat;
                t_tot += tf.elapsed().as_micros() as u64;
                frames += 1;

                if fps_t.elapsed().as_secs() >= 1 {
                    let fps = frames as f32 / fps_t.elapsed().as_secs_f32();
                    let n = frames.max(1) as u64;
                    info!(
                        "display/renderer: {:.1} fps total={:.2}ms | \
                        clear={:.2}ms mvp={:.2}ms raster={:.2}ms | \
                        flush_cmd={:.2}ms flush_data={:.2}ms flush={:.2}ms",
                        fps,
                        (t_tot / n) as f32 / 1000.,
                        (t_cl / n) as f32 / 1000.,
                        (t_mv / n) as f32 / 1000.,
                        (t_rs / n) as f32 / 1000.,
                        (t_cmd / n) as f32 / 1000.,
                        (t_dat / n) as f32 / 1000.,
                        (t_cmd + t_dat) as f32 / n as f32 / 1000.,
                    );
                    frames = 0;
                    fps_t = std::time::Instant::now();
                    t_cl = 0;
                    t_mv = 0;
                    t_rs = 0;
                    t_cmd = 0;
                    t_dat = 0;
                    t_tot = 0;
                }

                ax += ANGLE_SPEED_X;
                ay += ANGLE_SPEED_Y;
                az += ANGLE_SPEED_Z;
                let pi2 = core::f32::consts::PI * 2.0;
                if ax > pi2 {
                    ax -= pi2;
                }
                if ay > pi2 {
                    ay -= pi2;
                }
                if az > pi2 {
                    az -= pi2;
                }
            }
        })
        .map_err(|e| anyhow::anyhow!("spawn failed: {:?}", e))?;

    {
        let r = running_start;
        server.fn_handler(
            "/api/display/renderer/start",
            esp_idf_svc::http::Method::Get,
            move |req| {
                r.store(true, Ordering::Relaxed);
                let mut resp =
                    req.into_response(200, Some("OK"), &[("Content-Type", "application/json")])?;
                resp.write(br#"{"running":true}"#)?;
                Ok::<(), anyhow::Error>(())
            },
        )?;
    }
    {
        let r = running_stop;
        server.fn_handler(
            "/api/display/renderer/stop",
            esp_idf_svc::http::Method::Get,
            move |req| {
                r.store(false, Ordering::Relaxed);
                let mut resp =
                    req.into_response(200, Some("OK"), &[("Content-Type", "application/json")])?;
                resp.write(br#"{"running":false}"#)?;
                Ok::<(), anyhow::Error>(())
            },
        )?;
    }

    info!("display/renderer: endpoints registered");
    Ok(())
}
