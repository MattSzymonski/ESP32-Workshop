// Shader-based renderer submodule — software rasteriser with vertex/fragment shaders.
//
// Implements a complete tiny rendering pipeline from scratch:
//   * MVP transform built per frame (model rotation × view × perspective projection).
//   * Vertex shader: transforms each vertex to clip space and produces varyings
//     (uv coordinate + Gouraud light intensity).
//   * Triangle rasteriser with screen-space bounding box and barycentric coverage.
//   * Perspective-correct interpolation of varyings via 1/w weighting.
//   * Per-pixel z-buffer for hidden-surface removal.
//   * Fragment shader: outputs colour from interpolated varyings (UV coords modulated
//     by Gouraud light intensity).
//
// Uses the shared display Mutex the same way as wireframe_renderer: the lock is
// only held during the ~5 ms SPI flush; the ~tens-of-ms render happens lock-free.

use esp_idf_svc::hal::delay::FreeRtos;
use esp_idf_svc::http::server::EspHttpServer;
use log::info;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::{SharedDisplay, SharedMode, FB_BYTES, H, MODE_SHADER, PIXELS, W};

pub(super) const CARD_HTML: &str = include_str!("card.html");

// ─── scene / animation config ─────────────────────────────────────────────────

const FOV_RAD: f32 = core::f32::consts::PI / 3.0;
const CAMERA_Z: f32 = 2.5;
const NEAR_PLANE: f32 = 0.1;
const FAR_PLANE: f32 = 100.0;
const ANGLE_SPEED_X: f32 = 0.025;
const ANGLE_SPEED_Y: f32 = 0.04;
const ANGLE_SPEED_Z: f32 = 0.015;

/// Ambient light term added to the Gouraud diffuse contribution.
const AMBIENT: f32 = 0.25;

// ─── linear-algebra primitives ────────────────────────────────────────────────

/// 3-component float vector.
#[derive(Clone, Copy)]
struct Vec3 {
    x: f32,
    y: f32,
    z: f32,
}

impl Vec3 {
    #[allow(dead_code)] // Kept for clarity / future shader experiments.
    fn dot(self, other: Vec3) -> f32 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }
    #[allow(dead_code)] // Kept for clarity / future shader experiments.
    fn normalised(self) -> Vec3 {
        let len_sq = self.dot(self);
        if len_sq <= 0.0 {
            return self;
        }
        let inv_len = 1.0 / libm::sqrtf(len_sq);
        Vec3 {
            x: self.x * inv_len,
            y: self.y * inv_len,
            z: self.z * inv_len,
        }
    }
}

/// 4-component float vector used for homogeneous clip-space positions.
#[derive(Clone, Copy)]
struct Vec4 {
    x: f32,
    y: f32,
    z: f32,
    w: f32,
}

/// Column-major 4×4 matrix. Stored as `mat[col][row]`.
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

    /// Multiplies this matrix by a column vector.
    fn mul_vec4(&self, vec: Vec4) -> Vec4 {
        let m = &self.0;
        Vec4 {
            x: m[0][0] * vec.x + m[1][0] * vec.y + m[2][0] * vec.z + m[3][0] * vec.w,
            y: m[0][1] * vec.x + m[1][1] * vec.y + m[2][1] * vec.z + m[3][1] * vec.w,
            z: m[0][2] * vec.x + m[1][2] * vec.y + m[2][2] * vec.z + m[3][2] * vec.w,
            w: m[0][3] * vec.x + m[1][3] * vec.y + m[2][3] * vec.z + m[3][3] * vec.w,
        }
    }

    /// Multiplies the upper-left 3×3 portion of this matrix by a Vec3
    /// (used to transform direction vectors / normals without translation).
    fn mul_vec3_dir(&self, vec: Vec3) -> Vec3 {
        let m = &self.0;
        Vec3 {
            x: m[0][0] * vec.x + m[1][0] * vec.y + m[2][0] * vec.z,
            y: m[0][1] * vec.x + m[1][1] * vec.y + m[2][1] * vec.z,
            z: m[0][2] * vec.x + m[1][2] * vec.y + m[2][2] * vec.z,
        }
    }

    fn rotation_x(angle: f32) -> Self {
        let (s, c) = (libm::sinf(angle), libm::cosf(angle));
        Mat4([
            [1., 0., 0., 0.],
            [0., c, s, 0.],
            [0., -s, c, 0.],
            [0., 0., 0., 1.],
        ])
    }
    fn rotation_y(angle: f32) -> Self {
        let (s, c) = (libm::sinf(angle), libm::cosf(angle));
        Mat4([
            [c, 0., -s, 0.],
            [0., 1., 0., 0.],
            [s, 0., c, 0.],
            [0., 0., 0., 1.],
        ])
    }
    fn rotation_z(angle: f32) -> Self {
        let (s, c) = (libm::sinf(angle), libm::cosf(angle));
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
    /// OpenGL-style perspective projection with `fov_y` in radians.
    fn perspective(fov_y_rad: f32, aspect: f32, near: f32, far: f32) -> Self {
        let f = 1.0 / libm::tanf(fov_y_rad * 0.5);
        let inv_depth = 1.0 / (near - far);
        Mat4([
            [f / aspect, 0., 0., 0.],
            [0., f, 0., 0.],
            [0., 0., (near + far) * inv_depth, -1.],
            [0., 0., 2. * near * far * inv_depth, 0.],
        ])
    }
}

