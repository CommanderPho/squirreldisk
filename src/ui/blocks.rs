//! 3-D blocks chart style ("file city").
//!
//! Reuses the squarified treemap layout for the XZ footprint; the Y axis
//! encodes file size as block height.  Everything is perspective-projected on
//! the CPU and rendered through a dedicated OpenGL shader that adds ambient
//! occlusion, hover highlight, and collector hatching.

use super::app::{App, CtxMenu, Target};
use super::sunburst::{group_key, view_children, SegKind, View, FREE_KEY};
use super::theme::{lerp_color, with_alpha, Theme};
use super::treemap::{self, Tile};
use super::view::node_label;
use super::widgets::{self, bold, cr, font, BtnStyle, Icon};
use crate::i18n::{tr, trf};
use crate::sound::Sfx;
use crate::tree::{fmt_count, fmt_size, Kind, Tree, NONE};
use eframe::egui::{self, Align2, Color32, Id, Pos2, Rect, Sense, Stroke, Ui, Vec2};
use eframe::glow::{self, HasContext};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;


// -----------------------------------------------------------------------
// 3-D projection helpers
// -----------------------------------------------------------------------

/// Simple camera for isometric-ish perspective over the block city.
#[derive(Clone, Copy)]
struct Camera {
    /// Viewport in screen coords.
    viewport: Rect,
    /// Pitch angle (radians from horizontal, looking down).
    pitch: f32,
    /// Yaw angle (radians, 0 = looking along +X).
    yaw: f32,
    /// Distance from the centre of the scene.
    distance: f32,
}

impl Camera {
    fn new(viewport: Rect) -> Self {
        Camera {
            viewport,
            pitch: 0.82,   // ~47° tilt
            yaw: 0.42,     // slight rotation so left and front faces are visible
            distance: 1.8,
        }
    }

    /// Projects a 3-D point (x,y,z ∈ [0,1]) to screen coords.
    /// Returns None when behind the camera.
    fn project(&self, x: f32, y: f32, z: f32) -> Option<Pos2> {
        // centre the scene at origin
        let (cx, cy, cz) = (x - 0.5, y, z - 0.5);

        // rotate around Y (yaw)
        let (sy, cy_) = self.yaw.sin_cos();
        let (rx, rz) = (cx * cy_ - cz * sy, cx * sy + cz * cy_);

        // rotate around X (pitch)
        let (sp, cp) = self.pitch.sin_cos();
        let (ry, rz2) = (cy * cp - rz * sp, cy * sp + rz * cp);

        // perspective
        let eye_z = self.distance;
        let dz = rz2 + eye_z;
        if dz < 0.05 {
            return None;
        }
        let scale = eye_z / dz;
        let sx = rx * scale;
        let sy_ = -ry * scale; // flip Y for screen

        // map to viewport
        let vw = self.viewport.width();
        let vh = self.viewport.height();
        let s = vw.min(vh) * 1.1;
        Some(Pos2::new(
            self.viewport.center().x + sx * s,
            self.viewport.center().y + sy_ * s + vh * 0.10,
        ))
    }
}


// -----------------------------------------------------------------------
// Block data
// -----------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Block {
    pub key: u64,
    pub node: u32,
    pub kind: SegKind,
    /// Footprint in normalised [0,1]×[0,1] space.
    pub x0: f32,
    pub z0: f32,
    pub x1: f32,
    pub z1: f32,
    /// Height [0,1].
    pub height: f32,
    pub color: Color32,
    pub size: u64,
    pub depth: u8,
}

/// Maximum number of blocks before we stop adding more.
const MAX_BLOCKS: usize = 4000;

/// Height exponent – sqrt gives a less extreme range.
const HEIGHT_EXP: f32 = 0.45;


// -----------------------------------------------------------------------
// Layout: squarified treemap → normalised blocks
// -----------------------------------------------------------------------

