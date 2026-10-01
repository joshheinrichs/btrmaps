//! Drawing the map with OpenGL as one virtual texture, the way large maps are drawn.
//!
//! Every tile the window has is a page in one atlas texture, allocated once. A small
//! page table says, for each on-screen page of the shown level, which atlas slot holds
//! it or its nearest ancestor; the shader follows it per pixel, so the map is one draw
//! call and a missing page shows its ancestor scaled up with no overdraw or seams.
//! Pages hold data (set and compression and age codes per pixel); the shader colors
//! them from a lookup table. Pages are sent whole, within a byte budget per frame, into
//! the atlas: no textures are created while drawing.

use crate::palette::GpuColors;
use crate::tiles::{Key, TILE, Tile};
use eframe::egui::{self, Color32};
use eframe::egui_glow::ShaderVersion;
use eframe::glow::{self, HasContext as _};
use std::collections::HashMap;
use std::sync::Arc;

const SIDE: usize = TILE as usize;
/// The atlas is this many pages a side.
const SLOTS: usize = 16;
/// The page table covers this many pages a side of the shown level: more than any
/// screen shows, since its pages are no bigger than two screen pixels.
pub const WINDOW: usize = 64;
/// Bytes sent to the GPU per frame at most; the rest go in the next frames.
const BUDGET: usize = 1 << 20;
/// Width of the set color table; sets fill it row by row.
const TABLE_WIDTH: usize = 4096;

const PRECISION: &str = "
#ifdef GL_ES
    #if defined(GL_FRAGMENT_PRECISION_HIGH) && GL_FRAGMENT_PRECISION_HIGH == 1
        precision highp float;
    #else
        precision mediump float;
    #endif
#endif
";

const VERTEX: &str = "
#if NEW_SHADER_INTERFACE
    #define I in
    #define O out
#else
    #define I attribute
    #define O varying
#endif
uniform vec4 u_corners;
I vec2 a_pos;
O vec2 v_px;
void main() {
    gl_Position = vec4(a_pos, 0.0, 1.0);
    vec2 along = vec2(a_pos.x + 1.0, 1.0 - a_pos.y) * 0.5;
    v_px = mix(u_corners.xy, u_corners.zw, along);
}
";

/// `v_px` is the position in pixels of the shown level, from the page table's corner.
/// A page texel is a packed pixel (`palette::pack`) in RGBA bytes: the set id in RGB,
/// the codes in A. The set's color comes from the table; where the table says
/// transparent, from the ramp entry for the pixel's compression or age code. Pixels of
/// any set but the focused one fade 82% of the way to the background.
///
/// The four pixels around each screen pixel are colored first, then blended (ids can't
/// be blended): only within a screen pixel of a pixel's edge when zoomed in, so blocks
/// stay sharp, and evenly when map pixels are smaller than screen pixels.
const FRAGMENT: &str = "
#if NEW_SHADER_INTERFACE
    #define I in
    out vec4 f_color;
    #define gl_FragColor f_color
    #define texture2D texture
#else
    #define I varying
#endif
uniform sampler2D u_atlas;
uniform sampler2D u_pages;
uniform sampler2D u_table;
uniform sampler2D u_ramp;
uniform vec2 u_origin;
uniform vec2 u_table_size;
uniform float u_age;
uniform float u_focus;
uniform vec3 u_background;
I vec2 v_px;
const float SIDE = 256.0;
const float SLOTS = 16.0;
const float WINDOW = 64.0;
float byte(float v) { return floor(v * 255.0 + 0.5); }
vec4 shade(vec2 px) {
    vec2 page = floor(px / SIDE);
    if (any(lessThan(page, vec2(0.0))) || any(greaterThanEqual(page, vec2(WINDOW)))) {
        return vec4(0.0);
    }
    vec4 entry = texture2D(u_pages, (page + 0.5) / WINDOW);
    if (entry.a < 0.5) {
        return vec4(0.0);
    }
    float k = exp2(byte(entry.b));
    vec2 local = floor((mod(u_origin + page, k) * SIDE + px - page * SIDE) / k);
    vec2 slot = vec2(byte(entry.r), byte(entry.g));
    vec4 t = texture2D(u_atlas, (slot * SIDE + local + 0.5) / (SLOTS * SIDE));
    float set = byte(t.r) + byte(t.g) * 256.0 + byte(t.b) * 65536.0;
    if (set > 16777214.0) {
        return vec4(0.0);
    }
    float column = mod(set, u_table_size.x);
    float row = (set - column) / u_table_size.x;
    vec4 color = texture2D(u_table, (vec2(column, row) + 0.5) / u_table_size);
    if (color.a < 0.5) {
        float code = byte(t.a);
        float index = u_age > 0.5 ? 8.0 + floor(code / 8.0) : mod(code, 8.0);
        color = texture2D(u_ramp, vec2((index + 0.5) / 16.0, 0.5));
    }
    if (u_focus >= 0.0 && abs(set - u_focus) > 0.5) {
        color = vec4(mix(u_background, color.rgb, 0.18), 1.0);
    }
    return color;
}
void main() {
    vec2 p = v_px - 0.5;
    vec2 base = floor(p);
    vec2 edge = clamp((fract(p) - 0.5) / max(fwidth(p), 1e-4) + 0.5, 0.0, 1.0);
    vec4 top = mix(shade(base), shade(base + vec2(1.0, 0.0)), edge.x);
    vec4 bottom = mix(shade(base + vec2(0.0, 1.0)), shade(base + vec2(1.0, 1.0)), edge.x);
    gl_FragColor = mix(top, bottom, edge.y);
}
";