// ─── geometry ────────────────────────────────────────────────────────────────

/// Vertex attributes input to the vertex shader.
#[derive(Clone, Copy)]
struct Vertex {
    pos: Vec3,
    normal: Vec3,
    uv: (f32, f32),
}

const FACE_COUNT: usize = 6;
const VERTS_PER_FACE: usize = 4;
const TRIS_PER_FACE: usize = 2;
const INDICES_PER_FACE: usize = TRIS_PER_FACE * 3;

/// 24 unique vertices of the cube grouped by face (4 verts/face × 6 faces).
/// Each face has its own outward normal and a (0,0)–(1,1) UV mapping.
const fn cube_vertices() -> [Vertex; FACE_COUNT * VERTS_PER_FACE] {
    const fn face(a: Vec3, b: Vec3, c: Vec3, d: Vec3, n: Vec3) -> [Vertex; 4] {
        [
            Vertex {
                pos: a,
                normal: n,
                uv: (0.0, 0.0),
            },
            Vertex {
                pos: b,
                normal: n,
                uv: (1.0, 0.0),
            },
            Vertex {
                pos: c,
                normal: n,
                uv: (1.0, 1.0),
            },
            Vertex {
                pos: d,
                normal: n,
                uv: (0.0, 1.0),
            },
        ]
    }
    let p000 = Vec3 {
        x: -0.5,
        y: -0.5,
        z: -0.5,
    };
    let p100 = Vec3 {
        x: 0.5,
        y: -0.5,
        z: -0.5,
    };
    let p110 = Vec3 {
        x: 0.5,
        y: 0.5,
        z: -0.5,
    };
    let p010 = Vec3 {
        x: -0.5,
        y: 0.5,
        z: -0.5,
    };
    let p001 = Vec3 {
        x: -0.5,
        y: -0.5,
        z: 0.5,
    };
    let p101 = Vec3 {
        x: 0.5,
        y: -0.5,
        z: 0.5,
    };
    let p111 = Vec3 {
        x: 0.5,
        y: 0.5,
        z: 0.5,
    };
    let p011 = Vec3 {
        x: -0.5,
        y: 0.5,
        z: 0.5,
    };
    let nx_pos = Vec3 {
        x: 1.,
        y: 0.,
        z: 0.,
    };
    let nx_neg = Vec3 {
        x: -1.,
        y: 0.,
        z: 0.,
    };
    let ny_pos = Vec3 {
        x: 0.,
        y: 1.,
        z: 0.,
    };
    let ny_neg = Vec3 {
        x: 0.,
        y: -1.,
        z: 0.,
    };
    let nz_pos = Vec3 {
        x: 0.,
        y: 0.,
        z: 1.,
    };
    let nz_neg = Vec3 {
        x: 0.,
        y: 0.,
        z: -1.,
    };
    let f0 = face(p001, p101, p111, p011, nz_pos); // front  (+Z)
    let f1 = face(p100, p000, p010, p110, nz_neg); // back   (−Z)
    let f2 = face(p101, p100, p110, p111, nx_pos); // right  (+X)
    let f3 = face(p000, p001, p011, p010, nx_neg); // left   (−X)
    let f4 = face(p011, p111, p110, p010, ny_pos); // top    (+Y)
    let f5 = face(p000, p100, p101, p001, ny_neg); // bottom (−Y)
    [
        f0[0], f0[1], f0[2], f0[3], f1[0], f1[1], f1[2], f1[3], f2[0], f2[1], f2[2], f2[3], f3[0],
        f3[1], f3[2], f3[3], f4[0], f4[1], f4[2], f4[3], f5[0], f5[1], f5[2], f5[3],
    ]
}
const CUBE_VERTS: [Vertex; FACE_COUNT * VERTS_PER_FACE] = cube_vertices();