pub fn layout(tree: &Tree, view: View, depth: usize, theme: &Theme, free: u64) -> Vec<Block> {
    // Reuse the treemap layout with a unit-square area.
    let unit = Rect::from_min_size(Pos2::ZERO, Vec2::new(1.0, 1.0));
    let tiles = treemap::layout(tree, view, unit, depth, theme, free);
    // The largest size drives height normalisation.
    let max_size = tiles.iter().filter(|t| t.depth > 0 && !t.nested).map(|t| t.size).max().unwrap_or(1).max(1);

    let mut blocks = Vec::with_capacity(tiles.len());
    for t in &tiles {
        if t.depth == 0 {
            continue; // skip the root tile
        }
        if t.nested {
            continue; // folder frame — skip in 3D, children handle it
        }
        let h = (t.size as f64 / max_size as f64).powf(HEIGHT_EXP as f64) as f32;
        let h = h.clamp(0.02, 1.0);
        blocks.push(Block {
            key: t.key,
            node: t.node,
            kind: t.kind,
            x0: t.rect.left(),
            z0: t.rect.top(),
            x1: t.rect.right(),
            z1: t.rect.bottom(),
            height: h,
            color: t.color,
            size: t.size,
            depth: t.depth,
        });
        if blocks.len() >= MAX_BLOCKS {
            break;
        }
    }
    ## END for t in &tiles...

    blocks
}


// -----------------------------------------------------------------------
// Animation state
// -----------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct BlockAnim {
    pub block: Block,
    // current (animated) values
    x0: f32,
    z0: f32,
    x1: f32,
    z1: f32,
    height: f32,
    color: Color32,
    alpha: f32,
    hover: f32,
    alive: bool,
}

#[derive(Default)]
pub struct BlocksState {
    pub anims: HashMap<u64, BlockAnim>,
    order: Vec<u64>,
    pub sig: Option<(u64, View, usize, [i32; 4], String)>,
    pub last_layout: Option<Instant>,
    pub settled: bool,
}

impl BlocksState {
    pub fn set_target(&mut self, blocks: Vec<Block>) {
        for a in self.anims.values_mut() {
            a.alive = false;
        }
        self.order.clear();
        for b in blocks {
            self.order.push(b.key);
            match self.anims.get_mut(&b.key) {
                Some(a) => {
                    a.alive = true;
                    a.block = b;
                }
                None => {
                    // new blocks grow upwards from zero height
                    self.anims.insert(b.key, BlockAnim {
                        x0: b.x0, z0: b.z0, x1: b.x1, z1: b.z1,
                        height: 0.0,
                        color: b.color,
                        alpha: 0.0,
                        hover: 0.0,
                        alive: true,
                        block: b,
                    });
                }
            }
        }
        self.settled = false;
    }


    pub fn step(&mut self, dt: f32, hovered: Option<u64>) {
        let k = 1.0 - (-dt * 10.0).exp();
        let kh = 1.0 - (-dt * 16.0).exp();
        let mut moving = false;
        self.anims.retain(|_, a| a.alive || a.alpha > 0.01);
        for a in self.anims.values_mut() {
            let ht = if Some(a.block.key) == hovered { 1.0 } else { 0.0 };
            a.hover += (ht - a.hover) * kh;
            if a.alive {
                a.x0 += (a.block.x0 - a.x0) * k;
                a.z0 += (a.block.z0 - a.z0) * k;
                a.x1 += (a.block.x1 - a.x1) * k;
                a.z1 += (a.block.z1 - a.z1) * k;
                a.height += (a.block.height - a.height) * k;
                a.color = lerp_color(a.color, a.block.color, k);
                a.alpha += (1.0 - a.alpha) * k;
                if (a.x0 - a.block.x0).abs() > 0.001
                    || (a.height - a.block.height).abs() > 0.001
                    || a.alpha < 0.99
                {
                    moving = true;
                }
            } else {
                a.alpha = (a.alpha - a.alpha * k * 1.5 - dt).max(0.0);
                moving = true;
            }
            if (a.hover - ht).abs() > 0.01 {
                moving = true;
            }
        }
        ## END for a in self.anims.values_mut()...

        self.settled = !moving;
    }


