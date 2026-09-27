//! The GL state of a guest context (host object ids), and the ops that move
//! the WebGL2 context from one guest context's state to another's. Every
//! guest context runs on the one WebGL2 context of the executor (WebGL has
//! no shared contexts, ADR 0036); object state (vertex arrays, textures,
//! programs) lives in the objects themselves, so only this context-level
//! vector is switched.

use crate::exec::{Code, OpBuf, fw};

/// Capabilities of glEnable/glDisable, in [`GlState::enabled`] order.
pub const CAPS: [u32; 11] = [
    0x0BE2, // BLEND
    0x0B44, // CULL_FACE
    0x0B71, // DEPTH_TEST
    0x0BD0, // DITHER
    0x8037, // POLYGON_OFFSET_FILL
    0x8D69, // PRIMITIVE_RESTART_FIXED_INDEX
    0x8C89, // RASTERIZER_DISCARD
    0x809E, // SAMPLE_ALPHA_TO_COVERAGE
    0x80A0, // SAMPLE_COVERAGE
    0x0C11, // SCISSOR_TEST
    0x0B90, // STENCIL_TEST
];

pub fn cap_index(cap: u32) -> Option<usize> {
    CAPS.iter().position(|&c| c == cap)
}

/// Pixel store parameters, in [`GlState::pixel_store`] order.
pub const PIXEL_STORE: [u32; 10] = [
    0x0CF5, // UNPACK_ALIGNMENT
    0x0D05, // PACK_ALIGNMENT
    0x0CF2, // UNPACK_ROW_LENGTH
    0x0CF3, // UNPACK_SKIP_ROWS
    0x0CF4, // UNPACK_SKIP_PIXELS
    0x806E, // UNPACK_IMAGE_HEIGHT
    0x806D, // UNPACK_SKIP_IMAGES
    0x0D02, // PACK_ROW_LENGTH
    0x0D03, // PACK_SKIP_ROWS
    0x0D04, // PACK_SKIP_PIXELS
];

pub fn pixel_store_index(p: u32) -> Option<usize> {
    PIXEL_STORE.iter().position(|&c| c == p)
}

/// Texture targets per unit, in [`GlState::textures`] order.
pub const TEX_TARGETS: [u32; 4] = [0x0DE1, 0x8513, 0x806F, 0x8C1A];

/// Slot of a texture target (TEXTURE_EXTERNAL_OES is a 2D texture on the host).
pub fn tex_slot(target: u32) -> Option<usize> {
    match target {
        0x8D65 => Some(0),
        t => TEX_TARGETS.iter().position(|&c| c == t),
    }
}

/// Generic buffer bindings kept per context (ELEMENT_ARRAY is vertex array
/// state), in [`GlState::buffers`] order.
pub const BUFFER_TARGETS: [u32; 7] = [0x8892, 0x8F36, 0x8F37, 0x88EB, 0x88EC, 0x8A11, 0x8C8E];

pub fn buffer_slot(target: u32) -> Option<usize> {
    BUFFER_TARGETS.iter().position(|&c| c == target)
}

pub const UNITS: usize = 32;
pub const ATTRIBS: usize = 16;
pub const UBO_BINDINGS: usize = 24;

/// A generic vertex attribute's current value: kind (0 float, 1 int, 2 uint)
/// and four 32-bit values.
pub type AttribValue = (u8, [u32; 4]);

#[derive(Clone, Debug, PartialEq)]
pub struct GlState {
    pub enabled: [bool; CAPS.len()],
    pub blend_color: [f32; 4],
    pub blend_eq: [u32; 2],
    pub blend_func: [u32; 4],
    pub color_mask: [bool; 4],
    pub depth_mask: bool,
    pub stencil_writemask: [u32; 2],
    pub clear_color: [f32; 4],
    pub clear_depth: f32,
    pub clear_stencil: i32,
    pub cull_face: u32,
    pub front_face: u32,
    pub depth_func: u32,
    pub depth_range: [f32; 2],
    pub line_width: f32,
    pub polygon_offset: [f32; 2],
    pub sample_coverage: (f32, bool),
    pub scissor: [i32; 4],
    pub viewport: [i32; 4],
    /// Front, back: (func, ref, mask).
    pub stencil_func: [(u32, i32, u32); 2],
    /// Front, back: (fail, zfail, zpass).
    pub stencil_op: [(u32, u32, u32); 2],
    /// GENERATE_MIPMAP_HINT, FRAGMENT_SHADER_DERIVATIVE_HINT.
    pub hints: [u32; 2],
    pub pixel_store: [i32; PIXEL_STORE.len()],
    pub active_texture: u32,
    pub textures: [[u32; 4]; UNITS],
    pub samplers: [u32; UNITS],
    pub program: u32,
    pub vao: u32,
    pub buffers: [u32; BUFFER_TARGETS.len()],
    /// Indexed UNIFORM_BUFFER bindings: (buffer, offset, size; size 0 = base).
    pub ubo: [(u32, i64, i64); UBO_BINDINGS],
    pub draw_fbo: u32,
    pub read_fbo: u32,
    pub renderbuffer: u32,
    pub transform_feedback: u32,
    pub attribs: [AttribValue; ATTRIBS],
}