/// Triangle vertex indices into CUBE_VERTS — for face F (verts at F*4..F*4+3) the
/// triangles are (0,2,1) and (0,3,2) of that face.
///
/// The order is (0,2,1) rather than the natural (0,1,2) because the viewport
/// applies a y-flip (screen y-down). A face that is CCW in eye space becomes
/// CW in screen space; reversing the indices flips it back to CCW so the
/// rasteriser's `area2_f > 0` test matches the face-level `normal.z > 0`
/// backface test.
const CUBE_INDICES: [u8; FACE_COUNT * INDICES_PER_FACE] = {
    let mut out = [0u8; FACE_COUNT * INDICES_PER_FACE];
    let mut f = 0usize;
    while f < FACE_COUNT {
        let base = (f * VERTS_PER_FACE) as u8;
        let i = f * INDICES_PER_FACE;
        out[i] = base;
        out[i + 1] = base + 2;
        out[i + 2] = base + 1;
        out[i + 3] = base;
        out[i + 4] = base + 3;
        out[i + 5] = base + 2;
        f += 1;
    }
    out
};

// ─── pipeline data ────────────────────────────────────────────────────────────

/// Output of the combined vertex shader + projection + viewport map.
/// All attribute values are pre-scaled to byte range (0..255) ready for the
/// integer fixed-point interpolation in the rasteriser.
#[derive(Clone, Copy)]
struct VsOutput {
    /// True if the vertex is in front of the near plane (`clip.w > 0`).
    valid: bool,
    /// Pixel-space x, y after the perspective divide and viewport transform.
    sx: f32,
    sy: f32,
    /// Depth in [0, 1] for the z-buffer (0 = near, 1 = far).
    z_unit: f32,
    /// Vertex attributes in 0..255 scale.
    u_byte: f32,
    v_byte: f32,
    light_byte: f32,
}

/// Pre-normalised LIGHT_DIR (LIGHT_DIR / |LIGHT_DIR|).
/// LIGHT_DIR = (0.5, 0.7, 0.8) with |LIGHT_DIR| = sqrt(1.38) ≈ 1.17473.
const LIGHT_DIR_NORM: Vec3 = Vec3 {
    x: 0.42566073,
    y: 0.59592503,
    z: 0.68105716,
};

// ─── shaders ──────────────────────────────────────────────────────────────────

/// Combined vertex shader + perspective divide + viewport map + Gouraud lighting.
/// Cube normals are unit length and `model_rot` is a pure rotation, so the world
/// normal is also unit length — we skip the per-vertex `normalised()` call.
#[inline(always)]
fn vertex_shader_project(vertex: &Vertex, mvp: &Mat4, model_rot: &Mat4) -> VsOutput {
    let clip = mvp.mul_vec4(Vec4 {
        x: vertex.pos.x,
        y: vertex.pos.y,
        z: vertex.pos.z,
        w: 1.0,
    });
    if clip.w <= 0.0 {
        return VsOutput {
            valid: false,
            sx: 0.0,
            sy: 0.0,
            z_unit: 0.0,
            u_byte: 0.0,
            v_byte: 0.0,
            light_byte: 0.0,
        };
    }
    let inv_w = 1.0 / clip.w;
    let world_n = model_rot.mul_vec3_dir(vertex.normal);
    let n_dot_l = (world_n.x * LIGHT_DIR_NORM.x
        + world_n.y * LIGHT_DIR_NORM.y
        + world_n.z * LIGHT_DIR_NORM.z)
        .max(0.0);
    let intensity = AMBIENT + (1.0 - AMBIENT) * n_dot_l;
    VsOutput {
        valid: true,
        sx: (clip.x * inv_w + 1.0) * (W as f32 * 0.5),
        sy: (1.0 - clip.y * inv_w) * (H as f32 * 0.5),
        z_unit: clip.z * inv_w * 0.5 + 0.5,
        u_byte: vertex.uv.0 * 255.0,
        v_byte: vertex.uv.1 * 255.0,
        light_byte: intensity * 255.0,
    }
}