    /// Current colour of a block (used by the side list).
    pub fn color_of(&self, key: u64) -> Option<Color32> {
        self.anims.get(&key).filter(|a| a.alive).map(|a| a.block.color)
    }


    /// Centre of a block's top face projected to the viewport (for demo director).
    pub fn center_of(&self, key: u64, cam: &Camera) -> Option<Pos2> {
        self.anims.get(&key).filter(|a| a.alive).and_then(|a| {
            let cx = (a.x0 + a.x1) * 0.5;
            let cz = (a.z0 + a.z1) * 0.5;
            cam.project(cx, a.height * 0.35, cz)
        })
    }


    fn draw_list(&self) -> Vec<&BlockAnim> {
        let mut v: Vec<&BlockAnim> = self.anims.values().filter(|a| !a.alive && a.alpha > 0.01).collect();
        v.extend(self.order.iter().filter_map(|k| self.anims.get(k)));
        v
    }


    /// Deepest block under screen position `p`, using projection.
    fn hit(&self, p: Pos2, cam: &Camera) -> Option<&BlockAnim> {
        // Project all top-face quads and test containment, back-to-front.
        // For correctness under perspective, we project the 4 top corners
        // and do a 2D point-in-quad test.
        let mut best: Option<&BlockAnim> = None;
        for a in self.anims.values() {
            if !a.alive || a.alpha < 0.3 {
                continue;
            }
            let h = a.height * 0.35;
            let corners = [
                cam.project(a.x0, h, a.z0),
                cam.project(a.x1, h, a.z0),
                cam.project(a.x1, h, a.z1),
                cam.project(a.x0, h, a.z1),
            ];
            if let [Some(c0), Some(c1), Some(c2), Some(c3)] = corners {
                if point_in_quad(p, c0, c1, c2, c3) {
                    // prefer deeper / smaller blocks (higher depth, or closer to camera)
                    match best {
                        Some(b) if b.block.depth >= a.block.depth => {}
                        _ => best = Some(a),
                    }
                }
            }
        }
        best
    }
}

fn point_in_quad(p: Pos2, a: Pos2, b: Pos2, c: Pos2, d: Pos2) -> bool {
    point_in_tri(p, a, b, c) || point_in_tri(p, a, c, d)
}

fn point_in_tri(p: Pos2, a: Pos2, b: Pos2, c: Pos2) -> bool {
    let cross = |o: Pos2, u: Pos2, v: Pos2| (u.x - o.x) * (v.y - o.y) - (u.y - o.y) * (v.x - o.x);
    let d1 = cross(p, a, b);
    let d2 = cross(p, b, c);
    let d3 = cross(p, c, a);
    let has_neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let has_pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(has_neg && has_pos)
}


// -----------------------------------------------------------------------
// OpenGL renderer
// -----------------------------------------------------------------------

const BLOCK_VS: &str = r#"
#ifdef NEW_SHADER_INTERFACE
  #define I in
  #define O out
#else
  #define I attribute
  #define O varying
#endif
uniform vec2 u_screen;
I vec2 a_pos;
I vec4 a_color;
I vec3 a_normal;
I vec3 a_params;  // hover, flags, alpha
O vec4 v_color;
O vec3 v_normal;
O vec3 v_params;
void main() {
    v_color = a_color;
    v_normal = a_normal;
    v_params = a_params;
    gl_Position = vec4(2.0 * a_pos.x / u_screen.x - 1.0, 1.0 - 2.0 * a_pos.y / u_screen.y, 0.0, 1.0);
}
"#;

const BLOCK_FS: &str = r#"
#ifdef GL_ES
precision highp float;
#endif
#ifdef NEW_SHADER_INTERFACE
  #define I in
  out vec4 f_color;
  #define FRAG f_color
#else
  #define I varying
  #define FRAG gl_FragColor
#endif
uniform float u_time;
uniform float u_ppp;
I vec4 v_color;
I vec3 v_normal;
I vec3 v_params;

bool has_flag(float flags, float f) {
    return mod(floor(flags / f), 2.0) > 0.5;
}