/// What one frame shows: the shown level's on-screen pages (the first, and how many
/// across and down), the tiles at hand at every level, and the colors.
pub struct Scene {
    pub level: u32,
    pub origin: (u64, u64),
    pub pages: (u64, u64),
    pub tiles: Vec<Arc<Tile>>,
    pub colors: Arc<GpuColors>,
}

/// The page table: for each on-screen page of `level`, row by row in a `WINDOW`-wide
/// grid, the atlas slot holding it or its nearest ancestor (column, row), how many
/// levels up that is, and 255 when there is one at all.
pub fn page_table(scene: &Scene, slot_of: impl Fn(Key) -> Option<u16>) -> Vec<u8> {
    let mut table = vec![0u8; WINDOW * WINDOW * 4];
    let (w, h) = (
        scene.pages.0.min(WINDOW as u64),
        scene.pages.1.min(WINDOW as u64),
    );
    for j in 0..h {
        for i in 0..w {
            let (x, y) = (scene.origin.0 + i, scene.origin.1 + j);
            let found = (0..=scene.level).find_map(|up| {
                let key = Key {
                    level: scene.level - up,
                    x: x >> up,
                    y: y >> up,
                };
                slot_of(key).map(|slot| (slot, up))
            });
            if let Some((slot, up)) = found {
                let p = (j as usize * WINDOW + i as usize) * 4;
                let at = slot as usize;
                table[p..p + 4].copy_from_slice(&[
                    (at % SLOTS) as u8,
                    (at / SLOTS) as u8,
                    up as u8,
                    255,
                ]);
            }
        }
    }
    table
}

/// A page in the atlas: its slot, which rendering it holds, and the last frame that
/// needed it.
struct Resident {
    slot: u16,
    id: u64,
    used: u64,
}

/// The set color table on the GPU: rows allocated, sets sent, and for which colors.
struct Table {
    texture: glow::Texture,
    rows: usize,
    sent: usize,
    look: u64,
}

pub struct MapPainter {
    program: glow::Program,
    vao: Option<glow::VertexArray>,
    vbo: glow::Buffer,
    a_pos: u32,
    uniforms: HashMap<&'static str, glow::UniformLocation>,
    atlas: glow::Texture,
    pages: glow::Texture,
    resident: HashMap<Key, Resident>,
    free: Vec<u16>,
    /// The page table as last uploaded.
    table: Vec<u8>,
    colors: Option<Table>,
    ramp: Option<(glow::Texture, u64)>,
    /// This frame's scene, whether it's applied yet, and frames so far.
    scene: Option<Scene>,
    synced: bool,
    frame: u64,
    /// To ask for another frame when uploads are left over.
    ctx: egui::Context,
    /// Time spent painting and pages updated since last asked.
    painting: std::time::Duration,
    uploads: usize,
}