// ─── rasteriser (integer hot path) ────────────────────────────────────────────
//
// Demo-scene tricks applied:
//   * Integer half-space (edge function) test, evaluated incrementally with adds.
//   * Affine UV / Gouraud / depth interpolation in Q16.16 fixed point (no per-pixel
//     divide — perspective-correct interpolation is dropped because the cube faces
//     are tiny on a 160×80 screen and the error is invisible).
//   * u16 z-buffer cleared by `write_bytes(0xFF)` (true memset) instead of a per-element
//     f32 store loop.
//   * Bitwise OR-trick `(w0 | w1 | w2) >= 0` for the inside-triangle test (one branch).
//   * Pure integer fragment shader: r = (u8·l8)>>8, g = (v8·l8)>>8, RGB565 by shifts.
//   * `unsafe` pointer indexing in the inner loop (bbox is clipped → in-bounds).
//   * Face-level backface cull at the vertex stage (skips up to 18 vertex shader runs).

/// Rasterises one triangle. `pixels_filled` is incremented for each pixel actually written.
fn rasterise_triangle(
    framebuffer: &mut [u8; FB_BYTES],
    z_buffer: &mut [u16; PIXELS],
    v0: &VsOutput,
    v1: &VsOutput,
    v2: &VsOutput,
    pixels_filled: &mut u32,
) {
    // Backface cull via 2-D edge cross-product (signed area × 2).
    let dx01 = v1.sx - v0.sx;
    let dy01 = v1.sy - v0.sy;
    let dx02 = v2.sx - v0.sx;
    let dy02 = v2.sy - v0.sy;
    let area2_f = dx01 * dy02 - dy01 * dx02;
    if area2_f <= 0.5 {
        return; // back-facing or sub-pixel
    }
    let inv_area2 = 1.0 / area2_f;

    // Bounding box clipped to the screen (truncation toward zero is fine — the
    // half-space test rejects pixels not actually covered).
    let min_x = (v0.sx.min(v1.sx).min(v2.sx) as i32).max(0);
    let max_x = (v0.sx.max(v1.sx).max(v2.sx) as i32).min(W as i32 - 1);
    let min_y = (v0.sy.min(v1.sy).min(v2.sy) as i32).max(0);
    let max_y = (v0.sy.max(v1.sy).max(v2.sy) as i32).min(H as i32 - 1);
    if min_x > max_x || min_y > max_y {
        return;
    }

    // Snap vertex coords to the integer pixel grid for integer edge functions.
    let x0 = v0.sx as i32;
    let y0 = v0.sy as i32;
    let x1 = v1.sx as i32;
    let y1 = v1.sy as i32;
    let x2 = v2.sx as i32;
    let y2 = v2.sy as i32;

    // Edge function coefficients: e(x, y) = a*x + b*y + c.
    // (Winding is CCW because we already required area2_f > 0.)
    let a01 = y0 - y1;
    let b01 = x1 - x0;
    let c01 = x0 * y1 - x1 * y0;
    let a12 = y1 - y2;
    let b12 = x2 - x1;
    let c12 = x1 * y2 - x2 * y1;
    let a20 = y2 - y0;
    let b20 = x0 - x2;
    let c20 = x2 * y0 - x0 * y2;

    // Edge values at the top-left of the bbox — then incremented by adds in the loop.
    let mut w0_row = a12 * min_x + b12 * min_y + c12;
    let mut w1_row = a20 * min_x + b20 * min_y + c20;
    let mut w2_row = a01 * min_x + b01 * min_y + c01;

    // Build per-attribute deltas in f32, then convert to Q16.16 fixed point.
    //   attr(x,y) = (w0·A0 + w1·A1 + w2·A2) / area2_f
    //   d/dx      = (a12·A0 + a20·A1 + a01·A2) / area2_f
    //   d/dy      = (b12·A0 + b20·A1 + b01·A2) / area2_f
    macro_rules! setup_attr {
        ($a0:expr, $a1:expr, $a2:expr) => {{
            let a0: f32 = $a0;
            let a1: f32 = $a1;
            let a2: f32 = $a2;
            let dx_f = (a12 as f32 * a0 + a20 as f32 * a1 + a01 as f32 * a2) * inv_area2;
            let dy_f = (b12 as f32 * a0 + b20 as f32 * a1 + b01 as f32 * a2) * inv_area2;
            let init_f = (w0_row as f32 * a0 + w1_row as f32 * a1 + w2_row as f32 * a2) * inv_area2;
            (
                (init_f * 65536.0) as i32,
                (dx_f * 65536.0) as i32,
                (dy_f * 65536.0) as i32,
            )
        }};
    }

    let z0 = v0.z_unit * 65535.0;
    let z1 = v1.z_unit * 65535.0;
    let z2 = v2.z_unit * 65535.0;
    let (mut z_row, dz_dx, dz_dy) = setup_attr!(z0, z1, z2);
    let (mut u_row, du_dx, du_dy) = setup_attr!(v0.u_byte, v1.u_byte, v2.u_byte);
    let (mut v_row, dv_dx, dv_dy) = setup_attr!(v0.v_byte, v1.v_byte, v2.v_byte);
    let (mut l_row, dl_dx, dl_dy) = setup_attr!(v0.light_byte, v1.light_byte, v2.light_byte);

    let mut local_pixels = 0u32;
    let fb_ptr = framebuffer.as_mut_ptr();
    let zb_ptr = z_buffer.as_mut_ptr();

    for py in min_y..=max_y {
        let mut w0 = w0_row;
        let mut w1 = w1_row;
        let mut w2 = w2_row;
        let mut z = z_row;
        let mut u = u_row;
        let mut v = v_row;
        let mut l = l_row;
        let row_start = py as usize * W;

        for px in min_x..=max_x {
            // OR-trick: if any wN has its sign bit set, the OR is negative.
            if (w0 | w1 | w2) >= 0 {
                let idx = row_start + px as usize;
                let z16 = (z >> 16).clamp(0, 65535) as u16;
                let prev_z = unsafe { *zb_ptr.add(idx) };
                if z16 < prev_z {
                    unsafe { *zb_ptr.add(idx) = z16 };
                    let u8_v = (u >> 16).clamp(0, 255) as u32;
                    let v8_v = (v >> 16).clamp(0, 255) as u32;
                    let l8_v = (l >> 16).clamp(0, 255) as u32;
                    // Fragment shader: visualise UV as (R, G), modulated by Gouraud light.
                    let r = (u8_v * l8_v) >> 8; // 0..255
                    let g = (v8_v * l8_v) >> 8;
                    let b = l8_v >> 2; // 0..63 (small blue tint)
                    let color = (((r & 0xF8) as u16) << 8)
                        | (((g & 0xFC) as u16) << 3)
                        | (((b as u16) & 0xF8) >> 3);
                    let byte_idx = idx * 2;
                    unsafe {
                        *fb_ptr.add(byte_idx) = (color >> 8) as u8;
                        *fb_ptr.add(byte_idx + 1) = color as u8;
                    }
                    local_pixels += 1;
                }
            }
            w0 += a12;
            w1 += a20;
            w2 += a01;
            z += dz_dx;
            u += du_dx;
            v += dv_dx;
            l += dl_dx;
        }
        w0_row += b12;
        w1_row += b20;
        w2_row += b01;
        z_row += dz_dy;
        u_row += du_dy;
        v_row += dv_dy;
        l_row += dl_dy;
    }
    *pixels_filled += local_pixels;
}