void main() {
    float hover = v_params.x;
    float flags = v_params.y;
    float alpha = v_params.z;

    // simple directional + ambient lighting
    vec3 light = normalize(vec3(-0.4, 0.7, -0.5));
    float diff = max(dot(v_normal, light), 0.0);
    float ambient = 0.45;
    float lit = ambient + (1.0 - ambient) * diff;

    vec3 c = v_color.rgb * lit;

    // hover
    c = mix(c, vec3(1.0), 0.22 * hover);

    // collector hatch
    if (has_flag(flags, 1.0)) {
        float g = dot(c, vec3(0.299, 0.587, 0.114));
        c = mix(c, vec3(g), 0.75) * 0.7;
        float stripe = step(0.5, fract((gl_FragCoord.x + gl_FragCoord.y) / (9.0 * u_ppp) - u_time * 0.4));
        c = mix(c, c * 1.35, stripe * 0.5);
    }
    // dragged ghost
    if (has_flag(flags, 2.0)) {
        c *= 0.55;
    }
    // edge darkening (AO-like)
    float edge = 1.0 - smoothstep(0.0, 2.0 / u_ppp, min(gl_FragCoord.x, gl_FragCoord.y));
    c *= 1.0 - 0.15 * edge;

    FRAG = vec4(clamp(c, 0.0, 1.0) * alpha, alpha);
}
"#;

/// Per-vertex layout: pos(2) + color(4) + normal(3) + params(3) = 12 floats
const BV_FLOATS: usize = 12;

pub struct BlocksGl {
    program: glow::Program,
    vbo: glow::Buffer,
    ebo: glow::Buffer,
    vao: glow::VertexArray,
    u_screen: Option<glow::UniformLocation>,
    u_time: Option<glow::UniformLocation>,
    u_ppp: Option<glow::UniformLocation>,
}

impl BlocksGl {
    pub fn new(gl: &glow::Context) -> Result<Self, String> {
        let sv = egui_glow::ShaderVersion::get(gl);
        let header = format!(
            "{}\n{}",
            sv.version_declaration(),
            if sv.is_new_shader_interface() { "#define NEW_SHADER_INTERFACE\n" } else { "" }
        );
        unsafe {
            let program = gl.create_program()?;
            let mut shaders = Vec::new();
            for (ty, src) in [(glow::VERTEX_SHADER, BLOCK_VS), (glow::FRAGMENT_SHADER, BLOCK_FS)] {
                let s = gl.create_shader(ty)?;
                gl.shader_source(s, &format!("{header}{src}"));
                gl.compile_shader(s);
                if !gl.get_shader_compile_status(s) {
                    return Err(format!("blocks shader compile: {}", gl.get_shader_info_log(s)));
                }
                gl.attach_shader(program, s);
                shaders.push(s);
            }
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                return Err(format!("blocks shader link: {}", gl.get_program_info_log(program)));
            }
            for s in shaders {
                gl.detach_shader(program, s);
                gl.delete_shader(s);
            }
            let vbo = gl.create_buffer()?;
            let ebo = gl.create_buffer()?;
            let vao = gl.create_vertex_array()?;
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            let stride = (BV_FLOATS * 4) as i32;
            for (name, size, offset) in [("a_pos", 2, 0), ("a_color", 4, 2), ("a_normal", 3, 6), ("a_params", 3, 9)] {
                if let Some(loc) = gl.get_attrib_location(program, name) {
                    gl.enable_vertex_attrib_array(loc);
                    gl.vertex_attrib_pointer_f32(loc, size, glow::FLOAT, false, stride, offset * 4);
                }
            }
            gl.bind_vertex_array(None);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);
            Ok(BlocksGl {
                u_screen: gl.get_uniform_location(program, "u_screen"),
                u_time: gl.get_uniform_location(program, "u_time"),
                u_ppp: gl.get_uniform_location(program, "u_ppp"),
                program,
                vbo,
                ebo,
                vao,
            })
        }
    }


    pub fn paint(&self, gl: &glow::Context, verts: &[f32], indices: &[u32], screen: [f32; 2], time: f32, ppp: f32) {
        if indices.is_empty() {
            return;
        }
        unsafe {
            gl.use_program(Some(self.program));
            gl.uniform_2_f32(self.u_screen.as_ref(), screen[0], screen[1]);
            gl.uniform_1_f32(self.u_time.as_ref(), time);
            gl.uniform_1_f32(self.u_ppp.as_ref(), ppp);
            gl.bind_vertex_array(Some(self.vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytemuck_f32(verts), glow::STREAM_DRAW);
            gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(self.ebo));
            gl.buffer_data_u8_slice(glow::ELEMENT_ARRAY_BUFFER, bytemuck_u32(indices), glow::STREAM_DRAW);
            gl.enable(glow::BLEND);
            gl.blend_equation(glow::FUNC_ADD);
            gl.blend_func_separate(glow::ONE, glow::ONE_MINUS_SRC_ALPHA, glow::ONE_MINUS_DST_ALPHA, glow::ONE);
            gl.draw_elements(glow::TRIANGLES, indices.len() as i32, glow::UNSIGNED_INT, 0);
            gl.bind_vertex_array(None);
            gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, None);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);
        }
    }


    pub fn destroy(&self, gl: &glow::Context) {
        unsafe {
            gl.delete_program(self.program);
            gl.delete_buffer(self.vbo);
            gl.delete_buffer(self.ebo);
            gl.delete_vertex_array(self.vao);
        }
    }
}