impl Default for GlState {
    fn default() -> Self {
        let mut enabled = [false; CAPS.len()];
        enabled[3] = true; // DITHER
        Self {
            enabled,
            blend_color: [0.0; 4],
            blend_eq: [0x8006; 2],
            blend_func: [1, 0, 1, 0],
            color_mask: [true; 4],
            depth_mask: true,
            stencil_writemask: [u32::MAX; 2],
            clear_color: [0.0; 4],
            clear_depth: 1.0,
            clear_stencil: 0,
            cull_face: 0x0405,
            front_face: 0x0901,
            depth_func: 0x0201,
            depth_range: [0.0, 1.0],
            line_width: 1.0,
            polygon_offset: [0.0; 2],
            sample_coverage: (1.0, false),
            scissor: [0; 4],
            viewport: [0; 4],
            stencil_func: [(0x0207, 0, u32::MAX); 2],
            stencil_op: [(0x1E00, 0x1E00, 0x1E00); 2],
            hints: [0x1100; 2],
            pixel_store: [4, 4, 0, 0, 0, 0, 0, 0, 0, 0],
            active_texture: 0,
            textures: [[0; 4]; UNITS],
            samplers: [0; UNITS],
            program: 0,
            vao: 0,
            buffers: [0; BUFFER_TARGETS.len()],
            ubo: [(0, 0, 0); UBO_BINDINGS],
            draw_fbo: 0,
            read_fbo: 0,
            renderbuffer: 0,
            transform_feedback: 0,
            attribs: [(0, [0, 0, 0, 1f32.to_bits()]); ATTRIBS],
        }
    }
}

const FRONT: u32 = 0x0404;
const BACK: u32 = 0x0405;