/// Per-frame timing breakdown returned from `render_frame` for performance reporting.
#[derive(Default, Clone, Copy)]
struct FrameStats {
    clear_color_us: u64,
    clear_depth_us: u64,
    mvp_us: u64,
    vs_us: u64,
    raster_us: u64,
    triangles_drawn: u32,
    pixels_filled: u32,
}

/// Renders one frame of the rotating cube into `framebuffer` and clears `z_buffer`.
fn render_frame(
    framebuffer: &mut [u8; FB_BYTES],
    z_buffer: &mut [u16; PIXELS],
    angle_x: f32,
    angle_y: f32,
    angle_z: f32,
) -> FrameStats {
    let mut stats = FrameStats::default();

    // 1. Clear the colour buffer (slice::fill on u8 lowers to memset).
    let t = std::time::Instant::now();
    framebuffer.fill(0);
    stats.clear_color_us = t.elapsed().as_micros() as u64;

    // 2. Clear the depth buffer with a true memset: every byte = 0xFF → every u16 = 0xFFFF
    //    (max depth). Done unsafely to bypass `slice::fill` which doesn't lower to memset
    //    for non-byte element types.
    let t = std::time::Instant::now();
    unsafe {
        std::ptr::write_bytes(z_buffer.as_mut_ptr() as *mut u8, 0xFF, PIXELS * 2);
    }
    stats.clear_depth_us = t.elapsed().as_micros() as u64;

    // 3. Build the MVP matrix and the bare model rotation (used for normal transform).
    let t = std::time::Instant::now();
    let model_rot = Mat4::rotation_x(angle_x)
        .mul(&Mat4::rotation_y(angle_y))
        .mul(&Mat4::rotation_z(angle_z));
    let view = Mat4::translation(0.0, 0.0, -CAMERA_Z);
    let proj = Mat4::perspective(FOV_RAD, W as f32 / H as f32, NEAR_PLANE, FAR_PLANE);
    let mvp = proj.mul(&view).mul(&model_rot);
    stats.mvp_us = t.elapsed().as_micros() as u64;

    // 4. Vertex stage with face-level backface culling.
    //    Each face's outward normal is shared by all 4 of its vertices, so we
    //    transform the face normal once and skip the entire face if it points
    //    away from the camera (eye-space normal.z <= 0; since the view is a pure
    //    translate along -z, the eye-space normal equals the world-space normal).
    let t = std::time::Instant::now();
    let mut vs = [VsOutput {
        valid: false,
        sx: 0.0,
        sy: 0.0,
        z_unit: 0.0,
        u_byte: 0.0,
        v_byte: 0.0,
        light_byte: 0.0,
    }; FACE_COUNT * VERTS_PER_FACE];
    let mut face_visible = [false; FACE_COUNT];
    for face in 0..FACE_COUNT {
        let face_normal_world = model_rot.mul_vec3_dir(CUBE_VERTS[face * VERTS_PER_FACE].normal);
        if face_normal_world.z > 0.0 {
            face_visible[face] = true;
            for k in 0..VERTS_PER_FACE {
                let vi = face * VERTS_PER_FACE + k;
                vs[vi] = vertex_shader_project(&CUBE_VERTS[vi], &mvp, &model_rot);
            }
        }
    }
    stats.vs_us = t.elapsed().as_micros() as u64;

    // 5. Rasterise each visible triangle.
    let t = std::time::Instant::now();
    let mut tri = 0;
    while tri < CUBE_INDICES.len() {
        let face = tri / INDICES_PER_FACE;
        if face_visible[face] {
            let i0 = CUBE_INDICES[tri] as usize;
            let i1 = CUBE_INDICES[tri + 1] as usize;
            let i2 = CUBE_INDICES[tri + 2] as usize;
            let v0 = &vs[i0];
            let v1 = &vs[i1];
            let v2 = &vs[i2];
            if v0.valid && v1.valid && v2.valid {
                stats.triangles_drawn += 1;
                rasterise_triangle(framebuffer, z_buffer, v0, v1, v2, &mut stats.pixels_filled);
            }
        }
        tri += 3;
    }
    stats.raster_us = t.elapsed().as_micros() as u64;

    stats
}