fn bytemuck_f32(v: &[f32]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}
fn bytemuck_u32(v: &[u32]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

pub struct BlocksFrame {
    pub vertices: Vec<f32>,
    pub indices: Vec<u32>,
    pub screen: [f32; 2],
    pub time: f32,
}

pub fn blocks_callback(renderer: Arc<std::sync::Mutex<BlocksGl>>, frame: Arc<BlocksFrame>, rect: Rect) -> egui::PaintCallback {
    egui::PaintCallback {
        rect,
        callback: Arc::new(egui_glow::CallbackFn::new(move |info, painter| {
            if let Ok(r) = renderer.lock() {
                r.paint(painter.gl(), &frame.vertices, &frame.indices, frame.screen, frame.time, info.pixels_per_point);
            }
        })),
    }
}


// -----------------------------------------------------------------------
// Mesh builder: 3-D boxes → projected 2-D triangles
// -----------------------------------------------------------------------

const FLAG_COLLECTED: f32 = 1.0;
const FLAG_DRAGGED: f32 = 2.0;

/// Builds the projected mesh for all blocks.
fn build_block_mesh(anims: &[&BlockAnim], cam: &Camera, tree: &Tree, root: u32, collector: &[u32], dragging_node: Option<u32>, scanning: bool, verts: &mut Vec<f32>, indices: &mut Vec<u32>) {
    verts.clear();
    indices.clear();

    // Sort back-to-front for correct blending under perspective.
    // Approximate depth by the block's centre distance from camera.
    let mut sorted: Vec<(usize, f32)> = anims.iter().enumerate().map(|(i, a)| {
        let cx = (a.x0 + a.x1) * 0.5 - 0.5;
        let cz = (a.z0 + a.z1) * 0.5 - 0.5;
        let (sy, cy_) = cam.yaw.sin_cos();
        let rz = cx * sy + cz * cy_;
        (i, rz)
    }).collect();
    sorted.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    for &(idx, _) in &sorted {
        let a = anims[idx];
        if a.alpha < 0.01 {
            continue;
        }

        let col = a.color;
        let c = [col.r() as f32 / 255.0, col.g() as f32 / 255.0, col.b() as f32 / 255.0, 1.0];
        let h = a.height * 0.35; // scale height into scene range
        let pad = 0.003; // small gap between blocks

        let x0 = a.x0 + pad;
        let x1 = a.x1 - pad;
        let z0 = a.z0 + pad;
        let z1 = a.z1 - pad;
        if x1 <= x0 || z1 <= z0 {
            continue;
        }

        let mut flags = 0.0f32;
        if let SegKind::Node(k) = a.block.kind {
            if a.block.node != root && collector.iter().any(|c_| *c_ == a.block.node || tree.is_ancestor(*c_, a.block.node)) {
                flags += FLAG_COLLECTED;
            }
            if Some(a.block.node) == dragging_node {
                flags += FLAG_DRAGGED;
            }
        }
        let params = [a.hover, flags, a.alpha];

        // The 3 visible faces of a box under our camera angle:
        //   top (y=h), front (z=z1), right (x=x1)
        // We emit the faces whose normals face the camera.
        let faces: [([(f32, f32, f32); 4], [f32; 3]); 5] = [
            // top
            ([(x0, h, z0), (x1, h, z0), (x1, h, z1), (x0, h, z1)], [0.0, 1.0, 0.0]),
            // front (z=z1, normal +Z)
            ([(x0, 0.0, z1), (x1, 0.0, z1), (x1, h, z1), (x0, h, z1)], [0.0, 0.0, 1.0]),
            // back (z=z0, normal -Z)
            ([(x1, 0.0, z0), (x0, 0.0, z0), (x0, h, z0), (x1, h, z0)], [0.0, 0.0, -1.0]),
            // right (x=x1, normal +X)
            ([(x1, 0.0, z0), (x1, 0.0, z1), (x1, h, z1), (x1, h, z0)], [1.0, 0.0, 0.0]),
            // left (x=x0, normal -X)
            ([(x0, 0.0, z1), (x0, 0.0, z0), (x0, h, z0), (x0, h, z1)], [-1.0, 0.0, 0.0]),
        ];

        for (quad, normal) in &faces {
            let projected: Vec<Option<Pos2>> = quad.iter().map(|&(px, py, pz)| cam.project(px, py, pz)).collect();
            // skip faces where any vertex is behind the camera
            if projected.iter().any(|p| p.is_none()) {
                continue;
            }
            let pts: Vec<Pos2> = projected.into_iter().flatten().collect();

            // back-face culling: skip if the projected quad winds clockwise (facing away)
            let cross = (pts[1].x - pts[0].x) * (pts[2].y - pts[0].y) - (pts[1].y - pts[0].y) * (pts[2].x - pts[0].x);
            if cross >= 0.0 {
                continue;
            }

            // Darken side faces slightly for visual depth
            let face_dim = if normal[1] > 0.5 { 1.0 } else { 0.78 };
            let fc = [c[0] * face_dim, c[1] * face_dim, c[2] * face_dim, c[3]];

            let base = (verts.len() / BV_FLOATS) as u32;
            for pt in &pts {
                verts.extend_from_slice(&[pt.x, pt.y, fc[0], fc[1], fc[2], fc[3], normal[0], normal[1], normal[2], params[0], params[1], params[2]]);
            }
            ## END for pt in &pts...

            indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        ## END for (quad, normal) in &faces...
    }
    ## END for &(idx, _) in &sorted...
}


// -----------------------------------------------------------------------
// CPU fallback: flat-shaded quads via egui painter
// -----------------------------------------------------------------------

fn cpu_blocks(painter: &egui::Painter, verts: &[f32], indices: &[u32]) {
    use egui::{epaint, Mesh, Shape};
    let mut mesh = Mesh::default();
    let nv = verts.len() / BV_FLOATS;
    for i in 0..nv {
        let off = i * BV_FLOATS;
        let alpha = verts[off + 11];
        let c = Color32::from_rgba_premultiplied(
            (verts[off + 2] * alpha * 255.0) as u8,
            (verts[off + 3] * alpha * 255.0) as u8,
            (verts[off + 4] * alpha * 255.0) as u8,
            (alpha * 255.0) as u8,
        );
        mesh.vertices.push(epaint::Vertex { pos: Pos2::new(verts[off], verts[off + 1]), uv: egui::epaint::WHITE_UV, color: c });
    }
    ## END for i in 0..nv...

    for idx in indices {
        mesh.indices.push(*idx);
    }
    ## END for idx in indices...

    painter.add(Shape::mesh(mesh));
}


// -----------------------------------------------------------------------
// UI integration
// -----------------------------------------------------------------------

impl App {
    pub fn blocks_ui(&mut self, ui: &mut Ui, chart: Rect, panel: Rect, si: usize) {
        let theme = self.theme.clone();
        let depth = self.settings.rings.clamp(3, 9);
        let area = chart.shrink2(Vec2::new(12.0, 10.0));
        let modal_open = self.modal.is_some() || self.ctx_menu.is_some();
        let dragging_node = self.drag.as_ref().filter(|d| d.session == si).map(|d| d.node);
        let cam = Camera::new(area);

        // free space handling (same as treemap)
        {
            let s = &mut self.sessions[si];
            if let Target::Volume(v) = &s.target {
                if s.free_at.elapsed().as_secs_f32() > 2.0 {
                    s.free_space = if crate::scan::demo::enabled() {
                        v.available
                    } else {
                        crate::scan::platform::volume_space(std::path::Path::new(&v.mount)).map(|x| x.1).unwrap_or(v.available)
                    };
                    s.free_at = Instant::now();
                }
            }
        }

        // layout (throttled while scanning)
        {
            let s = &mut self.sessions[si];
            let t = s.tree.read().unwrap();
            let free = if s.view == View::node(t.root) && matches!(s.target, Target::Volume(_)) { s.free_space } else { 0 };
            let r = [area.left() as i32, area.top() as i32, area.width() as i32, area.height() as i32];
            let sig = (t.version ^ (free >> 20).rotate_left(40), s.view, depth, r, theme.name.to_string());
            let changed_view = s.blocks.sig.as_ref().is_none_or(|old| old.1 != sig.1 || old.3 != sig.3 || old.4 != sig.4 || old.2 != sig.2);
            let interval = if s.is_scanning() { 160 } else { 30 };
            let due = s.blocks.last_layout.is_none_or(|l| l.elapsed().as_millis() > interval);
            if s.blocks.sig.as_ref() != Some(&sig) && (changed_view || due) {
                let blocks = layout(&t, s.view, depth, &theme, free);
                drop(t);
                s.blocks.set_target(blocks);
                s.blocks.sig = Some(sig);
                s.blocks.last_layout = Some(Instant::now());
            }
        }

        // interaction
        let resp = ui.interact(chart, Id::new(("blocks", si)), Sense::click_and_drag());
        let pointer = ui.ctx().input(|i| i.pointer.latest_pos());
        let hit = if !modal_open && self.drag.is_none() {
            pointer.filter(|p| area.contains(*p)).and_then(|p| self.sessions[si].blocks.hit(*p, &cam).map(|a| a.block.clone()))
        } else {
            None
        };
        {
            let s = &mut self.sessions[si];
            s.hovered_key = hit.as_ref().map(|b| b.key).or(s.list_hover.map(|n| n as u64));
            let in_panel = pointer.is_some_and(|p| panel.contains(p));
            if let Some(b) = &hit {
                s.panel_view = match b.kind {
                    SegKind::Node(Kind::Dir | Kind::Hidden) if s.can_enter(b.node) => View::node(b.node),
                    SegKind::Group { parent, skip, .. } => View { node: parent, skip },
                    SegKind::Free => s.view,
                    _ => {
                        let tr_ = s.tree.read().unwrap();
                        if tr_.is_alive(b.node) { View::node(tr_.get(b.node).parent) } else { s.view }
                    }
                };
                s.selected = if b.kind != SegKind::Free { Some(b.node) } else { None };
            } else if !in_panel && self.drag.is_none() {
                s.panel_view = s.view;
                s.selected = None;
            }
        }
        if hit.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.clicked() {
            if let Some(b) = hit.clone() {
                match b.kind {
                    SegKind::Group { parent, skip, .. } => {
                        self.sessions[si].navigate(View { node: parent, skip });
                        self.sfx(Sfx::Blip);
                    }
                    SegKind::Node(Kind::Dir | Kind::Hidden) if self.sessions[si].can_enter(b.node) => {
                        self.sessions[si].navigate(View::node(b.node));
                        self.sfx(Sfx::Blip);
                    }
                    SegKind::Node(Kind::SmallFiles) => self.expand(si, b.node),
                    _ => {}
                }
            } else if pointer.is_some_and(|p| area.contains(p)) && self.sessions[si].go_up() {
                self.sfx(Sfx::BlipDown);
            }
        }
        if resp.double_clicked() {
            if let Some(b) = &hit {
                if matches!(b.kind, SegKind::Node(Kind::File)) && self.sessions[si].source().is_local() {
                    let path = self.sessions[si].tree.read().unwrap().path(b.node);
                    super::app::os_open(&path, false);
                }
            }
        }
        if resp.secondary_clicked() {
            if let (Some(b), Some(p)) = (&hit, pointer) {
                if matches!(b.kind, SegKind::Node(_)) {
                    self.ctx_menu = Some(CtxMenu { session: si, node: b.node, pos: p, opened: Instant::now() });
                }
            }
        }
        if resp.drag_started() && self.drag.is_none() {
            if let Some(o) = ui.ctx().input(|i| i.pointer.press_origin()) {
                let b = self.sessions[si].blocks.hit(o, &cam).map(|a| (a.block.clone(), a.color));
                if let Some((b, color)) = b {
                    if let SegKind::Node(k) = b.kind {
                        if !matches!(k, Kind::Hidden | Kind::Mount) {
                            self.begin_drag(si, b.node, color, o);
                        }
                    }
                }
            }
        }
        if !modal_open && self.drag.is_none() && ui.ctx().input(|i| i.key_pressed(egui::Key::Delete)) {
            if let Some(b) = &hit {
                if self.sessions[si].collect(b.node) {
                    self.sfx(Sfx::Plop);
                    self.bin_bounce = 1.0;
                }
            }
        }

        // animate + draw
        let dt = self.dt;
        let s = &mut self.sessions[si];
        let hk = s.hovered_key;
        s.blocks.step(dt, hk);
        let t = s.tree.read().unwrap();
        let root = t.root;
        let collector = s.collector.clone();
        let draw = s.blocks.draw_list();

        // build projected mesh
        let mut block_verts = std::mem::take(&mut s.block_verts);
        let mut block_idx = std::mem::take(&mut s.block_idx);
        build_block_mesh(&draw, &cam, &t, root, &collector, dragging_node, s.is_scanning(), &mut block_verts, &mut block_idx);
        drop(t);

        // render
        let screen = ui.ctx().content_rect();
        if let Some(gl) = &self.blocks_gl {
            let frame = BlocksFrame {
                vertices: block_verts.clone(),
                indices: block_idx.clone(),
                screen: [screen.width(), screen.height()],
                time: self.time as f32,
            };
            ui.painter().with_clip_rect(chart.expand(30.0)).add(blocks_callback(gl.clone(), Arc::new(frame), screen));
        } else {
            cpu_blocks(ui.painter(), &block_verts, &block_idx);
        }

        let s = &mut self.sessions[si];
        s.block_verts = block_verts;
        s.block_idx = block_idx;

        // ground plane shadow
        {
            let p = ui.painter();
            // project ground corners to give a floor reference
            if let (Some(a), Some(b), Some(c), Some(d)) = (cam.project(0.0, 0.0, 0.0), cam.project(1.0, 0.0, 0.0), cam.project(1.0, 0.0, 1.0), cam.project(0.0, 0.0, 1.0)) {
                let pts = vec![a, b, c, d];
                p.add(egui::Shape::convex_polygon(pts, with_alpha(theme.bg_bottom, 0.35), Stroke::NONE));
            }
        }

        // scanning status
        let scanning = self.sessions[si].is_scanning();
        let empty = self.sessions[si].blocks.anims.is_empty();
        if empty {
            let msg = if scanning { tr("Scanning…") } else { tr("Empty") };
            ui.painter().text(area.center(), Align2::CENTER_CENTER, msg, font(13.0), theme.text_faint);
        }
        if scanning {
            let p = ui.painter().clone();
            self.scan_pill(&p, chart, si);
        }
    }
}