/// A texture of RGBA bytes (uninitialized when `data` is None), nearest-neighbour.
unsafe fn texture(
    gl: &glow::Context,
    w: usize,
    h: usize,
    data: Option<&[u8]>,
) -> Option<glow::Texture> {
    // SAFETY: plain OpenGL calls with egui's context current.
    unsafe {
        let t = gl.create_texture().ok()?;
        gl.bind_texture(glow::TEXTURE_2D, Some(t));
        for (param, value) in [
            (glow::TEXTURE_MIN_FILTER, glow::NEAREST),
            (glow::TEXTURE_MAG_FILTER, glow::NEAREST),
            (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
            (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
        ] {
            gl.tex_parameter_i32(glow::TEXTURE_2D, param, value as i32);
        }
        let (w, h, rgba) = (w as i32, h as i32, glow::RGBA);
        let pixels = glow::PixelUnpackData::Slice(data);
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            rgba as i32,
            w,
            h,
            0,
            rgba,
            glow::UNSIGNED_BYTE,
            pixels,
        );
        Some(t)
    }
}

/// Overwrite a rectangle of a texture with RGBA bytes.
unsafe fn update(gl: &glow::Context, t: glow::Texture, x: usize, y: usize, w: usize, data: &[u8]) {
    let h = data.len() / 4 / w;
    // SAFETY: plain OpenGL calls with egui's context current.
    unsafe {
        gl.bind_texture(glow::TEXTURE_2D, Some(t));
        let pixels = glow::PixelUnpackData::Slice(Some(data));
        let (x, y, w, h) = (x as i32, y as i32, w as i32, h as i32);
        gl.tex_sub_image_2d(
            glow::TEXTURE_2D,
            0,
            x,
            y,
            w,
            h,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            pixels,
        );
    }
}

fn bytes(pixels: impl IntoIterator<Item = u32>) -> Vec<u8> {
    pixels.into_iter().flat_map(u32::to_le_bytes).collect()
}

impl MapPainter {
    pub fn new(gl: &glow::Context, ctx: egui::Context) -> Result<Self, String> {
        let version = ShaderVersion::get(gl);
        // fwidth is core everywhere but OpenGL ES 2, where it is an extension (fragment
        // shaders only), and extensions go before any statement.
        let derivatives = match version {
            ShaderVersion::Es100 => "#extension GL_OES_standard_derivatives : enable\n",
            _ => "",
        };
        let prefix = |extensions: &str| {
            format!(
                "{}#define NEW_SHADER_INTERFACE {}\n{extensions}{PRECISION}",
                version.version_declaration(),
                i32::from(version.is_new_shader_interface())
            )
        };
        // SAFETY: plain OpenGL calls on the context eframe made current for us.
        unsafe {
            let program = gl.create_program()?;
            let mut shaders = Vec::new();
            for (kind, source, extensions) in [
                (glow::VERTEX_SHADER, VERTEX, ""),
                (glow::FRAGMENT_SHADER, FRAGMENT, derivatives),
            ] {
                let shader = gl.create_shader(kind)?;
                gl.shader_source(shader, &format!("{}{source}", prefix(extensions)));
                gl.compile_shader(shader);
                if !gl.get_shader_compile_status(shader) {
                    return Err(gl.get_shader_info_log(shader));
                }
                gl.attach_shader(program, shader);
                shaders.push(shader);
            }
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                return Err(gl.get_program_info_log(program));
            }
            for shader in shaders {
                gl.detach_shader(program, shader);
                gl.delete_shader(shader);
            }
            let a_pos = gl
                .get_attrib_location(program, "a_pos")
                .ok_or("the shader has no a_pos")?;
            let quad: Vec<u8> = [-1.0f32, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0]
                .iter()
                .flat_map(|f| f.to_ne_bytes())
                .collect();
            let vbo = gl.create_buffer()?;
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &quad, glow::STATIC_DRAW);
            let vao = gl.create_vertex_array().ok();
            if let Some(vao) = vao {
                gl.bind_vertex_array(Some(vao));
                gl.enable_vertex_attrib_array(a_pos);
                gl.vertex_attrib_pointer_f32(a_pos, 2, glow::FLOAT, false, 8, 0);
                gl.bind_vertex_array(None);
            }
            gl.bind_buffer(glow::ARRAY_BUFFER, None);
            let names = [
                "u_corners",
                "u_atlas",
                "u_pages",
                "u_table",
                "u_ramp",
                "u_origin",
                "u_table_size",
                "u_age",
                "u_focus",
                "u_background",
            ];
            let uniforms = names
                .into_iter()
                .filter_map(|n| Some((n, gl.get_uniform_location(program, n)?)))
                .collect();
            let atlas = texture(gl, SLOTS * SIDE, SLOTS * SIDE, None).ok_or("no atlas")?;
            let table = vec![0u8; WINDOW * WINDOW * 4];
            let pages = texture(gl, WINDOW, WINDOW, Some(&table)).ok_or("no page table")?;
            Ok(MapPainter {
                program,
                vao,
                vbo,
                a_pos,
                uniforms,
                atlas,
                pages,
                resident: HashMap::new(),
                free: (0..(SLOTS * SLOTS) as u16).rev().collect(),
                table,
                colors: None,
                ramp: None,
                scene: None,
                synced: true,
                frame: 0,
                ctx,
                painting: std::time::Duration::ZERO,
                uploads: 0,
            })
        }
    }

    /// Milliseconds spent painting and pages updated since the last call.
    pub fn take_stats(&mut self) -> (f32, usize) {
        let painting = std::mem::take(&mut self.painting).as_secs_f32() * 1e3;
        (painting, std::mem::take(&mut self.uploads))
    }

    /// What this frame shows.
    pub fn prepare(&mut self, scene: Scene) {
        self.scene = Some(scene);
        self.synced = false;
        self.frame += 1;
    }

    /// Bring the color table and ramp up to date: the table only grows, so only new
    /// rows are sent, unless the colors changed.
    unsafe fn sync_colors(&mut self, gl: &glow::Context, colors: &GpuColors) {
        // SAFETY: plain OpenGL calls inside egui's paint callback.
        unsafe {
            let rows = colors.table.len().max(1).div_ceil(TABLE_WIDTH);
            let row_bytes = |from: usize, to: usize| {
                let pixels = (from * TABLE_WIDTH..to * TABLE_WIDTH)
                    .map(|i| colors.table.get(i).map_or(0, |c| u32::from_le_bytes(*c)));
                bytes(pixels)
            };
            match &mut self.colors {
                Some(t) if t.look == colors.look && rows <= t.rows => {
                    if colors.table.len() > t.sent {
                        let first = t.sent / TABLE_WIDTH;
                        update(
                            gl,
                            t.texture,
                            0,
                            first,
                            TABLE_WIDTH,
                            &row_bytes(first, rows),
                        );
                        t.sent = colors.table.len();
                    }
                }
                _ => {
                    let height = rows.next_power_of_two();
                    if let Some(old) = self.colors.take() {
                        gl.delete_texture(old.texture);
                    }
                    let data = row_bytes(0, height);
                    if let Some(texture) = texture(gl, TABLE_WIDTH, height, Some(&data)) {
                        self.colors = Some(Table {
                            texture,
                            rows: height,
                            sent: colors.table.len(),
                            look: colors.look,
                        });
                    }
                }
            }
            if self.ramp.as_ref().map(|r| r.1) != Some(colors.look) {
                if let Some((old, _)) = self.ramp.take() {
                    gl.delete_texture(old);
                }
                let ramp = bytes(colors.ramp.iter().map(|c| u32::from_le_bytes(*c)));
                self.ramp = texture(gl, 16, 1, Some(&ramp)).map(|t| (t, colors.look));
            }
        }
    }

    /// A free atlas slot, taking the one of the page needed longest ago if none is free;
    /// never one this frame needs.
    fn slot(&mut self) -> Option<u16> {
        if let Some(slot) = self.free.pop() {
            return Some(slot);
        }
        let (&key, _) = self
            .resident
            .iter()
            .filter(|(_, r)| r.used < self.frame)
            .min_by_key(|(_, r)| r.used)?;
        self.resident.remove(&key).map(|r| r.slot)
    }

    /// Once a frame: send the scene's pages the GPU doesn't hold yet (or holds an older
    /// rendering of), within the budget; then the page table if it changed.
    unsafe fn sync(&mut self, gl: &glow::Context, scene: &Scene) {
        self.synced = true;
        let mut sent = 0;
        let mut behind = false;
        for tile in &scene.tiles {
            let slot = match self.resident.get_mut(&tile.key) {
                Some(r) if r.id == tile.id => {
                    r.used = self.frame;
                    continue;
                }
                Some(r) => Some(r.slot),
                None => None,
            };
            if sent + SIDE * SIDE * 4 > BUDGET {
                behind = true;
                continue;
            }
            let Some(slot) = slot.or_else(|| self.slot()) else {
                continue;
            };
            let (x, y) = (
                (slot as usize % SLOTS) * SIDE,
                (slot as usize / SLOTS) * SIDE,
            );
            // SAFETY: plain OpenGL calls inside egui's paint callback.
            unsafe {
                update(
                    gl,
                    self.atlas,
                    x,
                    y,
                    SIDE,
                    &bytes(tile.packed.iter().copied()),
                )
            };
            sent += SIDE * SIDE * 4;
            let resident = Resident {
                slot,
                id: tile.id,
                used: self.frame,
            };
            self.resident.insert(tile.key, resident);
            self.uploads += 1;
        }
        if behind {
            self.ctx.request_repaint();
        }
        let table = page_table(scene, |key| self.resident.get(&key).map(|r| r.slot));
        if table != self.table {
            // SAFETY: as above.
            unsafe { update(gl, self.pages, 0, 0, WINDOW, &table) };
            self.table = table;
        }
    }

    /// Draw the map over the viewport egui set up for this callback. `corners` are its
    /// top left and bottom right in pixels of the shown level, from the page table's
    /// corner.
    pub fn paint(
        &mut self,
        gl: &glow::Context,
        corners: [f32; 4],
        focus: Option<u32>,
        background: Color32,
    ) {
        let start = std::time::Instant::now();
        let Some(scene) = self.scene.take() else {
            return;
        };
        // SAFETY: plain OpenGL calls inside egui's paint callback; egui restores its own
        // state afterwards.
        unsafe {
            if !self.synced {
                self.sync_colors(gl, &scene.colors);
                self.sync(gl, &scene);
            }
            if let (Some(table), Some((ramp, _))) = (&self.colors, &self.ramp) {
                let (table, rows, ramp) = (table.texture, table.rows, *ramp);
                gl.use_program(Some(self.program));
                let units = [
                    (0, self.atlas, "u_atlas"),
                    (1, self.pages, "u_pages"),
                    (2, table, "u_table"),
                    (3, ramp, "u_ramp"),
                ];
                for (unit, t, name) in units {
                    gl.active_texture(glow::TEXTURE0 + unit);
                    gl.bind_texture(glow::TEXTURE_2D, Some(t));
                    gl.uniform_1_i32(self.uniforms.get(name), unit as i32);
                }
                gl.active_texture(glow::TEXTURE0);
                let u = |name| self.uniforms.get(name);
                let [x0, y0, x1, y1] = corners;
                gl.uniform_4_f32(u("u_corners"), x0, y0, x1, y1);
                let (ox, oy) = (scene.origin.0 as f32, scene.origin.1 as f32);
                gl.uniform_2_f32(u("u_origin"), ox, oy);
                gl.uniform_2_f32(u("u_table_size"), TABLE_WIDTH as f32, rows as f32);
                gl.uniform_1_f32(u("u_age"), f32::from(u8::from(scene.colors.age)));
                gl.uniform_1_f32(u("u_focus"), focus.map_or(-1.0, |f| f as f32));
                let bg = background.to_array().map(|x| x as f32 / 255.0);
                gl.uniform_3_f32(u("u_background"), bg[0], bg[1], bg[2]);
                match self.vao {
                    Some(vao) => gl.bind_vertex_array(Some(vao)),
                    None => {
                        gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
                        gl.enable_vertex_attrib_array(self.a_pos);
                        gl.vertex_attrib_pointer_f32(self.a_pos, 2, glow::FLOAT, false, 8, 0);
                    }
                }
                gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                if self.vao.is_some() {
                    gl.bind_vertex_array(None);
                }
            }
        }
        self.painting += start.elapsed();
    }

    pub fn destroy(&self, gl: &glow::Context) {
        // SAFETY: deleting objects this painter made, once, on exit.
        unsafe {
            gl.delete_program(self.program);
            gl.delete_buffer(self.vbo);
            if let Some(vao) = self.vao {
                gl.delete_vertex_array(vao);
            }
            gl.delete_texture(self.atlas);
            gl.delete_texture(self.pages);
            if let Some(t) = &self.colors {
                gl.delete_texture(t.texture);
            }
            if let Some((t, _)) = self.ramp {
                gl.delete_texture(t);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::GpuColors;

    #[test]
    fn missing_pages_fall_back_to_their_nearest_resident_ancestor() {
        let key = |level, x, y| Key { level, x, y };
        let scene = Scene {
            level: 2,
            origin: (1, 1),
            pages: (2, 1),
            tiles: Vec::new(),
            colors: Arc::new(GpuColors::default()),
        };
        // Page (1, 1) of level 2 is in; (2, 1) isn't, nor is its parent (1, 0) of
        // level 1, but the top page is.
        let resident = HashMap::from([(key(2, 1, 1), 7u16), (key(0, 0, 0), 3)]);
        let table = page_table(&scene, |k| resident.get(&k).copied());
        assert_eq!(table[0..4], [7, 0, 0, 255], "its own page");
        assert_eq!(table[4..8], [3, 0, 2, 255], "two levels up");
        assert_eq!(table[8..12], [0, 0, 0, 0], "off screen");
        assert_eq!(table[WINDOW * 4..WINDOW * 4 + 4], [0, 0, 0, 0], "row below");
        let slot = 37u16;
        let resident = HashMap::from([(key(2, 1, 1), slot)]);
        let table = page_table(&scene, |k| resident.get(&k).copied());
        assert_eq!(table[0..4], [5, 2, 0, 255], "slot 37 is column 5, row 2");
    }
}