// ─── module entry point ──────────────────────────────────────────────────────

/// Spawns the render thread and registers `/api/display/shader/start` and
/// `/api/display/shader/stop` endpoints on the HTTP server.
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

    // Stack must be large enough for the pipeline locals; the framebuffer and
    // z-buffer themselves live on the heap (allocated via Vec to avoid materialising
    // 25 KB temporaries on the thread stack — `Box::new([_; N])` would copy
    // the array through the stack and instantly blow it).
    std::thread::Builder::new()
        .stack_size(16384)
        .spawn(move || {
            let mut framebuffer: Box<[u8; FB_BYTES]> = vec![0u8; FB_BYTES]
                .into_boxed_slice()
                .try_into()
                .expect("framebuffer length mismatch");
            let mut z_buffer: Box<[u16; PIXELS]> = vec![0xFFFFu16; PIXELS]
                .into_boxed_slice()
                .try_into()
                .expect("z-buffer length mismatch");
            let mut angle_x = 0.0f32;
            let mut angle_y = 0.0f32;
            let mut angle_z = 0.0f32;
            let mut was_inactive = true;

            info!("display/shader: render thread started");

            // Per-second FPS and per-stage timing accumulators.
            let mut frames: u32 = 0;
            let mut fps_timer = std::time::Instant::now();
            let mut acc = FrameStats::default();
            let mut acc_flush_cmd_us = 0u64;
            let mut acc_flush_data_us = 0u64;
            let mut acc_total_us = 0u64;

            // Reset all per-second accumulators back to zero (used after logging
            // and when the renderer is paused).
            macro_rules! reset_stats {
                () => {
                    frames = 0;
                    fps_timer = std::time::Instant::now();
                    acc = FrameStats::default();
                    acc_flush_cmd_us = 0;
                    acc_flush_data_us = 0;
                    acc_total_us = 0;
                };
            }

            loop {
                // Sleep at low cost when this mode is not active.
                let active = mode.load(Ordering::Relaxed) == MODE_SHADER
                    && running_thread.load(Ordering::Relaxed);

                if !active {
                    if !was_inactive {
                        // On the first inactive tick, blank the display and reset accumulators.
                        display.lock().unwrap().fill_solid(0x0000);
                        was_inactive = true;
                        reset_stats!();
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
                let frame =
                    render_frame(&mut framebuffer, &mut z_buffer, angle_x, angle_y, angle_z);
                acc.clear_color_us += frame.clear_color_us;
                acc.clear_depth_us += frame.clear_depth_us;
                acc.mvp_us += frame.mvp_us;
                acc.vs_us += frame.vs_us;
                acc.raster_us += frame.raster_us;
                acc.triangles_drawn += frame.triangles_drawn;
                acc.pixels_filled += frame.pixels_filled;

                let (flush_cmd_us, flush_data_us) =
                    display.lock().unwrap().flush_frame_timed(&framebuffer);
                acc_flush_cmd_us += flush_cmd_us;
                acc_flush_data_us += flush_data_us;
                acc_total_us += frame_timer.elapsed().as_micros() as u64;
                frames += 1;

                if fps_timer.elapsed().as_secs() >= 1 {
                    // Log full per-stage breakdown once per second.
                    let fps = frames as f32 / fps_timer.elapsed().as_secs_f32();
                    let n = frames.max(1) as u64;
                    let avg_tris = acc.triangles_drawn as f32 / frames.max(1) as f32;
                    let avg_pixels = acc.pixels_filled as f32 / frames.max(1) as f32;
                    info!(
                        "display/shader: {:.1} fps total={:.2}ms | \
                         clear_c={:.2} clear_z={:.2} mvp={:.2} vs={:.2} raster={:.2} ms | \
                         flush_cmd={:.2} flush_data={:.2} ms | tris={:.1} pixels={:.0}",
                        fps,
                        (acc_total_us / n) as f32 / 1000.,
                        (acc.clear_color_us / n) as f32 / 1000.,
                        (acc.clear_depth_us / n) as f32 / 1000.,
                        (acc.mvp_us / n) as f32 / 1000.,
                        (acc.vs_us / n) as f32 / 1000.,
                        (acc.raster_us / n) as f32 / 1000.,
                        (acc_flush_cmd_us / n) as f32 / 1000.,
                        (acc_flush_data_us / n) as f32 / 1000.,
                        avg_tris,
                        avg_pixels,
                    );
                    reset_stats!();
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

    // GET /api/display/shader/start — sets the running flag so the render thread produces frames.
    {
        let running_flag = running_start;
        server.fn_handler(
            "/api/display/shader/start",
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
    // GET /api/display/shader/stop — clears the running flag; the thread blanks the display.
    {
        let running_flag = running_stop;
        server.fn_handler(
            "/api/display/shader/stop",
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

    info!("display/shader: endpoints registered");
    Ok(())
}