/// Emits the ops that turn WebGL state `from` into `to`.
pub fn transition(ops: &mut OpBuf, from: &GlState, to: &GlState) {
    if from == to {
        return;
    }
    for (i, &cap) in CAPS.iter().enumerate() {
        if from.enabled[i] != to.enabled[i] {
            ops.op(if to.enabled[i] { Code::Enable } else { Code::Disable }, &[cap]);
        }
    }
    if from.blend_color != to.blend_color {
        let c = to.blend_color;
        ops.op(Code::BlendColor, &[fw(c[0]), fw(c[1]), fw(c[2]), fw(c[3])]);
    }
    if from.blend_eq != to.blend_eq {
        ops.op(Code::BlendEquationSeparate, &to.blend_eq);
    }
    if from.blend_func != to.blend_func {
        ops.op(Code::BlendFuncSeparate, &to.blend_func);
    }
    if from.color_mask != to.color_mask {
        let m = to.color_mask.map(u32::from);
        ops.op(Code::ColorMask, &m);
    }
    if from.depth_mask != to.depth_mask {
        ops.op(Code::DepthMask, &[to.depth_mask as u32]);
    }
    for (k, face) in [FRONT, BACK].into_iter().enumerate() {
        if from.stencil_writemask[k] != to.stencil_writemask[k] {
            ops.op(Code::StencilMaskSeparate, &[face, to.stencil_writemask[k]]);
        }
        if from.stencil_func[k] != to.stencil_func[k] {
            let (f, r, m) = to.stencil_func[k];
            ops.op(Code::StencilFuncSeparate, &[face, f, r as u32, m]);
        }
        if from.stencil_op[k] != to.stencil_op[k] {
            let (a, b, c) = to.stencil_op[k];
            ops.op(Code::StencilOpSeparate, &[face, a, b, c]);
        }
    }
    if from.clear_color != to.clear_color {
        let c = to.clear_color;
        ops.op(Code::ClearColor, &[fw(c[0]), fw(c[1]), fw(c[2]), fw(c[3])]);
    }
    if from.clear_depth != to.clear_depth {
        ops.op(Code::ClearDepth, &[fw(to.clear_depth)]);
    }
    if from.clear_stencil != to.clear_stencil {
        ops.op(Code::ClearStencil, &[to.clear_stencil as u32]);
    }
    if from.cull_face != to.cull_face {
        ops.op(Code::CullFace, &[to.cull_face]);
    }
    if from.front_face != to.front_face {
        ops.op(Code::FrontFace, &[to.front_face]);
    }
    if from.depth_func != to.depth_func {
        ops.op(Code::DepthFunc, &[to.depth_func]);
    }
    if from.depth_range != to.depth_range {
        ops.op(Code::DepthRange, &[fw(to.depth_range[0]), fw(to.depth_range[1])]);
    }
    if from.line_width != to.line_width {
        ops.op(Code::LineWidth, &[fw(to.line_width)]);
    }
    if from.polygon_offset != to.polygon_offset {
        ops.op(Code::PolygonOffset, &[fw(to.polygon_offset[0]), fw(to.polygon_offset[1])]);
    }
    if from.sample_coverage != to.sample_coverage {
        ops.op(Code::SampleCoverage, &[fw(to.sample_coverage.0), to.sample_coverage.1 as u32]);
    }
    if from.scissor != to.scissor {
        ops.op(Code::Scissor, &to.scissor.map(|v| v as u32));
    }
    if from.viewport != to.viewport {
        ops.op(Code::Viewport, &to.viewport.map(|v| v as u32));
    }
    for (k, target) in [0x8192u32, 0x8B8B].into_iter().enumerate() {
        if from.hints[k] != to.hints[k] {
            ops.op(Code::Hint, &[target, to.hints[k]]);
        }
    }
    for (i, &p) in PIXEL_STORE.iter().enumerate() {
        if from.pixel_store[i] != to.pixel_store[i] {
            ops.op(Code::PixelStorei, &[p, to.pixel_store[i] as u32]);
        }
    }
    // Texture units: switch units only where something differs.
    let mut unit = from.active_texture;
    for u in 0..UNITS {
        if from.textures[u] != to.textures[u] {
            if unit != u as u32 {
                ops.op(Code::ActiveTexture, &[0x84C0 + u as u32]);
                unit = u as u32;
            }
            for (s, &target) in TEX_TARGETS.iter().enumerate() {
                if from.textures[u][s] != to.textures[u][s] {
                    ops.op(Code::BindTexture, &[target, to.textures[u][s]]);
                }
            }
        }
        if from.samplers[u] != to.samplers[u] {
            ops.op(Code::BindSampler, &[u as u32, to.samplers[u]]);
        }
    }
    if unit != to.active_texture {
        ops.op(Code::ActiveTexture, &[0x84C0 + to.active_texture]);
    }
    if from.program != to.program {
        ops.op(Code::UseProgram, &[to.program]);
    }
    if from.vao != to.vao {
        ops.op(Code::BindVertexArray, &[to.vao]);
    }
    for (u, (&a, &b)) in from.ubo.iter().zip(&to.ubo).enumerate() {
        if a != b {
            ops.op(Code::BindBufferRange, &[0x8A11, u as u32, b.0, b.1 as u32, b.2 as u32]);
        }
    }
    // Generic bindings after the indexed ones (BindBufferRange changes the
    // generic UNIFORM_BUFFER binding too).
    for (i, &target) in BUFFER_TARGETS.iter().enumerate() {
        if from.buffers[i] != to.buffers[i] || (target == 0x8A11 && from.ubo != to.ubo) {
            ops.op(Code::BindBuffer, &[target, to.buffers[i]]);
        }
    }
    if from.draw_fbo != to.draw_fbo {
        ops.op(Code::BindFramebuffer, &[0x8CA9, to.draw_fbo]);
    }
    if from.read_fbo != to.read_fbo {
        ops.op(Code::BindFramebuffer, &[0x8CA8, to.read_fbo]);
    }
    if from.renderbuffer != to.renderbuffer {
        ops.op(Code::BindRenderbuffer, &[to.renderbuffer]);
    }
    if from.transform_feedback != to.transform_feedback {
        ops.op(Code::BindTransformFeedback, &[to.transform_feedback]);
    }
    for (i, (a, b)) in from.attribs.iter().zip(&to.attribs).enumerate() {
        if a != b {
            let code = match b.0 {
                1 => Code::VertexAttribI4i,
                2 => Code::VertexAttribI4ui,
                _ => Code::VertexAttrib4f,
            };
            ops.op(code, &[i as u32, b.1[0], b.1[1], b.1[2], b.1[3]]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(ops: &OpBuf) -> Vec<u16> {
        let mut v = Vec::new();
        let mut i = 0;
        while i < ops.words.len() {
            v.push((ops.words[i] >> 16) as u16);
            i += (ops.words[i] & 0xffff) as usize;
        }
        v
    }

    #[test]
    fn transition_emits_only_differences() {
        let a = GlState::default();
        let mut ops = OpBuf::default();
        transition(&mut ops, &a, &a);
        assert!(ops.is_empty());
        let mut b = a.clone();
        b.enabled[0] = true;
        b.viewport = [0, 0, 1280, 800];
        b.textures[3][0] = 17;
        b.active_texture = 1;
        b.program = 5;
        transition(&mut ops, &a, &b);
        assert_eq!(
            codes(&ops),
            [
                Code::Enable as u16,
                Code::Viewport as u16,
                Code::ActiveTexture as u16,
                Code::BindTexture as u16,
                Code::ActiveTexture as u16,
                Code::UseProgram as u16
            ]
        );
        // Unit 3 selected for the bind, then unit 1 (the active one).
        let w = &ops.words;
        assert!(w.windows(2).any(|p| p == [(Code::ActiveTexture as u32) << 16 | 2, 0x84C3]));
        assert!(w.windows(2).any(|p| p == [(Code::ActiveTexture as u32) << 16 | 2, 0x84C1]));
        // And back.
        let mut back = OpBuf::default();
        transition(&mut back, &b, &a);
        assert_eq!(codes(&back).len(), 6);
    }
}
