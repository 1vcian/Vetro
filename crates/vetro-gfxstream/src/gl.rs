//! EGL and GLES on the host side: renderControl objects (contexts, window
//! surfaces, ColorBuffers, images, syncs) and GLES calls, turned into the
//! WebGL2 op stream. Behaviour follows upstream's host
//! (`RenderControl.cpp`, `FrameBuffer.cpp`, `GLESv2Decoder.cpp`); what the
//! guest can query is answered here, deterministically (ADR 0037).

use std::collections::{BTreeMap, BTreeSet};

use crate::exec::{Code, GlExecutor, Kind, Ops, fw};
use crate::formats::{self, Tex};
use crate::glsl;
use crate::state::{self, GlState};
use crate::tables::{GLES2, GLES2_BASE, RC, RC_BASE, gles2 as g, rc};
use crate::wire::{self, Arg, Args, Reply};

/// Guest names of one namespace → host ids.
#[derive(Clone, Debug, Default)]
pub struct Names {
    map: BTreeMap<u32, u32>,
    next: u32,
}

impl Names {
    /// A fresh guest name.
    fn fresh(&mut self) -> u32 {
        loop {
            self.next = self.next.wrapping_add(1).max(1);
            if !self.map.contains_key(&self.next) {
                return self.next;
            }
        }
    }
    pub fn get(&self, name: u32) -> Option<u32> {
        self.map.get(&name).copied()
    }
    fn insert(&mut self, name: u32, id: u32) {
        self.map.insert(name, id);
    }
    fn remove(&mut self, name: u32) -> Option<u32> {
        self.map.remove(&name)
    }
    /// Guest name of a host id (0 if none).
    pub fn guest_of(&self, id: u32) -> u32 {
        if id == 0 {
            return 0;
        }
        self.map.iter().find(|(_, v)| **v == id).map_or(0, |(k, _)| *k)
    }
    pub fn next_value(&self) -> u32 {
        self.next
    }
    /// (guest name, host id) pairs.
    pub fn pairs(&self) -> Vec<(u32, u32)> {
        self.map.iter().map(|(&k, &v)| (k, v)).collect()
    }
    pub fn from_parts(next: u32, pairs: Vec<(u32, u32)>) -> Self {
        Self { map: pairs.into_iter().collect(), next }
    }
    fn ids(&self) -> Vec<u32> {
        self.map.values().copied().collect()
    }
}

#[derive(Clone, Debug)]
pub struct ShaderObj {
    pub id: u32,
    pub ty: u32,
    pub source: String,
    pub scan: glsl::Shader,
    pub delete_pending: bool,
    pub attached: u32,
}

/// A uniform with its virtual locations `base .. base + size`.
#[derive(Clone, Debug)]
pub struct UniformLoc {
    pub var: glsl::Var,
    pub base: i32,
}

#[derive(Clone, Debug, Default)]
pub struct ProgramObj {
    pub id: u32,
    /// Attached shaders (guest names).
    pub shaders: Vec<u32>,
    /// glBindAttribLocation before link.
    pub binds: BTreeMap<String, u32>,
    pub linked: bool,
    pub uniforms: Vec<UniformLoc>,
    pub attribs: Vec<(glsl::Var, i32)>,
    pub blocks: Vec<glsl::Block>,
    /// Uniform values by virtual location (snapshots, glGetUniform*).
    pub values: BTreeMap<i32, Vec<u32>>,
    pub delete_pending: bool,
    /// glUniformBlockBinding: block index → binding.
    pub block_bindings: BTreeMap<u32, u32>,
}

impl ProgramObj {
    /// Virtual location of `name` ("u", "a[3]", "s.f", "a[1].f").
    pub fn location(&self, name: &str) -> i32 {
        let (base_name, index) = match name.strip_suffix(']').and_then(|n| n.rsplit_once('[')) {
            Some((b, i)) => match i.parse::<u32>() {
                Ok(i) => (b, Some(i)),
                Err(_) => (name, None),
            },
            None => (name, None),
        };
        for u in &self.uniforms {
            if u.var.name == name {
                return u.base;
            }
            if u.var.name == base_name
                && let Some(i) = index
            {
                if i < u.var.size {
                    return u.base + i as i32;
                }
                return -1;
            }
        }
        -1
    }

    /// The uniform owning virtual location `loc`.
    pub fn uniform_at(&self, loc: i32) -> Option<&UniformLoc> {
        self.uniforms.iter().find(|u| loc >= u.base && loc < u.base + u.var.size as i32)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Share {
    pub refs: u32,
    pub textures: Names,
    pub buffers: Names,
    pub renderbuffers: Names,
    pub samplers: Names,
    /// Shaders and programs share one namespace.
    pub sp: Names,
    pub shaders: BTreeMap<u32, ShaderObj>,
    pub programs: BTreeMap<u32, ProgramObj>,
    /// Buffer sizes by host id (glGetBufferParameteriv).
    pub buffer_sizes: BTreeMap<u32, i64>,
    /// Texture host ids that alias a ColorBuffer's texture (rcBindTexture):
    /// never deleted with the guest name.
    pub aliases: BTreeSet<u32>,
    /// Renderbuffers backed by a ColorBuffer (rcBindRenderbuffer): host
    /// renderbuffer id → ColorBuffer texture.
    pub rb_tex: BTreeMap<u32, u32>,
    /// What snapshots need to rebuild the objects (ADR 0037, "Snapshots").
    pub tex_info: BTreeMap<u32, TexInfo>,
    /// Renderbuffer storage: internal format, width, height, samples.
    pub rb_info: BTreeMap<u32, [u32; 4]>,
    pub sampler_params: BTreeMap<u32, BTreeMap<u32, (bool, u32)>>,
    /// Buffers ever bound to ELEMENT_ARRAY_BUFFER (WebGL keeps them there).
    pub element_buffers: BTreeSet<u32>,
    pub buffer_usage: BTreeMap<u32, u32>,
}

/// A texture as snapshots rebuild it.
#[derive(Clone, Debug, Default)]
pub struct TexInfo {
    /// The bind target of its first use (TEXTURE_2D, CUBE_MAP…).
    pub target: u32,
    /// (image target, level) → the level's specification.
    pub levels: BTreeMap<(u32, u32), LevelSpec>,
    pub params: BTreeMap<u32, (bool, u32)>,
    /// glTexStorage: levels, internal format, width, height, depth.
    pub storage: Option<[u32; 5]>,
    pub mipmap: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LevelSpec {
    pub ifmt: u32,
    pub w: u32,
    pub h: u32,
    pub d: u32,
    pub fmt: u32,
    pub ty: u32,
    /// Compressed data (it cannot be read back).
    pub compressed: Option<Vec<u8>>,
}

/// A vertex attribute of a vertex array, as snapshots rebuild it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AttribPtr {
    pub enabled: bool,
    pub size: u32,
    pub ty: u32,
    pub norm: bool,
    pub stride: u32,
    pub offset: u32,
    pub buffer: u32,
    pub integer: bool,
    pub divisor: u32,
    /// Client-side data (resent before every draw: nothing to rebuild).
    pub client: bool,
}

/// Format and type that go with an unsized (or sized) internal format for
/// an upload of no data.
pub fn unsized_of(ifmt: u32) -> (u32, u32) {
    use crate::formats::*;
    match ifmt {
        GL_RGB | GL_RGB8 => (GL_RGB, GL_UNSIGNED_BYTE),
        GL_RGB565 => (GL_RGB, GL_UNSIGNED_SHORT_5_6_5),
        GL_R8 | GL_RED => (GL_RED, GL_UNSIGNED_BYTE),
        GL_RG8 | GL_RG => (GL_RG, GL_UNSIGNED_BYTE),
        0x1906 => (0x1906, GL_UNSIGNED_BYTE), // ALPHA
        0x1909 => (0x1909, GL_UNSIGNED_BYTE), // LUMINANCE
        0x190A => (0x190A, GL_UNSIGNED_BYTE), // LUMINANCE_ALPHA
        _ => (GL_RGBA, GL_UNSIGNED_BYTE),
    }
}

#[derive(Clone, Debug)]
pub struct Ctx {
    pub share: u32,
    pub version: u32,
    pub config: u32,
    pub fbos: Names,
    pub vaos: Names,
    pub queries: Names,
    pub tfbs: Names,
    /// The host vertex array standing for the guest's vertex array 0.
    pub default_vao: u32,
    pub state: GlState,
    /// ELEMENT_ARRAY_BUFFER of each host vertex array.
    pub vao_elements: BTreeMap<u32, u32>,
    pub draw: u32,
    pub read: u32,
    /// The guest has framebuffer 0 bound for drawing / reading.
    pub draw_default: bool,
    pub read_default: bool,
    /// Viewport and scissor already set from a surface (EGL: at the first
    /// eglMakeCurrent with a surface).
    pub sized: bool,
    /// Attachments of the guest's framebuffers: (host fbo, attachment) →
    /// (object type, guest name, level).
    pub attachments: BTreeMap<(u32, u32), (u32, u32, i32)>,
    /// The attach op of each (host framebuffer, attachment), for snapshots.
    pub fb_ops: BTreeMap<(u32, u32), (Code, Vec<u32>)>,
    /// Vertex attributes of each host vertex array, for snapshots.
    pub vao_attribs: BTreeMap<u32, [AttribPtr; state::ATTRIBS]>,
}

#[derive(Clone, Debug)]
pub struct Surface {
    pub config: u32,
    pub width: u32,
    pub height: u32,
    pub cb: u32,
    pub fbo: u32,
    /// Depth-stencil renderbuffer (0 if the config has none).
    pub depth: u32,
}

#[derive(Clone, Debug)]
pub struct ColorBuffer {
    pub width: u32,
    pub height: u32,
    pub tex: Tex,
    /// Host texture id.
    pub id: u32,
    pub refs: u32,
    /// Created for a virtio-gpu resource (its lifetime is the resource's).
    pub resource: bool,
}

/// Per render thread (one per guest GL thread, one per pipe).
#[derive(Clone, Debug, Default)]
pub struct Thread {
    /// The virtio-gpu context of its pipe (for traces).
    pub id: u32,
    pub ctx: u32,
    pub draw: u32,
    pub read: u32,
    pub puid: u64,
}

/// Rendering statistics (frames presented, ops, bytes read back).
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub calls: u64,
    pub batches: u64,
    pub presents: u64,
    pub readback_bytes: u64,
    pub unhandled: u64,
}

const HANDLE_BASE: u32 = 0x4000_0000;

pub struct Gl {
    pub ops: Ops,
    /// Behind a `RefCell` so that snapshots (saved through `&self`) can run
    /// their readbacks.
    pub exec: core::cell::RefCell<Box<dyn GlExecutor>>,
    pub(crate) next_id: u32,
    pub(crate) next_handle: u32,
    pub(crate) next_sync: u64,
    pub shares: BTreeMap<u32, Share>,
    pub ctxs: BTreeMap<u32, Ctx>,
    pub surfaces: BTreeMap<u32, Surface>,
    pub cbs: BTreeMap<u32, ColorBuffer>,
    /// EGL_GL_TEXTURE_2D images: handle → host texture id.
    pub images: BTreeMap<u32, u32>,
    /// Creator process (puid) of contexts, surfaces and rc ColorBuffers.
    pub owners: BTreeMap<u32, u64>,
    /// What the WebGL context has now, and whose context it is (0 = none).
    pub applied: GlState,
    pub active: u32,
    pub display: (u32, u32),
    pub stats: Stats,
    /// Host log (unhandled calls, bad arguments), bounded.
    pub log: Vec<String>,
    /// Every call and pipe event goes to the log (`--gl-trace`), bounded.
    pub trace: bool,
    warned: BTreeSet<String>,
    scratch: Vec<u8>,
}

const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_READ_FRAMEBUFFER: u32 = 0x8CA8;
const GL_DRAW_FRAMEBUFFER: u32 = 0x8CA9;
const GL_ELEMENT_ARRAY_BUFFER: u32 = 0x8893;
const GL_RENDERBUFFER: u32 = 0x8D41;
const GL_TEXTURE: u32 = 0x1702;
const GL_VERTEX_SHADER: u32 = 0x8B31;

impl Gl {
    pub fn new(exec: Box<dyn GlExecutor>, display: (u32, u32)) -> Self {
        Self {
            ops: Ops::default(),
            exec: core::cell::RefCell::new(exec),
            next_id: 0,
            next_handle: HANDLE_BASE,
            next_sync: 0,
            shares: BTreeMap::new(),
            ctxs: BTreeMap::new(),
            surfaces: BTreeMap::new(),
            cbs: BTreeMap::new(),
            images: BTreeMap::new(),
            owners: BTreeMap::new(),
            applied: GlState::default(),
            active: 0,
            display,
            stats: Stats::default(),
            log: Vec::new(),
            trace: false,
            warned: BTreeSet::new(),
            scratch: Vec::new(),
        }
    }

    /// A trace line (only with `trace`), up to 200 000 lines.
    pub fn trace_line(&mut self, what: String) {
        if self.trace && self.log.len() < 200_000 {
            self.log.push(what);
        }
    }

    pub fn warn(&mut self, what: String) {
        if self.warned.len() < 4096 && self.warned.insert(what.clone()) && self.log.len() < 4096 {
            self.log.push(what);
        }
    }

    fn new_id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    fn new_handle(&mut self) -> u32 {
        loop {
            self.next_handle = self.next_handle.wrapping_add(1).max(HANDLE_BASE);
            let h = self.next_handle;
            if !self.ctxs.contains_key(&h) && !self.surfaces.contains_key(&h) && !self.cbs.contains_key(&h) {
                return h;
            }
        }
    }

    fn create(&mut self, kind: Kind, extra: u32) -> u32 {
        let id = self.new_id();
        self.ops.op(Code::Create, &[kind as u32, id, extra]);
        id
    }

    fn delete(&mut self, kind: Kind, id: u32) {
        if id != 0 {
            self.ops.op(Code::Delete, &[kind as u32, id]);
        }
    }

    /// Runs the queued ops; returns what the read ops produced.
    pub fn flush(&mut self) -> Vec<u8> {
        let out = self.run_ops();
        if let Some(out) = &out {
            self.stats.batches += 1;
            self.stats.readback_bytes += out.len() as u64;
        }
        out.unwrap_or_default()
    }

    /// Runs the queued ops (also through `&self`); `None` if there were none.
    pub fn run_ops(&self) -> Option<Vec<u8>> {
        if self.ops.is_empty() {
            return None;
        }
        let b = self.ops.take();
        let mut out = vec![0u8; b.out_len];
        self.exec.borrow_mut().execute(&b.words, &b.blob, &mut out);
        Some(out)
    }

    /// Replaces the executor (returns the previous one).
    pub fn set_executor(&self, exec: Box<dyn GlExecutor>) -> Box<dyn GlExecutor> {
        self.exec.replace(exec)
    }

    // ---- ColorBuffers ----

    /// A ColorBuffer with handle `handle` (a resource id, or an rc handle).
    pub fn cb_create(&mut self, handle: u32, width: u32, height: u32, tex: Tex, resource: bool) {
        let id = self.create(Kind::Texture, 0);
        self.ops.op(Code::TexAlloc, &[id, tex.internal, width, height, tex.format, tex.ty]);
        self.cbs.insert(handle, ColorBuffer { width, height, tex, id, refs: 1, resource });
    }

    pub fn cb_destroy(&mut self, handle: u32) {
        if let Some(cb) = self.cbs.remove(&handle) {
            self.delete(Kind::Texture, cb.id);
            for s in self.surfaces.values_mut() {
                if s.cb == handle {
                    s.cb = 0;
                }
            }
        }
    }

    /// Guest bytes (rows `stride` apart) → the ColorBuffer's rectangle.
    pub fn cb_upload(&mut self, handle: u32, x: u32, y: u32, w: u32, h: u32, data: &[u8], stride: usize) {
        let Some(cb) = self.cbs.get(&handle).cloned() else { return };
        if !cb.tex.transferable || w == 0 || h == 0 {
            return;
        }
        let row = (w * cb.tex.bpp) as usize;
        let mut px = std::mem::take(&mut self.scratch);
        px.clear();
        for r in 0..h as usize {
            let s = r * stride;
            match data.get(s..s + row) {
                Some(line) => px.extend_from_slice(line),
                None => px.resize(px.len() + row, 0),
            }
        }
        if cb.tex.swizzle {
            formats::swap_rb(&mut px);
        }
        self.ops.op_blob(Code::TexUpload, &[cb.id, x, y, w, h, cb.tex.format, cb.tex.ty], &px);
        self.scratch = px;
    }

    /// The ColorBuffer's rectangle as guest bytes (tightly packed rows,
    /// row 0 = texture row `y`): a readback (ADR 0037).
    pub fn cb_read(&mut self, handle: u32, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
        let Some(cb) = self.cbs.get(&handle).cloned() else { return Vec::new() };
        if !cb.tex.transferable || w == 0 || h == 0 {
            return vec![0; (w * h * cb.tex.bpp) as usize];
        }
        let n = (w * h * cb.tex.bpp) as usize;
        self.ops.op_read(Code::ReadTexture, &[cb.id, x, y, w, h, cb.tex.format, cb.tex.ty, n as u32], n);
        let mut out = self.flush();
        out.truncate(n);
        if cb.tex.swizzle {
            formats::swap_rb(&mut out);
        }
        out
    }

    /// Presents a ColorBuffer on the canvas.
    pub fn present(&mut self, handle: u32) {
        if let Some(cb) = self.cbs.get(&handle) {
            let (id, w, h) = (cb.id, cb.width, cb.height);
            self.ops.op(Code::Present, &[id, w, h]);
            self.stats.presents += 1;
        }
        self.flush();
    }

    // ---- contexts and state ----

    /// Makes `t`'s context the one the WebGL context reflects.
    fn activate(&mut self, t: &Thread) -> bool {
        if t.ctx == 0 || !self.ctxs.contains_key(&t.ctx) {
            return false;
        }
        if self.active != t.ctx {
            let to = self.ctxs[&t.ctx].state.clone();
            state::transition(&mut self.ops.buf(), &self.applied, &to);
            self.applied = to;
            self.active = t.ctx;
        }
        true
    }

    /// Changes the active context's state (and the mirror of what WebGL has).
    fn set(&mut self, t: &Thread, f: impl Fn(&mut GlState)) {
        if let Some(c) = self.ctxs.get_mut(&t.ctx) {
            f(&mut c.state);
            if self.active == t.ctx {
                f(&mut self.applied);
            }
        }
    }

    fn ctx(&self, t: &Thread) -> Option<&Ctx> {
        self.ctxs.get(&t.ctx)
    }

    fn share_of(&mut self, t: &Thread) -> Option<&mut Share> {
        let s = self.ctxs.get(&t.ctx)?.share;
        self.shares.get_mut(&s)
    }

    /// Host id of guest texture `name`, created on first use (GLES allows
    /// binding names never generated).
    fn texture(&mut self, t: &Thread, name: u32) -> u32 {
        if name == 0 {
            return 0;
        }
        if let Some(id) = self.share_of(t).and_then(|s| s.textures.get(name)) {
            return id;
        }
        let id = self.create(Kind::Texture, 0);
        if let Some(s) = self.share_of(t) {
            s.textures.insert(name, id);
        }
        id
    }

    fn buffer(&mut self, t: &Thread, name: u32) -> u32 {
        if name == 0 {
            return 0;
        }
        if let Some(id) = self.share_of(t).and_then(|s| s.buffers.get(name)) {
            return id;
        }
        let id = self.create(Kind::Buffer, 0);
        if let Some(s) = self.share_of(t) {
            s.buffers.insert(name, id);
        }
        id
    }

    fn renderbuffer(&mut self, t: &Thread, name: u32) -> u32 {
        if name == 0 {
            return 0;
        }
        if let Some(id) = self.share_of(t).and_then(|s| s.renderbuffers.get(name)) {
            return id;
        }
        let id = self.create(Kind::Renderbuffer, 0);
        if let Some(s) = self.share_of(t) {
            s.renderbuffers.insert(name, id);
        }
        id
    }

    fn sampler(&mut self, t: &Thread, name: u32) -> u32 {
        if name == 0 {
            return 0;
        }
        if let Some(id) = self.share_of(t).and_then(|s| s.samplers.get(name)) {
            return id;
        }
        let id = self.create(Kind::Sampler, 0);
        if let Some(s) = self.share_of(t) {
            s.samplers.insert(name, id);
        }
        id
    }

    fn framebuffer(&mut self, t: &Thread, name: u32) -> u32 {
        if name == 0 {
            return 0;
        }
        if let Some(id) = self.ctx(t).and_then(|c| c.fbos.get(name)) {
            return id;
        }
        let id = self.create(Kind::Framebuffer, 0);
        if let Some(c) = self.ctxs.get_mut(&t.ctx) {
            c.fbos.insert(name, id);
        }
        id
    }

    fn vertex_array(&mut self, t: &Thread, name: u32) -> u32 {
        let Some(c) = self.ctx(t) else { return 0 };
        if name == 0 {
            return c.default_vao;
        }
        if let Some(id) = c.vaos.get(name) {
            return id;
        }
        let id = self.create(Kind::VertexArray, 0);
        if let Some(c) = self.ctxs.get_mut(&t.ctx) {
            c.vaos.insert(name, id);
        }
        id
    }

    fn query(&mut self, t: &Thread, name: u32) -> u32 {
        if name == 0 {
            return 0;
        }
        if let Some(id) = self.ctx(t).and_then(|c| c.queries.get(name)) {
            return id;
        }
        let id = self.create(Kind::Query, 0);
        if let Some(c) = self.ctxs.get_mut(&t.ctx) {
            c.queries.insert(name, id);
        }
        id
    }

    fn transform_feedback(&mut self, t: &Thread, name: u32) -> u32 {
        if name == 0 {
            return 0;
        }
        if let Some(id) = self.ctx(t).and_then(|c| c.tfbs.get(name)) {
            return id;
        }
        let id = self.create(Kind::TransformFeedback, 0);
        if let Some(c) = self.ctxs.get_mut(&t.ctx) {
            c.tfbs.insert(name, id);
        }
        id
    }

    /// The host framebuffer standing for the guest's framebuffer 0: the
    /// surface's.
    fn surface_fbo(&self, surface: u32) -> u32 {
        self.surfaces.get(&surface).map_or(0, |s| s.fbo)
    }

    fn program_mut(&mut self, t: &Thread, name: u32) -> Option<&mut ProgramObj> {
        self.share_of(t)?.programs.get_mut(&name)
    }

    fn current_program(&self, t: &Thread) -> Option<(u32, &ProgramObj)> {
        let c = self.ctx(t)?;
        let s = self.shares.get(&c.share)?;
        let name = s.sp.guest_of(c.state.program);
        s.programs.get(&name).map(|p| (name, p))
    }

    // ---- renderControl ----

    fn rc_string(reply: &mut Reply, out: usize, size: i32, s: &str) -> i64 {
        let len = s.len() as i32 + 1;
        if size < len || reply.out(out).len() < len as usize {
            return i64::from(-len);
        }
        reply.out_str(out, s.as_bytes());
        i64::from(len)
    }

    /// One renderControl call; the reply is filled in place.
    fn rc(&mut self, t: &mut Thread, op: u32, a: &Args<'_, '_>, r: &mut Reply) {
        use crate::caps as c;
        match op {
            rc::rcGetRendererVersion => r.ret = 1,
            rc::rcGetEGLVersion => {
                r.out_i32s(0, &[1]);
                r.out_i32s(1, &[4]);
                r.ret = 1;
            }
            rc::rcQueryEGLString => {
                let s = match a.u(0) {
                    0x3053 => c::EGL_VENDOR,
                    0x3054 => c::EGL_VERSION,
                    0x3055 => c::EGL_EXTENSIONS,
                    0x308D => c::EGL_CLIENT_APIS,
                    _ => "",
                };
                r.ret = Self::rc_string(r, 0, a.i(2), s) as u64;
            }
            rc::rcGetGLString => {
                let s = match a.u(0) {
                    0x1F00 => c::GL_VENDOR,
                    0x1F01 => c::GL_RENDERER,
                    0x1F02 => c::GL_VERSION,
                    0x1F03 => c::GL_EXTENSIONS,
                    0x8B8C => c::GL_SHADING_LANGUAGE_VERSION,
                    _ => "",
                };
                r.ret = Self::rc_string(r, 0, a.i(2), s) as u64;
            }
            rc::rcGetHostExtensionsString => {
                r.ret = Self::rc_string(r, 0, a.i(0), c::GL_EXTENSIONS) as u64;
            }
            rc::rcGetNumConfigs => {
                r.out_u32s(0, &[c::CONFIG_ATTRIBS.len() as u32]);
                r.ret = c::CONFIGS.len() as u64;
            }
            rc::rcGetConfigs => {
                let p = c::pack_configs();
                let need = (p.len() * 4) as u32;
                if a.u(0) < need || r.out(0).len() < need as usize {
                    r.ret = (-(need as i64)) as u64;
                } else {
                    r.out_u32s(0, &p);
                    r.ret = c::CONFIGS.len() as u64;
                }
            }
            rc::rcChooseConfig => {
                let attribs: Vec<i32> = a.u32s(0).into_iter().map(|v| v as i32).collect();
                let found = c::choose_config(&attribs);
                let max = a.u(3) as usize;
                let n = if r.out(0).is_empty() { found.len() } else { found.len().min(max) };
                r.out_u32s(0, &found[..n.min(found.len())]);
                r.ret = n as u64;
            }
            rc::rcGetFBParam => {
                r.ret = match a.u(0) {
                    c::FB_WIDTH => self.display.0,
                    c::FB_HEIGHT => self.display.1,
                    c::FB_XDPI | c::FB_YDPI => 240,
                    c::FB_FPS => 60,
                    c::FB_MIN_SWAP_INTERVAL => 0,
                    c::FB_MAX_SWAP_INTERVAL => 1,
                    _ => 0,
                } as u64;
            }
            rc::rcCreateContext => {
                let h = self.context_create(a.u(0), a.u(1), a.u(2));
                self.owners.insert(h, t.puid);
                r.ret = u64::from(h);
            }
            rc::rcDestroyContext => {
                self.owners.remove(&a.u(0));
                self.context_destroy(a.u(0));
            }
            rc::rcCreateWindowSurface => {
                let h = self.surface_create(a.u(0), a.u(1), a.u(2));
                self.owners.insert(h, t.puid);
                r.ret = u64::from(h);
            }
            rc::rcDestroyWindowSurface => {
                self.owners.remove(&a.u(0));
                self.surface_destroy(a.u(0));
            }
            rc::rcCreateColorBuffer | rc::rcCreateColorBufferDMA => {
                let h = self.new_handle();
                self.cb_create(h, a.u(0), a.u(1), formats::tex_of_gl(a.u(2)), false);
                self.owners.insert(h, t.puid);
                r.ret = u64::from(h);
            }
            rc::rcCreateColorBufferWithHandle => {
                let h = a.u(3);
                if !self.cbs.contains_key(&h) {
                    self.cb_create(h, a.u(0), a.u(1), formats::tex_of_gl(a.u(2)), false);
                }
            }
            rc::rcOpenColorBuffer | rc::rcOpenColorBuffer2 => {
                if let Some(cb) = self.cbs.get_mut(&a.u(0)) {
                    cb.refs += 1;
                }
            }
            rc::rcCloseColorBuffer => {
                let h = a.u(0);
                if let Some(cb) = self.cbs.get_mut(&h) {
                    cb.refs = cb.refs.saturating_sub(1);
                    if cb.refs == 0 && !cb.resource {
                        self.cb_destroy(h);
                    }
                }
            }
            rc::rcSetWindowColorBuffer => self.surface_set_cb(a.u(0), a.u(1)),
            rc::rcFlushWindowColorBuffer => r.ret = 0,
            rc::rcFlushWindowColorBufferAsync | rc::rcFlushWindowColorBufferAsyncWithFrameNumber => {}
            rc::rcMakeCurrent | rc::rcMakeCurrentAsync => {
                let ok = self.make_current(t, a.u(0), a.u(1), a.u(2));
                r.ret = ok as u64;
            }
            rc::rcFBPost => self.present(a.u(0)),
            rc::rcFBSetSwapInterval => {}
            rc::rcBindTexture => self.bind_cb_texture(t, a.u(0)),
            rc::rcBindRenderbuffer => self.bind_cb_renderbuffer(t, a.u(0)),
            rc::rcColorBufferCacheFlush => r.ret = 0,
            rc::rcReadColorBuffer => {
                let (h, x, y, w, hh) = (a.u(0), a.u(1), a.u(2), a.u(3), a.u(4));
                let px = self.cb_read(h, x, y, w, hh);
                let o = r.out(0);
                let n = o.len().min(px.len());
                o[..n].copy_from_slice(&px[..n]);
            }
            rc::rcReadColorBufferDMA | rc::rcReadColorBufferYUV => r.ret = 0,
            rc::rcUpdateColorBuffer => {
                let (h, x, y, w, hh) = (a.u(0), a.u(1), a.u(2), a.u(3), a.u(4));
                let bpp = self.cbs.get(&h).map_or(4, |c| c.tex.bpp);
                let data = a.bytes(7);
                self.cb_upload(h, x, y, w, hh, data, (w * bpp) as usize);
                r.ret = 0;
            }
            rc::rcUpdateColorBufferDMA => r.ret = 0,
            rc::rcCreateClientImage => {
                // EGL_GL_TEXTURE_2D_KHR: the image is the context's texture.
                let (ctx, target, name) = (a.u(0), a.u(1), a.u(2));
                let id = self
                    .ctxs
                    .get(&ctx)
                    .and_then(|c| self.shares.get(&c.share))
                    .and_then(|s| s.textures.get(name));
                match (target, id) {
                    (0x30B1, Some(id)) => {
                        let h = self.new_handle();
                        self.images.insert(h, id);
                        r.ret = u64::from(h);
                    }
                    _ => r.ret = 0,
                }
            }
            rc::rcDestroyClientImage => {
                r.ret = self.images.remove(&a.u(0)).is_some() as u64;
            }
            rc::rcSelectChecksumHelper => {}
            rc::rcCreateSyncKHR => {
                self.next_sync += 1;
                let s = self.next_sync;
                let o = r.out(0);
                let n = o.len().min(8);
                o[..n].copy_from_slice(&s.to_le_bytes()[..n]);
            }
            rc::rcClientWaitSyncKHR => r.ret = 0x30F6, // EGL_CONDITION_SATISFIED_KHR
            rc::rcWaitSyncKHR | rc::rcDestroySyncKHRAsync => {}
            rc::rcDestroySyncKHR => r.ret = 0,
            rc::rcIsSyncSignaled => r.ret = 1,
            rc::rcSetPuid => t.puid = a.u64(0),
            rc::rcSetColorBufferVulkanMode | rc::rcSetColorBufferVulkanMode2 => r.ret = 0,
            rc::rcGetFBDisplayConfigsCount => r.ret = 1,
            rc::rcGetFBDisplayConfigsParam => {
                r.ret = match a.u(1) {
                    0x3057 => self.display.0, // EGL_WIDTH
                    0x3056 => self.display.1, // EGL_HEIGHT
                    0x3029 => 240,
                    _ => 0,
                } as u64;
            }
            rc::rcGetFBDisplayActiveConfig => r.ret = 0,
            rc::rcSetProcessMetadata | rc::rcSetTracingForPuid => {}
            rc::rcCompose
            | rc::rcComposeWithoutPost
            | rc::rcCreateDisplay
            | rc::rcCreateDisplayById
            | rc::rcDestroyDisplay
            | rc::rcSetDisplayColorBuffer
            | rc::rcGetDisplayColorBuffer
            | rc::rcGetColorBufferDisplay
            | rc::rcGetDisplayPose
            | rc::rcSetDisplayPose
            | rc::rcSetDisplayPoseDpi
            | rc::rcMapGpaToBufferHandle
            | rc::rcMapGpaToBufferHandle2 => r.ret = (-1i64) as u64,
            rc::rcComposeAsync | rc::rcComposeAsyncWithoutPost => {}
            rc::rcCreateBuffer | rc::rcCreateBuffer2 => r.ret = 0,
            rc::rcCloseBuffer => {}
            _ => {
                self.stats.unhandled += 1;
                let name = RC.get((op - RC_BASE) as usize).map_or("?", |o| o.name);
                self.warn(format!("unhandled renderControl call {name}"));
            }
        }
    }

    /// A render thread ends (its pipe closed): its context is released.
    pub fn release_thread(&mut self, t: &mut Thread) {
        self.make_current(t, 0, 0, 0);
    }

    /// What a guest process (puid) created and never destroyed goes away
    /// when its process pipe closes, like upstream's FrameBuffer cleanup.
    pub fn cleanup_process(&mut self, puid: u64) {
        if puid == 0 {
            return;
        }
        let handles: Vec<u32> = self.owners.iter().filter(|(_, p)| **p == puid).map(|(h, _)| *h).collect();
        for h in handles {
            self.owners.remove(&h);
            if self.ctxs.contains_key(&h) {
                self.context_destroy(h);
            } else if self.surfaces.contains_key(&h) {
                self.surface_destroy(h);
            } else if self.cbs.get(&h).is_some_and(|c| !c.resource) {
                self.cb_destroy(h);
            }
        }
    }

    fn context_create(&mut self, config: u32, share: u32, version: u32) -> u32 {
        let share_id = match self.ctxs.get(&share) {
            Some(c) => c.share,
            None => {
                let h = self.new_handle();
                self.shares.insert(h, Share::default());
                h
            }
        };
        self.shares.get_mut(&share_id).unwrap().refs += 1;
        let vao = self.create(Kind::VertexArray, 0);
        let h = self.new_handle();
        let st = GlState { vao, ..GlState::default() };
        self.ctxs.insert(
            h,
            Ctx {
                share: share_id,
                version,
                config,
                fbos: Names::default(),
                vaos: Names::default(),
                queries: Names::default(),
                tfbs: Names::default(),
                default_vao: vao,
                state: st,
                vao_elements: BTreeMap::new(),
                draw: 0,
                read: 0,
                draw_default: true,
                read_default: true,
                sized: false,
                attachments: BTreeMap::new(),
                fb_ops: BTreeMap::new(),
                vao_attribs: BTreeMap::new(),
            },
        );
        h
    }

    fn context_destroy(&mut self, h: u32) {
        let Some(c) = self.ctxs.remove(&h) else { return };
        if self.active == h {
            self.active = 0;
        }
        for id in c.fbos.ids() {
            self.delete(Kind::Framebuffer, id);
        }
        for id in c.vaos.ids() {
            self.delete(Kind::VertexArray, id);
        }
        for id in c.queries.ids() {
            self.delete(Kind::Query, id);
        }
        for id in c.tfbs.ids() {
            self.delete(Kind::TransformFeedback, id);
        }
        self.delete(Kind::VertexArray, c.default_vao);
        let last = match self.shares.get_mut(&c.share) {
            Some(s) => {
                s.refs = s.refs.saturating_sub(1);
                s.refs == 0
            }
            None => false,
        };
        if last && let Some(s) = self.shares.remove(&c.share) {
            for id in s.textures.ids() {
                if !s.aliases.contains(&id) {
                    self.delete(Kind::Texture, id);
                }
            }
            for id in s.buffers.ids() {
                self.delete(Kind::Buffer, id);
            }
            for id in s.renderbuffers.ids() {
                self.delete(Kind::Renderbuffer, id);
            }
            for id in s.samplers.ids() {
                self.delete(Kind::Sampler, id);
            }
            for sh in s.shaders.values() {
                self.delete(Kind::Shader, sh.id);
            }
            for p in s.programs.values() {
                self.delete(Kind::Program, p.id);
            }
        }
    }

    fn surface_create(&mut self, config: u32, width: u32, height: u32) -> u32 {
        let fbo = self.create(Kind::Framebuffer, 0);
        let cfg = crate::caps::CONFIGS.get(config as usize).copied().unwrap_or(crate::caps::CONFIGS[0]);
        let depth = if cfg.depth > 0 || cfg.stencil > 0 { self.create(Kind::Renderbuffer, 0) } else { 0 };
        let h = self.new_handle();
        self.surfaces.insert(h, Surface { config, width, height, cb: 0, fbo, depth });
        h
    }

    fn surface_destroy(&mut self, h: u32) {
        if let Some(s) = self.surfaces.remove(&h) {
            self.delete(Kind::Framebuffer, s.fbo);
            self.delete(Kind::Renderbuffer, s.depth);
        }
    }

    fn surface_set_cb(&mut self, surface: u32, cb: u32) {
        let Some((w, h, tex)) = self.cbs.get(&cb).map(|c| (c.width, c.height, c.id)) else { return };
        let Some(s) = self.surfaces.get_mut(&surface) else { return };
        s.cb = cb;
        s.width = w;
        s.height = h;
        let (fbo, depth) = (s.fbo, s.depth);
        self.ops.op(Code::SurfaceAttach, &[fbo, tex, depth, w, h]);
    }

    fn make_current(&mut self, t: &mut Thread, ctx: u32, draw: u32, read: u32) -> bool {
        if ctx == 0 {
            *t = Thread { puid: t.puid, ..Thread::default() };
            return true;
        }
        if !self.ctxs.contains_key(&ctx) {
            return false;
        }
        t.ctx = ctx;
        t.draw = draw;
        t.read = read;
        let (dfbo, rfbo) = (self.surface_fbo(draw), self.surface_fbo(read));
        let size = self.surfaces.get(&draw).map(|s| (s.width as i32, s.height as i32));
        let c = self.ctxs.get_mut(&ctx).unwrap();
        c.draw = draw;
        c.read = read;
        if c.draw_default {
            c.state.draw_fbo = dfbo;
        }
        if c.read_default {
            c.state.read_fbo = rfbo;
        }
        if !c.sized
            && let Some((w, h)) = size
        {
            c.state.viewport = [0, 0, w, h];
            c.state.scissor = [0, 0, w, h];
            c.sized = true;
        }
        if self.active == ctx {
            // The WebGL context already reflects this context: bring it up to
            // date with the new surfaces.
            let to = self.ctxs[&ctx].state.clone();
            state::transition(&mut self.ops.buf(), &self.applied, &to);
            self.applied = to;
        }
        true
    }

    /// rcBindTexture: the texture bound to TEXTURE_2D on the active unit of
    /// the current context becomes the ColorBuffer's texture.
    fn bind_cb_texture(&mut self, t: &Thread, cb: u32) {
        let Some(tex) = self.cbs.get(&cb).map(|c| c.id) else { return };
        if !self.activate(t) {
            return;
        }
        let c = &self.ctxs[&t.ctx];
        let unit = c.state.active_texture as usize;
        let old = c.state.textures[unit][0];
        let share = c.share;
        let Some(s) = self.shares.get_mut(&share) else { return };
        let name = s.textures.guest_of(old);
        if name == 0 {
            return;
        }
        let was_alias = s.aliases.contains(&old);
        s.textures.insert(name, tex);
        s.aliases.insert(tex);
        if !was_alias {
            self.delete(Kind::Texture, old);
        }
        self.set(t, |st| st.textures[unit][0] = tex);
        self.ops.op(Code::BindTexture, &[GL_TEXTURE_2D, tex]);
    }

    /// rcBindRenderbuffer: the current renderbuffer is backed by the
    /// ColorBuffer (attachments of it attach the ColorBuffer's texture).
    fn bind_cb_renderbuffer(&mut self, t: &Thread, cb: u32) {
        let Some(tex) = self.cbs.get(&cb).map(|c| c.id) else { return };
        let Some(c) = self.ctxs.get(&t.ctx) else { return };
        let rb = c.state.renderbuffer;
        let share = c.share;
        if rb != 0
            && let Some(s) = self.shares.get_mut(&share)
        {
            s.rb_tex.insert(rb, tex);
        }
    }

    // ---- GLES ----

    fn names_in(a: &Args<'_, '_>, i: usize, n: usize) -> Vec<u32> {
        let mut v = a.u32s(i);
        v.truncate(n);
        v
    }

    /// Guest buffer name → (host id), on the ARRAY_BUFFER etc. binding.
    fn bind_buffer(&mut self, t: &Thread, target: u32, name: u32) {
        let id = self.buffer(t, name);
        if target == GL_ELEMENT_ARRAY_BUFFER {
            if let Some(c) = self.ctxs.get_mut(&t.ctx) {
                let vao = c.state.vao;
                c.vao_elements.insert(vao, id);
                let share = c.share;
                if id != 0
                    && let Some(s) = self.shares.get_mut(&share)
                {
                    s.element_buffers.insert(id);
                }
            }
            self.ops.op(Code::BindBuffer, &[target, id]);
            return;
        }
        let Some(slot) = state::buffer_slot(target) else {
            self.warn(format!("glBindBuffer target {target:#x}"));
            return;
        };
        self.set(t, |s| s.buffers[slot] = id);
        self.ops.op(Code::BindBuffer, &[target, id]);
    }

    fn bind_framebuffer(&mut self, t: &Thread, target: u32, name: u32) {
        let id = if name == 0 { 0 } else { self.framebuffer(t, name) };
        let Some(c) = self.ctxs.get_mut(&t.ctx) else { return };
        let (dfbo, rfbo) = (c.draw, c.read);
        let (dfbo, rfbo) = (self.surface_fbo(dfbo), self.surface_fbo(rfbo));
        let c = self.ctxs.get_mut(&t.ctx).unwrap();
        let draw = target == GL_FRAMEBUFFER || target == GL_DRAW_FRAMEBUFFER;
        let read = target == GL_FRAMEBUFFER || target == GL_READ_FRAMEBUFFER;
        if draw {
            c.draw_default = name == 0;
        }
        if read {
            c.read_default = name == 0;
        }
        let d = if name == 0 { dfbo } else { id };
        let r = if name == 0 { rfbo } else { id };
        self.set(t, |s| {
            if draw {
                s.draw_fbo = d;
            }
            if read {
                s.read_fbo = r;
            }
        });
        if draw {
            self.ops.op(Code::BindFramebuffer, &[GL_DRAW_FRAMEBUFFER, d]);
        }
        if read {
            self.ops.op(Code::BindFramebuffer, &[GL_READ_FRAMEBUFFER, r]);
        }
    }

    /// Links a program: declarations from the shaders' text, attribute
    /// locations fixed before the link (explicit bindings, `layout`, then
    /// the lowest free ones in declaration order), virtual uniform
    /// locations in declaration order.
    fn link(&mut self, t: &Thread, name: u32) {
        let Some(share) = self.share_of(t) else { return };
        let Some(p) = share.programs.get(&name) else { return };
        let mut vs = glsl::Shader::default();
        let mut fs = glsl::Shader::default();
        for sh in &p.shaders {
            if let Some(s) = share.shaders.get(sh) {
                if s.ty == GL_VERTEX_SHADER {
                    vs = s.scan.clone();
                } else {
                    fs = s.scan.clone();
                }
            }
        }
        let binds = p.binds.clone();
        let pid = p.id;
        // Uniforms: vertex first, then fragment ones not already seen.
        let mut uniforms: Vec<UniformLoc> = Vec::new();
        let mut next = 0i32;
        for v in vs.uniforms.iter().chain(&fs.uniforms) {
            if uniforms.iter().any(|u| u.var.name == v.name) {
                continue;
            }
            uniforms.push(UniformLoc { var: v.clone(), base: next });
            next += v.size as i32;
        }
        // Attributes.
        let mut used = [false; state::ATTRIBS + 8];
        let mut attribs: Vec<(glsl::Var, i32)> = Vec::new();
        let mut auto = Vec::new();
        for v in &vs.inputs {
            let slots = glsl::type_slots(v.ty) * v.size;
            let loc = v.location.or_else(|| binds.get(&v.name).copied());
            match loc {
                Some(l) => {
                    for k in l..l + slots {
                        if let Some(u) = used.get_mut(k as usize) {
                            *u = true;
                        }
                    }
                    attribs.push((v.clone(), l as i32));
                }
                None => {
                    auto.push(attribs.len());
                    attribs.push((v.clone(), -1));
                }
            }
        }
        for k in auto {
            let slots = (glsl::type_slots(attribs[k].0.ty) * attribs[k].0.size) as usize;
            let l =
                (0..state::ATTRIBS).find(|&l| (l..l + slots).all(|i| !used.get(i).copied().unwrap_or(true)));
            if let Some(l) = l {
                for u in &mut used[l..l + slots] {
                    *u = true;
                }
                attribs[k].1 = l as i32;
                let nm = attribs[k].0.name.clone();
                self.ops.op_blob(Code::BindAttribLocation, &[pid, l as u32], nm.as_bytes());
            }
        }
        for (nm, l) in &binds {
            self.ops.op_blob(Code::BindAttribLocation, &[pid, *l], nm.as_bytes());
        }
        self.ops.op(Code::LinkProgram, &[pid]);
        // The executor resolves virtual locations by name.
        let mut words = vec![pid];
        let mut names = Vec::new();
        for u in &uniforms {
            let (off, len) = (names.len() as u32, u.var.name.len() as u32);
            names.extend_from_slice(u.var.name.as_bytes());
            words.extend_from_slice(&[u.base as u32, u.var.size, off, len, u.var.array as u32]);
        }
        self.ops.op_blob(Code::ProgramUniforms, &words, &names);
        let blocks: Vec<glsl::Block> = vs.blocks.iter().chain(&fs.blocks).cloned().collect();
        if let Some(p) = self.program_mut(t, name) {
            p.linked = true;
            p.uniforms = uniforms;
            p.attribs = attribs;
            p.blocks = blocks;
            p.values.clear();
        }
    }

    fn uniform(
        &mut self,
        t: &Thread,
        code: Code,
        loc: i32,
        count: u32,
        comps: u32,
        data: &[u8],
        extra: &[u32],
    ) {
        if loc < 0 {
            return;
        }
        let Some((name, _)) = self.current_program(t) else { return };
        // Values kept for snapshots and glGetUniform*.
        let words: Vec<u32> = data.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect();
        if let Some(p) = self.program_mut(t, name) {
            let per = comps as usize;
            for k in 0..count as usize {
                if let Some(v) = words.get(k * per..(k + 1) * per) {
                    p.values.insert(loc + k as i32, v.to_vec());
                }
            }
        }
        let mut args = vec![loc as u32, count, comps];
        args.extend_from_slice(extra);
        self.ops.op_blob(code, &args, data);
    }

    fn uniform_scalar(&mut self, t: &Thread, code: Code, loc: i32, vals: &[u32]) {
        let data: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.uniform(t, code, loc, 1, vals.len() as u32, &data, &[]);
    }

    /// GL integer state for glGetIntegerv / glGetFloatv / glGetBooleanv.
    fn get_state(&mut self, t: &Thread, pname: u32) -> Option<Vec<f64>> {
        if let Some(v) = crate::caps::limit(pname) {
            return Some(v.iter().map(|&x| f64::from(x)).collect());
        }
        let c = self.ctx(t)?;
        let s = &c.state;
        let share = self.shares.get(&c.share)?;
        let f = |x: f32| f64::from(x);
        let u = |x: u32| f64::from(x);
        let i = |x: i32| f64::from(x);
        let b = |x: bool| if x { 1.0 } else { 0.0 };
        if let Some(k) = state::cap_index(pname) {
            return Some(vec![b(s.enabled[k])]);
        }
        if let Some(k) = state::pixel_store_index(pname) {
            return Some(vec![i(s.pixel_store[k])]);
        }
        let (draw_cfg, draw_default) = (self.surfaces.get(&c.draw).map_or(0, |sf| sf.config), c.draw_default);
        let cfg = crate::caps::CONFIGS.get(draw_cfg as usize).copied().unwrap_or(crate::caps::CONFIGS[0]);
        let unit = s.active_texture as usize;
        Some(match pname {
            0x821D => vec![crate::caps::gl_extension_list().len() as f64], // NUM_EXTENSIONS
            0x0BA2 => s.viewport.iter().map(|&v| i(v)).collect(),
            0x0C10 => s.scissor.iter().map(|&v| i(v)).collect(),
            0x0C22 => s.clear_color.iter().map(|&v| f(v)).collect(),
            0x0B73 => vec![f(s.clear_depth)],
            0x0B91 => vec![i(s.clear_stencil)],
            0x8005 => s.blend_color.iter().map(|&v| f(v)).collect(),
            0x8009 => vec![u(s.blend_eq[0])],
            0x883D => vec![u(s.blend_eq[1])],
            0x80C9 => vec![u(s.blend_func[0])],
            0x80C8 => vec![u(s.blend_func[1])],
            0x80CB => vec![u(s.blend_func[2])],
            0x80CA => vec![u(s.blend_func[3])],
            0x0C23 => s.color_mask.iter().map(|&v| b(v)).collect(),
            0x0B72 => vec![b(s.depth_mask)],
            0x0B98 => vec![u(s.stencil_writemask[0])],
            0x8CA5 => vec![u(s.stencil_writemask[1])],
            0x0B92 => vec![u(s.stencil_func[0].0)],
            0x0B97 => vec![i(s.stencil_func[0].1)],
            0x0B93 => vec![u(s.stencil_func[0].2)],
            0x8800 => vec![u(s.stencil_func[1].0)],
            0x8CA3 => vec![i(s.stencil_func[1].1)],
            0x8CA4 => vec![u(s.stencil_func[1].2)],
            0x0B94 => vec![u(s.stencil_op[0].0)],
            0x0B95 => vec![u(s.stencil_op[0].1)],
            0x0B96 => vec![u(s.stencil_op[0].2)],
            0x8801 => vec![u(s.stencil_op[1].0)],
            0x8802 => vec![u(s.stencil_op[1].1)],
            0x8803 => vec![u(s.stencil_op[1].2)],
            0x0B45 => vec![u(s.cull_face)],
            0x0B46 => vec![u(s.front_face)],
            0x0B74 => vec![u(s.depth_func)],
            0x0B70 => s.depth_range.iter().map(|&v| f(v)).collect(),
            0x0B21 => vec![f(s.line_width)],
            0x8038 => vec![f(s.polygon_offset[0])],
            0x2A00 => vec![f(s.polygon_offset[1])],
            0x80AA => vec![f(s.sample_coverage.0)],
            0x80AB => vec![b(s.sample_coverage.1)],
            0x8192 => vec![u(s.hints[0])],
            0x8B8B => vec![u(s.hints[1])],
            0x84E0 => vec![u(0x84C0 + s.active_texture)],
            0x8069 => vec![u(share.textures.guest_of(s.textures[unit][0]))],
            0x8514 => vec![u(share.textures.guest_of(s.textures[unit][1]))],
            0x806A => vec![u(share.textures.guest_of(s.textures[unit][2]))],
            0x8C1D => vec![u(share.textures.guest_of(s.textures[unit][3]))],
            0x8919 => vec![u(share.samplers.guest_of(s.samplers[unit]))],
            0x8894 => vec![u(share.buffers.guest_of(s.buffers[0]))],
            0x8895 => {
                let e = c.vao_elements.get(&s.vao).copied().unwrap_or(0);
                vec![u(share.buffers.guest_of(e))]
            }
            0x8F36 => vec![u(share.buffers.guest_of(s.buffers[1]))],
            0x8F37 => vec![u(share.buffers.guest_of(s.buffers[2]))],
            0x88ED => vec![u(share.buffers.guest_of(s.buffers[3]))],
            0x88EF => vec![u(share.buffers.guest_of(s.buffers[4]))],
            0x8A28 => vec![u(share.buffers.guest_of(s.buffers[5]))],
            0x8C8F => vec![u(share.buffers.guest_of(s.buffers[6]))],
            0x8CA6 => vec![if c.draw_default { 0.0 } else { u(c.fbos.guest_of(s.draw_fbo)) }],
            0x8CAA => vec![if c.read_default { 0.0 } else { u(c.fbos.guest_of(s.read_fbo)) }],
            0x8CA7 => vec![u(share.renderbuffers.guest_of(s.renderbuffer))],
            0x8B8D => vec![u(share.sp.guest_of(s.program))],
            0x85B5 => vec![if s.vao == c.default_vao { 0.0 } else { u(c.vaos.guest_of(s.vao)) }],
            0x8E25 => vec![u(c.tfbs.guest_of(s.transform_feedback))],
            // Framebuffer bits: the surface's config for framebuffer 0.
            0x0D52 => vec![i(if draw_default { cfg.r } else { 8 })],
            0x0D53 => vec![i(if draw_default { cfg.g } else { 8 })],
            0x0D54 => vec![i(if draw_default { cfg.b } else { 8 })],
            0x0D55 => vec![i(if draw_default { cfg.a } else { 8 })],
            0x0D56 => vec![i(if draw_default { cfg.depth } else { 24 })],
            0x0D57 => vec![i(if draw_default { cfg.stencil } else { 8 })],
            0x80A8 | 0x80A9 => vec![0.0], // SAMPLE_BUFFERS, SAMPLES
            0x0C02 => vec![u(0x0405)],    // READ_BUFFER: BACK
            0x8825..=0x8828 => vec![u(if pname == 0x8825 { 0x0405 } else { 0 })], // DRAW_BUFFERi
            0x84FD => vec![2.0],
            _ => return None,
        })
    }

    /// One GLES call. Returns false for opcodes it does not know.
    fn gles(&mut self, t: &mut Thread, op: u32, a: &Args<'_, '_>, r: &mut Reply) {
        if !self.activate(t) {
            // No current context: queries still get their (zero) reply.
            return;
        }
        self.stats.calls += 1;
        self.track_attrib_pointer(t, op, a);
        match op {
            // ---- context state ----
            g::glActiveTexture => {
                let unit = a.u(0).wrapping_sub(0x84C0).min(state::UNITS as u32 - 1);
                self.set(t, |s| s.active_texture = unit);
                self.ops.op(Code::ActiveTexture, &[0x84C0 + unit]);
            }
            g::glEnable | g::glDisable => {
                let on = op == g::glEnable;
                let cap = a.u(0);
                if let Some(k) = state::cap_index(cap) {
                    self.set(t, |s| s.enabled[k] = on);
                    self.ops.op(if on { Code::Enable } else { Code::Disable }, &[cap]);
                }
            }
            g::glIsEnabled => {
                r.ret = state::cap_index(a.u(0))
                    .and_then(|k| self.ctx(t).map(|c| c.state.enabled[k]))
                    .unwrap_or(false) as u64;
            }
            g::glBlendColor => {
                let c = [a.f(0), a.f(1), a.f(2), a.f(3)];
                self.set(t, |s| s.blend_color = c);
                self.ops.op(Code::BlendColor, &[fw(c[0]), fw(c[1]), fw(c[2]), fw(c[3])]);
            }
            g::glBlendEquation | g::glBlendEquationSeparate => {
                let e = if op == g::glBlendEquation { [a.u(0), a.u(0)] } else { [a.u(0), a.u(1)] };
                self.set(t, |s| s.blend_eq = e);
                self.ops.op(Code::BlendEquationSeparate, &e);
            }
            g::glBlendFunc | g::glBlendFuncSeparate => {
                let f = if op == g::glBlendFunc {
                    [a.u(0), a.u(1), a.u(0), a.u(1)]
                } else {
                    [a.u(0), a.u(1), a.u(2), a.u(3)]
                };
                self.set(t, |s| s.blend_func = f);
                self.ops.op(Code::BlendFuncSeparate, &f);
            }
            g::glColorMask => {
                let m = [a.b(0), a.b(1), a.b(2), a.b(3)];
                self.set(t, |s| s.color_mask = m);
                self.ops.op(Code::ColorMask, &m.map(u32::from));
            }
            g::glDepthMask => {
                let m = a.b(0);
                self.set(t, |s| s.depth_mask = m);
                self.ops.op(Code::DepthMask, &[m as u32]);
            }
            g::glStencilMask | g::glStencilMaskSeparate => {
                let (face, m) = if op == g::glStencilMask { (0x0408, a.u(0)) } else { (a.u(0), a.u(1)) };
                self.set(t, |s| {
                    if face != 0x0405 {
                        s.stencil_writemask[0] = m;
                    }
                    if face != 0x0404 {
                        s.stencil_writemask[1] = m;
                    }
                });
                self.ops.op(Code::StencilMaskSeparate, &[face, m]);
            }
            g::glStencilFunc | g::glStencilFuncSeparate => {
                let (face, f, rf, m) = if op == g::glStencilFunc {
                    (0x0408, a.u(0), a.i(1), a.u(2))
                } else {
                    (a.u(0), a.u(1), a.i(2), a.u(3))
                };
                self.set(t, |s| {
                    if face != 0x0405 {
                        s.stencil_func[0] = (f, rf, m);
                    }
                    if face != 0x0404 {
                        s.stencil_func[1] = (f, rf, m);
                    }
                });
                self.ops.op(Code::StencilFuncSeparate, &[face, f, rf as u32, m]);
            }
            g::glStencilOp | g::glStencilOpSeparate => {
                let (face, x, y, z) = if op == g::glStencilOp {
                    (0x0408, a.u(0), a.u(1), a.u(2))
                } else {
                    (a.u(0), a.u(1), a.u(2), a.u(3))
                };
                self.set(t, |s| {
                    if face != 0x0405 {
                        s.stencil_op[0] = (x, y, z);
                    }
                    if face != 0x0404 {
                        s.stencil_op[1] = (x, y, z);
                    }
                });
                self.ops.op(Code::StencilOpSeparate, &[face, x, y, z]);
            }
            g::glClearColor => {
                let c = [a.f(0), a.f(1), a.f(2), a.f(3)];
                self.set(t, |s| s.clear_color = c);
                self.ops.op(Code::ClearColor, &[fw(c[0]), fw(c[1]), fw(c[2]), fw(c[3])]);
            }
            g::glClearDepthf => {
                let d = a.f(0);
                self.set(t, |s| s.clear_depth = d);
                self.ops.op(Code::ClearDepth, &[fw(d)]);
            }
            g::glClearStencil => {
                let v = a.i(0);
                self.set(t, |s| s.clear_stencil = v);
                self.ops.op(Code::ClearStencil, &[v as u32]);
            }
            g::glCullFace => {
                let v = a.u(0);
                self.set(t, |s| s.cull_face = v);
                self.ops.op(Code::CullFace, &[v]);
            }
            g::glFrontFace => {
                let v = a.u(0);
                self.set(t, |s| s.front_face = v);
                self.ops.op(Code::FrontFace, &[v]);
            }
            g::glDepthFunc => {
                let v = a.u(0);
                self.set(t, |s| s.depth_func = v);
                self.ops.op(Code::DepthFunc, &[v]);
            }
            g::glDepthRangef => {
                let v = [a.f(0), a.f(1)];
                self.set(t, |s| s.depth_range = v);
                self.ops.op(Code::DepthRange, &[fw(v[0]), fw(v[1])]);
            }
            g::glLineWidth => {
                let v = a.f(0);
                self.set(t, |s| s.line_width = v);
                self.ops.op(Code::LineWidth, &[fw(v)]);
            }
            g::glPolygonOffset => {
                let v = [a.f(0), a.f(1)];
                self.set(t, |s| s.polygon_offset = v);
                self.ops.op(Code::PolygonOffset, &[fw(v[0]), fw(v[1])]);
            }
            g::glSampleCoverage => {
                let v = (a.f(0), a.b(1));
                self.set(t, |s| s.sample_coverage = v);
                self.ops.op(Code::SampleCoverage, &[fw(v.0), v.1 as u32]);
            }
            g::glScissor => {
                let v = [a.i(0), a.i(1), a.i(2), a.i(3)];
                self.set(t, |s| s.scissor = v);
                self.ops.op(Code::Scissor, &v.map(|x| x as u32));
            }
            g::glViewport => {
                let v = [a.i(0), a.i(1), a.i(2), a.i(3)];
                self.set(t, |s| s.viewport = v);
                self.ops.op(Code::Viewport, &v.map(|x| x as u32));
            }
            g::glHint => {
                let k = match a.u(0) {
                    0x8192 => Some(0),
                    0x8B8B => Some(1),
                    _ => None,
                };
                if let Some(k) = k {
                    let v = a.u(1);
                    self.set(t, |s| s.hints[k] = v);
                    self.ops.op(Code::Hint, &[a.u(0), v]);
                }
            }
            g::glPixelStorei => {
                if let Some(k) = state::pixel_store_index(a.u(0)) {
                    let v = a.i(1);
                    self.set(t, |s| s.pixel_store[k] = v);
                    self.ops.op(Code::PixelStorei, &[a.u(0), v as u32]);
                }
            }
            g::glVertexAttrib1f | g::glVertexAttrib2f | g::glVertexAttrib3f | g::glVertexAttrib4f => {
                let n = (op - g::glVertexAttrib1f) as usize / 2 + 1;
                let mut v = [0u32, 0, 0, 1f32.to_bits()];
                for (k, x) in v.iter_mut().enumerate().take(n) {
                    *x = a.u(1 + k);
                }
                self.vertex_attrib(t, a.u(0), 0, v);
            }
            g::glVertexAttrib1fv | g::glVertexAttrib2fv | g::glVertexAttrib3fv | g::glVertexAttrib4fv => {
                let n = (op - g::glVertexAttrib1fv) as usize / 2 + 1;
                let mut v = [0u32, 0, 0, 1f32.to_bits()];
                for (k, x) in a.u32s(1).into_iter().take(n).enumerate() {
                    v[k] = x;
                }
                self.vertex_attrib(t, a.u(0), 0, v);
            }
            g::glVertexAttribI4i | g::glVertexAttribI4ui => {
                let v = [a.u(1), a.u(2), a.u(3), a.u(4)];
                self.vertex_attrib(t, a.u(0), if op == g::glVertexAttribI4i { 1 } else { 2 }, v);
            }
            g::glVertexAttribI4iv | g::glVertexAttribI4uiv => {
                let mut v = [0u32; 4];
                for (k, x) in a.u32s(1).into_iter().take(4).enumerate() {
                    v[k] = x;
                }
                self.vertex_attrib(t, a.u(0), if op == g::glVertexAttribI4iv { 1 } else { 2 }, v);
            }
            // ---- bindings ----
            g::glBindTexture => {
                let id = self.texture(t, a.u(1));
                if let Some(slot) = state::tex_slot(a.u(0)) {
                    let unit = self.ctx(t).map_or(0, |c| c.state.active_texture) as usize;
                    self.set(t, |s| s.textures[unit][slot] = id);
                    self.ops.op(Code::BindTexture, &[state::TEX_TARGETS[slot], id]);
                }
            }
            g::glBindBuffer => self.bind_buffer(t, a.u(0), a.u(1)),
            g::glBindBufferRange | g::glBindBufferBase => {
                let (target, index, name) = (a.u(0), a.u(1), a.u(2));
                let (off, size) =
                    if op == g::glBindBufferRange { (i64::from(a.i(3)), i64::from(a.i(4))) } else { (0, 0) };
                let id = self.buffer(t, name);
                if target == 0x8A11 && (index as usize) < state::UBO_BINDINGS {
                    self.set(t, |s| {
                        s.ubo[index as usize] = (id, off, size);
                        s.buffers[5] = id;
                    });
                } else if let Some(slot) = state::buffer_slot(target) {
                    self.set(t, |s| s.buffers[slot] = id);
                }
                self.ops.op(Code::BindBufferRange, &[target, index, id, off as u32, size as u32]);
            }
            g::glBindFramebuffer => self.bind_framebuffer(t, a.u(0), a.u(1)),
            g::glBindRenderbuffer => {
                let id = self.renderbuffer(t, a.u(1));
                self.set(t, |s| s.renderbuffer = id);
                self.ops.op(Code::BindRenderbuffer, &[id]);
            }
            g::glBindVertexArray | g::glBindVertexArrayOES => {
                let id = self.vertex_array(t, a.u(0));
                self.set(t, |s| s.vao = id);
                self.ops.op(Code::BindVertexArray, &[id]);
            }
            g::glBindSampler => {
                let unit = a.u(0).min(state::UNITS as u32 - 1);
                let id = self.sampler(t, a.u(1));
                self.set(t, |s| s.samplers[unit as usize] = id);
                self.ops.op(Code::BindSampler, &[unit, id]);
            }
            g::glBindTransformFeedback => {
                let id = self.transform_feedback(t, a.u(1));
                self.set(t, |s| s.transform_feedback = id);
                self.ops.op(Code::BindTransformFeedback, &[id]);
            }
            g::glUseProgram => {
                let name = a.u(0);
                let id = self.share_of(t).and_then(|s| s.programs.get(&name)).map_or(0, |p| p.id);
                self.set(t, |s| s.program = id);
                self.ops.op(Code::UseProgram, &[id]);
            }
            // ---- names ----
            g::glGenTextures
            | g::glGenBuffers
            | g::glGenRenderbuffers
            | g::glGenFramebuffers
            | g::glGenVertexArrays
            | g::glGenVertexArraysOES
            | g::glGenSamplers
            | g::glGenQueries
            | g::glGenTransformFeedbacks => {
                let n = a.i(0).max(0) as usize;
                let mut names = Vec::with_capacity(n);
                for _ in 0..n.min(r.out(0).len() / 4) {
                    names.push(self.gen_name(t, op));
                }
                r.out_u32s(0, &names);
            }
            g::glDeleteTextures
            | g::glDeleteBuffers
            | g::glDeleteRenderbuffers
            | g::glDeleteFramebuffers
            | g::glDeleteVertexArrays
            | g::glDeleteVertexArraysOES
            | g::glDeleteSamplers
            | g::glDeleteQueries
            | g::glDeleteTransformFeedbacks => {
                for name in Self::names_in(a, 1, a.i(0).max(0) as usize) {
                    self.delete_name(t, op, name);
                }
            }
            g::glIsTexture => {
                r.ret = self.share_of(t).is_some_and(|s| s.textures.get(a.u(0)).is_some()) as u64
            }
            g::glIsBuffer => r.ret = self.share_of(t).is_some_and(|s| s.buffers.get(a.u(0)).is_some()) as u64,
            g::glIsRenderbuffer => {
                r.ret = self.share_of(t).is_some_and(|s| s.renderbuffers.get(a.u(0)).is_some()) as u64
            }
            g::glIsSampler => {
                r.ret = self.share_of(t).is_some_and(|s| s.samplers.get(a.u(0)).is_some()) as u64
            }
            g::glIsFramebuffer => r.ret = self.ctx(t).is_some_and(|c| c.fbos.get(a.u(0)).is_some()) as u64,
            g::glIsVertexArray | g::glIsVertexArrayOES => {
                r.ret = self.ctx(t).is_some_and(|c| c.vaos.get(a.u(0)).is_some()) as u64
            }
            g::glIsQuery => r.ret = self.ctx(t).is_some_and(|c| c.queries.get(a.u(0)).is_some()) as u64,
            g::glIsTransformFeedback => {
                r.ret = self.ctx(t).is_some_and(|c| c.tfbs.get(a.u(0)).is_some()) as u64
            }
            // ---- shaders and programs ----
            g::glCreateShader => {
                let ty = a.u(0);
                let id = self.create(Kind::Shader, ty);
                let Some(s) = self.share_of(t) else { return };
                let name = s.sp.fresh();
                s.sp.insert(name, id);
                s.shaders.insert(
                    name,
                    ShaderObj {
                        id,
                        ty,
                        source: String::new(),
                        scan: glsl::Shader::default(),
                        delete_pending: false,
                        attached: 0,
                    },
                );
                r.ret = u64::from(name);
            }
            g::glCreateProgram => {
                let id = self.create(Kind::Program, 0);
                let Some(s) = self.share_of(t) else { return };
                let name = s.sp.fresh();
                s.sp.insert(name, id);
                s.programs.insert(name, ProgramObj { id, ..ProgramObj::default() });
                r.ret = u64::from(name);
            }
            g::glShaderString | g::glShaderSource => {
                let name = a.u(0);
                let src = String::from_utf8_lossy(a.str(1)).into_owned();
                let Some(s) = self.share_of(t) else { return };
                let Some(sh) = s.shaders.get_mut(&name) else { return };
                let vertex = sh.ty == GL_VERTEX_SHADER;
                sh.scan = glsl::scan(&src, vertex);
                sh.source = src;
                let id = sh.id;
                let text = glsl::rewrite_for_webgl(&sh.source);
                self.ops.op_blob(Code::ShaderSource, &[id], text.as_bytes());
            }
            g::glCompileShader => {
                if let Some(id) = self.share_of(t).and_then(|s| s.shaders.get(&a.u(0))).map(|s| s.id) {
                    self.ops.op(Code::CompileShader, &[id]);
                }
            }
            g::glAttachShader | g::glDetachShader => {
                let (p, sh) = (a.u(0), a.u(1));
                let Some(s) = self.share_of(t) else { return };
                let (Some(pid), Some(sid)) =
                    (s.programs.get(&p).map(|x| x.id), s.shaders.get(&sh).map(|x| x.id))
                else {
                    return;
                };
                let attach = op == g::glAttachShader;
                let prog = s.programs.get_mut(&p).unwrap();
                if attach {
                    if !prog.shaders.contains(&sh) {
                        prog.shaders.push(sh);
                        s.shaders.get_mut(&sh).unwrap().attached += 1;
                    }
                } else if let Some(k) = prog.shaders.iter().position(|&x| x == sh) {
                    prog.shaders.remove(k);
                    let so = s.shaders.get_mut(&sh).unwrap();
                    so.attached = so.attached.saturating_sub(1);
                    if so.attached == 0 && so.delete_pending {
                        s.shaders.remove(&sh);
                        s.sp.remove(sh);
                        self.ops.op(Code::DetachShader, &[pid, sid]);
                        self.delete(Kind::Shader, sid);
                        return;
                    }
                }
                self.ops.op(if attach { Code::AttachShader } else { Code::DetachShader }, &[pid, sid]);
            }
            g::glDeleteShader => {
                let sh = a.u(0);
                let Some(s) = self.share_of(t) else { return };
                let Some(so) = s.shaders.get_mut(&sh) else { return };
                if so.attached > 0 {
                    so.delete_pending = true;
                } else {
                    let id = so.id;
                    s.shaders.remove(&sh);
                    s.sp.remove(sh);
                    self.delete(Kind::Shader, id);
                }
            }
            g::glDeleteProgram => {
                let p = a.u(0);
                let current = self.ctx(t).map_or(0, |c| c.state.program);
                let Some(s) = self.share_of(t) else { return };
                let Some(po) = s.programs.get(&p) else { return };
                if po.id == current {
                    s.programs.get_mut(&p).unwrap().delete_pending = true;
                    return;
                }
                let po = s.programs.remove(&p).unwrap();
                s.sp.remove(p);
                let mut freed = Vec::new();
                for sh in &po.shaders {
                    if let Some(so) = s.shaders.get_mut(sh) {
                        so.attached = so.attached.saturating_sub(1);
                        if so.attached == 0 && so.delete_pending {
                            freed.push((*sh, so.id));
                        }
                    }
                }
                for (sh, _) in &freed {
                    s.shaders.remove(sh);
                    s.sp.remove(*sh);
                }
                self.delete(Kind::Program, po.id);
                for (_, id) in freed {
                    self.delete(Kind::Shader, id);
                }
            }
            g::glIsShader => r.ret = self.share_of(t).is_some_and(|s| s.shaders.contains_key(&a.u(0))) as u64,
            g::glIsProgram => {
                r.ret = self.share_of(t).is_some_and(|s| s.programs.contains_key(&a.u(0))) as u64
            }
            g::glBindAttribLocation => {
                let (p, idx) = (a.u(0), a.u(1));
                let name = String::from_utf8_lossy(a.str(2)).into_owned();
                if let Some(po) = self.program_mut(t, p) {
                    po.binds.insert(name, idx);
                }
            }
            g::glLinkProgram => self.link(t, a.u(0)),
            g::glValidateProgram | g::glReleaseShaderCompiler => {}
            g::glGetShaderiv => {
                let (sh, pname) = (a.u(0), a.u(1));
                let v = self.share_of(t).and_then(|s| s.shaders.get(&sh)).map(|so| match pname {
                    0x8B4F => so.ty as i32,               // SHADER_TYPE
                    0x8B80 => so.delete_pending as i32,   // DELETE_STATUS
                    0x8B81 => 1,                          // COMPILE_STATUS
                    0x8B84 => 1,                          // INFO_LOG_LENGTH ("\0")
                    0x8B88 => so.source.len() as i32 + 1, // SHADER_SOURCE_LENGTH
                    _ => 0,
                });
                r.out_i32s(0, &[v.unwrap_or(0)]);
            }
            g::glGetShaderInfoLog | g::glGetProgramInfoLog => {
                // Empty logs (host compiler messages stay on the host).
                r.out_i32s(0, &[0]);
                r.out_str(1, b"");
            }
            g::glGetShaderSource => {
                let src = self
                    .share_of(t)
                    .and_then(|s| s.shaders.get(&a.u(0)))
                    .map(|s| s.source.clone())
                    .unwrap_or_default();
                let n = r.out_str(1, src.as_bytes());
                r.out_i32s(0, &[n as i32]);
            }
            g::glGetProgramiv => {
                let (p, pname) = (a.u(0), a.u(1));
                let v = self.share_of(t).and_then(|s| s.programs.get(&p)).map(|po| match pname {
                    0x8B80 => po.delete_pending as i32, // DELETE_STATUS
                    0x8B82 => 1,                        // LINK_STATUS
                    0x8B83 => 1,                        // VALIDATE_STATUS
                    0x8B84 => 1,                        // INFO_LOG_LENGTH
                    0x8B85 => po.shaders.len() as i32,  // ATTACHED_SHADERS
                    0x8B86 => po.uniforms.len() as i32, // ACTIVE_UNIFORMS
                    0x8B87 => po.uniforms.iter().map(|u| u.var.name.len() as i32 + 4).max().unwrap_or(0),
                    0x8B89 => po.attribs.len() as i32, // ACTIVE_ATTRIBUTES
                    0x8B8A => po.attribs.iter().map(|a| a.0.name.len() as i32 + 1).max().unwrap_or(0),
                    0x8A36 => po.blocks.len() as i32, // ACTIVE_UNIFORM_BLOCKS
                    0x8A35 => po.blocks.iter().map(|b| b.name.len() as i32 + 4).max().unwrap_or(0),
                    0x8C83 => 0,      // TRANSFORM_FEEDBACK_VARYINGS
                    0x8C7F => 0x8C8C, // TRANSFORM_FEEDBACK_BUFFER_MODE
                    0x8C76 => 0,      // ..._VARYING_MAX_LENGTH
                    0x8741 => 0,      // PROGRAM_BINARY_LENGTH
                    _ => 0,
                });
                r.out_i32s(0, &[v.unwrap_or(0)]);
            }
            g::glGetActiveUniform | g::glGetActiveAttrib => {
                let (p, index) = (a.u(0), a.u(1) as usize);
                let found = self.share_of(t).and_then(|s| s.programs.get(&p)).and_then(|po| {
                    if op == g::glGetActiveUniform {
                        po.uniforms.get(index).map(|u| {
                            let n =
                                if u.var.array { format!("{}[0]", u.var.name) } else { u.var.name.clone() };
                            (n, u.var.size, u.var.ty)
                        })
                    } else {
                        po.attribs.get(index).map(|(v, _)| (v.name.clone(), v.size, v.ty))
                    }
                });
                if let Some((name, size, ty)) = found {
                    let n = r.out_str(3, name.as_bytes());
                    r.out_i32s(0, &[n as i32]);
                    r.out_i32s(1, &[size as i32]);
                    r.out_u32s(2, &[ty]);
                }
            }
            g::glGetUniformLocation => {
                let name = String::from_utf8_lossy(a.str(1)).into_owned();
                let loc =
                    self.share_of(t).and_then(|s| s.programs.get(&a.u(0))).map_or(-1, |p| p.location(&name));
                r.ret = loc as u32 as u64;
            }
            g::glGetAttribLocation => {
                let name = String::from_utf8_lossy(a.str(1)).into_owned();
                let loc = self
                    .share_of(t)
                    .and_then(|s| s.programs.get(&a.u(0)))
                    .and_then(|p| p.attribs.iter().find(|(v, _)| v.name == name).map(|(_, l)| *l))
                    .unwrap_or(-1);
                r.ret = loc as u32 as u64;
            }
            g::glGetFragDataLocation => r.ret = 0,
            g::glGetAttachedShaders => {
                let shaders = self
                    .share_of(t)
                    .and_then(|s| s.programs.get(&a.u(0)))
                    .map(|p| p.shaders.clone())
                    .unwrap_or_default();
                let max = a.i(1).max(0) as usize;
                let n = shaders.len().min(max);
                r.out_i32s(0, &[n as i32]);
                r.out_u32s(1, &shaders[..n]);
            }
            g::glGetShaderPrecisionFormat => {
                let (range, prec) = match a.u(1) {
                    0x8DF0..=0x8DF2 => ([127, 127], 23), // LOW/MEDIUM/HIGH_FLOAT
                    _ => ([31, 30], 0),                  // ints
                };
                r.out_i32s(0, &range);
                r.out_i32s(1, &[prec]);
            }
            g::glGetUniformBlockIndex => {
                let name = String::from_utf8_lossy(a.str(1)).into_owned();
                let idx = self
                    .share_of(t)
                    .and_then(|s| s.programs.get(&a.u(0)))
                    .and_then(|p| p.blocks.iter().position(|b| b.name == name))
                    .map_or(u32::MAX, |i| i as u32);
                r.ret = u64::from(idx);
            }
            g::glUniformBlockBinding => {
                if let Some(p) = self.program_mut(t, a.u(0)) {
                    p.block_bindings.insert(a.u(1), a.u(2));
                }
                if let Some(pid) = self.share_of(t).and_then(|s| s.programs.get(&a.u(0))).map(|p| p.id) {
                    self.ops.op(Code::UniformBlockBinding, &[pid, a.u(1), a.u(2)]);
                }
            }
            g::glGetActiveUniformBlockiv => {
                let blk = self
                    .share_of(t)
                    .and_then(|s| s.programs.get(&a.u(0)))
                    .and_then(|p| p.blocks.get(a.u(1) as usize).cloned());
                let v = blk.map_or(0, |b| match a.u(2) {
                    0x8A3F => b.binding.unwrap_or(0) as i32, // UNIFORM_BLOCK_BINDING
                    0x8A40 => b.members.iter().map(|m| 16 * m.size as i32 * 4).sum(), // DATA_SIZE (std140 bound)
                    0x8A41 => b.name.len() as i32 + 1,                                // NAME_LENGTH
                    0x8A42 => b.members.len() as i32,                                 // ACTIVE_UNIFORMS
                    0x8A44 | 0x8A46 => 1,                                             // REFERENCED_BY_*
                    _ => 0,
                });
                r.out_i32s(0, &[v]);
            }
            g::glGetActiveUniformBlockName => {
                let name = self
                    .share_of(t)
                    .and_then(|s| s.programs.get(&a.u(0)))
                    .and_then(|p| p.blocks.get(a.u(1) as usize).map(|b| b.name.clone()))
                    .unwrap_or_default();
                let n = r.out_str(1, name.as_bytes());
                r.out_i32s(0, &[n as i32]);
            }
            g::glGetUniformIndicesAEMU | g::glGetActiveUniformsiv => {
                self.warn(format!("{} answered with zeros", GLES2[(op - GLES2_BASE) as usize].name));
            }
            g::glGetUniformfv | g::glGetUniformiv | g::glGetUniformuiv => {
                let v = self
                    .share_of(t)
                    .and_then(|s| s.programs.get(&a.u(0)))
                    .and_then(|p| p.values.get(&a.i(1)).cloned())
                    .unwrap_or_default();
                r.out_u32s(0, &v);
            }
            g::glUniform1f | g::glUniform2f | g::glUniform3f | g::glUniform4f => {
                let n = [g::glUniform1f, g::glUniform2f, g::glUniform3f, g::glUniform4f]
                    .iter()
                    .position(|&x| x == op)
                    .unwrap()
                    + 1;
                let v: Vec<u32> = (0..n).map(|k| a.u(1 + k)).collect();
                self.uniform_scalar(t, Code::Uniformfv, a.i(0), &v);
            }
            g::glUniform1i | g::glUniform2i | g::glUniform3i | g::glUniform4i => {
                let n = [g::glUniform1i, g::glUniform2i, g::glUniform3i, g::glUniform4i]
                    .iter()
                    .position(|&x| x == op)
                    .unwrap()
                    + 1;
                let v: Vec<u32> = (0..n).map(|k| a.u(1 + k)).collect();
                self.uniform_scalar(t, Code::Uniformiv, a.i(0), &v);
            }
            g::glUniform1ui | g::glUniform2ui | g::glUniform3ui | g::glUniform4ui => {
                let n = [g::glUniform1ui, g::glUniform2ui, g::glUniform3ui, g::glUniform4ui]
                    .iter()
                    .position(|&x| x == op)
                    .unwrap()
                    + 1;
                let v: Vec<u32> = (0..n).map(|k| a.u(1 + k)).collect();
                self.uniform_scalar(t, Code::Uniformuiv, a.i(0), &v);
            }
            g::glUniform1fv | g::glUniform2fv | g::glUniform3fv | g::glUniform4fv => {
                let n = [g::glUniform1fv, g::glUniform2fv, g::glUniform3fv, g::glUniform4fv]
                    .iter()
                    .position(|&x| x == op)
                    .unwrap() as u32
                    + 1;
                self.uniform(t, Code::Uniformfv, a.i(0), a.u(1), n, a.bytes(2), &[]);
            }
            g::glUniform1iv | g::glUniform2iv | g::glUniform3iv | g::glUniform4iv => {
                let n = [g::glUniform1iv, g::glUniform2iv, g::glUniform3iv, g::glUniform4iv]
                    .iter()
                    .position(|&x| x == op)
                    .unwrap() as u32
                    + 1;
                self.uniform(t, Code::Uniformiv, a.i(0), a.u(1), n, a.bytes(2), &[]);
            }
            g::glUniform1uiv | g::glUniform2uiv | g::glUniform3uiv | g::glUniform4uiv => {
                let n = [g::glUniform1uiv, g::glUniform2uiv, g::glUniform3uiv, g::glUniform4uiv]
                    .iter()
                    .position(|&x| x == op)
                    .unwrap() as u32
                    + 1;
                self.uniform(t, Code::Uniformuiv, a.i(0), a.u(1), n, a.bytes(2), &[]);
            }
            g::glUniformMatrix2fv
            | g::glUniformMatrix3fv
            | g::glUniformMatrix4fv
            | g::glUniformMatrix2x3fv
            | g::glUniformMatrix3x2fv
            | g::glUniformMatrix2x4fv
            | g::glUniformMatrix4x2fv
            | g::glUniformMatrix3x4fv
            | g::glUniformMatrix4x3fv => {
                // (columns, rows) of each matrix type.
                let (c, rw) = match op {
                    g::glUniformMatrix2fv => (2, 2),
                    g::glUniformMatrix3fv => (3, 3),
                    g::glUniformMatrix4fv => (4, 4),
                    g::glUniformMatrix2x3fv => (2, 3),
                    g::glUniformMatrix3x2fv => (3, 2),
                    g::glUniformMatrix2x4fv => (2, 4),
                    g::glUniformMatrix4x2fv => (4, 2),
                    g::glUniformMatrix3x4fv => (3, 4),
                    _ => (4, 3),
                };
                self.uniform(t, Code::UniformMatrixfv, a.i(0), a.u(1), c * rw, a.bytes(3), &[c, rw, a.u(2)]);
            }
            // ---- buffers ----
            g::glBufferData => {
                let (target, size, usage) = (a.u(0), a.i(1), a.u(3));
                let data = a.bytes(2);
                self.note_buffer_size(t, target, i64::from(size));
                self.note_buffer_usage(t, target, usage);
                self.ops.op_blob_then(Code::BufferData, &[target, size as u32], data, &[usage]);
            }
            g::glBufferDataSyncAEMU => {
                let (target, size, usage) = (a.u(0), a.i(1), a.u(3));
                self.note_buffer_size(t, target, i64::from(size));
                self.note_buffer_usage(t, target, usage);
                self.ops.op_blob_then(Code::BufferData, &[target, size as u32], a.bytes(2), &[usage]);
                r.ret = 1;
            }
            g::glBufferSubData => {
                self.ops.op_blob(Code::BufferSubData, &[a.u(0), a.u(1)], a.bytes(3));
            }
            g::glCopyBufferSubData => {
                self.ops.op(Code::CopyBufferSubData, &[a.u(0), a.u(1), a.u(2), a.u(3), a.u(4)]);
            }
            g::glMapBufferRangeAEMU => {
                let (target, off, len, access) = (a.u(0), a.u(1), a.u(2), a.u(3));
                if access & 0x1 != 0 && !r.out(0).is_empty() {
                    // MAP_READ_BIT: a readback.
                    self.ops.op_read(Code::GetBufferSubData, &[target, off, len], len as usize);
                    let data = self.flush();
                    let o = r.out(0);
                    let n = o.len().min(data.len());
                    o[..n].copy_from_slice(&data[..n]);
                }
            }
            g::glUnmapBufferAEMU | g::glUnmapBufferAsyncAEMU => {
                let (target, off, access) = (a.u(0), a.u(1), a.u(3));
                // MAP_WRITE_BIT without MAP_FLUSH_EXPLICIT_BIT: the whole range.
                if access & 0x2 != 0 && access & 0x10 == 0 {
                    self.ops.op_blob(Code::BufferSubData, &[target, off], a.bytes(4));
                }
                if op == g::glUnmapBufferAEMU {
                    r.out(0).iter_mut().take(1).for_each(|b| *b = 1);
                }
            }
            g::glFlushMappedBufferRangeAEMU | g::glFlushMappedBufferRangeAEMU2 => {
                let (target, off) = (a.u(0), a.u(1));
                self.ops.op_blob(Code::BufferSubData, &[target, off], a.bytes(4));
            }
            // ---- textures ----
            g::glTexImage2D => {
                let (target, level, ifmt, w, h, border, fmt, ty) =
                    (a.u(0), a.u(1), a.u(2), a.u(3), a.u(4), a.u(5), a.u(6), a.u(7));
                self.track_level(t, target, level, LevelSpec { ifmt, w, h, d: 1, fmt, ty, compressed: None });
                self.ops.op_blob(
                    Code::TexImage2D,
                    &[target, level, ifmt, w, h, border, fmt, ty, 0],
                    a.bytes(8),
                );
            }
            g::glTexImage2DOffsetAEMU => {
                let v: Vec<u32> = (0..9).map(|k| a.u(k)).collect();
                let spec =
                    LevelSpec { ifmt: v[2], w: v[3], h: v[4], d: 1, fmt: v[6], ty: v[7], compressed: None };
                self.track_level(t, v[0], v[1], spec);
                self.ops.op_blob(
                    Code::TexImage2D,
                    &[v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7], 1 + v[8]],
                    &[],
                );
            }
            g::glTexSubImage2D => {
                let v: Vec<u32> = (0..8).map(|k| a.u(k)).collect();
                self.ops.op_blob(
                    Code::TexSubImage2D,
                    &[v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7], 0],
                    a.bytes(8),
                );
            }
            g::glTexSubImage2DOffsetAEMU => {
                let v: Vec<u32> = (0..9).map(|k| a.u(k)).collect();
                self.ops.op_blob(
                    Code::TexSubImage2D,
                    &[v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7], 1 + v[8]],
                    &[],
                );
            }
            g::glTexImage3D | g::glTexImage3DOES => {
                let v: Vec<u32> = (0..9).map(|k| a.u(k)).collect();
                let spec = LevelSpec {
                    ifmt: v[2],
                    w: v[3],
                    h: v[4],
                    d: v[5],
                    fmt: v[7],
                    ty: v[8],
                    compressed: None,
                };
                self.track_level(t, v[0], v[1], spec);
                self.ops.op_blob(
                    Code::TexImage3D,
                    &[v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7], v[8], 0],
                    a.bytes(9),
                );
            }
            g::glTexImage3DOffsetAEMU => {
                let v: Vec<u32> = (0..10).map(|k| a.u(k)).collect();
                let spec = LevelSpec {
                    ifmt: v[2],
                    w: v[3],
                    h: v[4],
                    d: v[5],
                    fmt: v[7],
                    ty: v[8],
                    compressed: None,
                };
                self.track_level(t, v[0], v[1], spec);
                self.ops.op_blob(
                    Code::TexImage3D,
                    &[v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7], v[8], 1 + v[9]],
                    &[],
                );
            }
            g::glTexSubImage3D | g::glTexSubImage3DOES => {
                let v: Vec<u32> = (0..10).map(|k| a.u(k)).collect();
                self.ops.op_blob(
                    Code::TexSubImage3D,
                    &[v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7], v[8], v[9], 0],
                    a.bytes(10),
                );
            }
            g::glTexSubImage3DOffsetAEMU => {
                let v: Vec<u32> = (0..11).map(|k| a.u(k)).collect();
                self.ops.op_blob(
                    Code::TexSubImage3D,
                    &[v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7], v[8], v[9], 1 + v[10]],
                    &[],
                );
            }
            g::glCompressedTexImage2D => {
                let v: Vec<u32> = (0..7).map(|k| a.u(k)).collect();
                let spec = LevelSpec {
                    ifmt: v[2],
                    w: v[3],
                    h: v[4],
                    d: 1,
                    fmt: 0,
                    ty: 0,
                    compressed: Some(a.bytes(7).to_vec()),
                };
                self.track_level(t, v[0], v[1], spec);
                self.ops.op_blob(
                    Code::CompressedTexImage2D,
                    &[v[0], v[1], v[2], v[3], v[4], v[5]],
                    a.bytes(7),
                );
            }
            g::glCompressedTexSubImage2D => {
                let v: Vec<u32> = (0..8).map(|k| a.u(k)).collect();
                self.ops.op_blob(
                    Code::CompressedTexSubImage2D,
                    &[v[0], v[1], v[2], v[3], v[4], v[5], v[6]],
                    a.bytes(8),
                );
            }
            g::glCopyTexImage2D => {
                let v: Vec<u32> = (0..8).map(|k| a.u(k)).collect();
                let (fmt, ty) = unsized_of(v[2]);
                self.track_level(
                    t,
                    v[0],
                    v[1],
                    LevelSpec { ifmt: v[2], w: v[5], h: v[6], d: 1, fmt, ty, compressed: None },
                );
                self.ops.op(Code::CopyTexImage2D, &v);
            }
            g::glCopyTexSubImage2D => {
                let v: Vec<u32> = (0..8).map(|k| a.u(k)).collect();
                self.ops.op(Code::CopyTexSubImage2D, &v);
            }
            g::glCopyTexSubImage3D | g::glCopyTexSubImage3DOES => {
                let v: Vec<u32> = (0..9).map(|k| a.u(k)).collect();
                self.ops.op(Code::CopyTexSubImage3D, &v);
            }
            g::glTexParameteri | g::glTexParameterf => {
                let target = if a.u(0) == 0x8D65 { GL_TEXTURE_2D } else { a.u(0) };
                let code = if op == g::glTexParameteri { Code::TexParameteri } else { Code::TexParameterf };
                self.track_tex_param(t, target, a.u(1), op == g::glTexParameterf, a.u(2));
                self.ops.op(code, &[target, a.u(1), a.u(2)]);
            }
            g::glTexParameteriv | g::glTexParameterfv => {
                let target = if a.u(0) == 0x8D65 { GL_TEXTURE_2D } else { a.u(0) };
                let v = a.u32s(2).first().copied().unwrap_or(0);
                let code = if op == g::glTexParameteriv { Code::TexParameteri } else { Code::TexParameterf };
                self.track_tex_param(t, target, a.u(1), op == g::glTexParameterfv, v);
                self.ops.op(code, &[target, a.u(1), v]);
            }
            g::glGenerateMipmap => {
                if let Some(ti) = self.tex_info_mut(t, a.u(0)) {
                    ti.mipmap = true;
                }
                self.ops.op(Code::GenerateMipmap, &[a.u(0)]);
            }
            g::glTexStorage2D => {
                let v: Vec<u32> = (0..5).map(|k| a.u(k)).collect();
                if let Some(ti) = self.tex_info_mut(t, v[0]) {
                    ti.storage = Some([v[1], v[2], v[3], v[4], 1]);
                }
                self.ops.op(Code::TexStorage2D, &v);
            }
            g::glTexStorage3D => {
                let v: Vec<u32> = (0..6).map(|k| a.u(k)).collect();
                if let Some(ti) = self.tex_info_mut(t, v[0]) {
                    ti.storage = Some([v[1], v[2], v[3], v[4], v[5]]);
                }
                self.ops.op(Code::TexStorage3D, &v);
            }
            g::glSamplerParameteri
            | g::glSamplerParameterf
            | g::glSamplerParameteriv
            | g::glSamplerParameterfv => {
                let id = self.sampler(t, a.u(0));
                let v = if op == g::glSamplerParameteri || op == g::glSamplerParameterf {
                    a.u(2)
                } else {
                    a.u32s(2).first().copied().unwrap_or(0)
                };
                let int = op == g::glSamplerParameteri || op == g::glSamplerParameteriv;
                let code = if int { Code::SamplerParameteri } else { Code::SamplerParameterf };
                if let Some(s) = self.share_of(t) {
                    s.sampler_params.entry(id).or_default().insert(a.u(1), (!int, v));
                }
                self.ops.op(code, &[id, a.u(1), v]);
            }
            g::glEGLImageTargetTexture2DOES => {
                // EGL_GL_TEXTURE_2D images (native buffers go through
                // rcBindTexture): alias the bound texture to the image's.
                if let Some(&tex) = self.images.get(&a.u(1)) {
                    let unit = self.ctx(t).map_or(0, |c| c.state.active_texture) as usize;
                    let old = self.ctx(t).map_or(0, |c| c.state.textures[unit][0]);
                    if let Some(s) = self.share_of(t) {
                        let name = s.textures.guest_of(old);
                        if name != 0 {
                            s.textures.insert(name, tex);
                            s.aliases.insert(tex);
                        }
                    }
                    self.set(t, |s| s.textures[unit][0] = tex);
                    self.ops.op(Code::BindTexture, &[GL_TEXTURE_2D, tex]);
                }
            }
            g::glEGLImageTargetRenderbufferStorageOES => {}
            // ---- renderbuffers and framebuffers ----
            g::glRenderbufferStorage => {
                self.track_rb(t, [a.u(1), a.u(2), a.u(3), 0]);
                self.ops.op(Code::RenderbufferStorage, &[a.u(1), a.u(2), a.u(3)]);
            }
            g::glRenderbufferStorageMultisample => {
                self.track_rb(t, [a.u(2), a.u(3), a.u(4), a.u(1)]);
                self.ops.op(Code::RenderbufferStorageMultisample, &[a.u(1), a.u(2), a.u(3), a.u(4)]);
            }
            g::glFramebufferTexture2D => {
                let (target, attach, textarget, name, level) = (a.u(0), a.u(1), a.u(2), a.u(3), a.i(4));
                let id = self.texture(t, name);
                let textarget = if textarget == 0x8D65 { GL_TEXTURE_2D } else { textarget };
                self.note_attachment(t, target, attach, GL_TEXTURE, name, level);
                self.rec_attach(
                    t,
                    target,
                    attach,
                    Code::FramebufferTexture2D,
                    &[attach, textarget, id, level as u32],
                );
                self.ops.op(Code::FramebufferTexture2D, &[target, attach, textarget, id, level as u32]);
            }
            g::glFramebufferRenderbuffer => {
                let (target, attach, name) = (a.u(0), a.u(1), a.u(3));
                let id = self.renderbuffer(t, name);
                self.note_attachment(t, target, attach, GL_RENDERBUFFER, name, 0);
                let backing = self.share_of(t).and_then(|s| s.rb_tex.get(&id).copied());
                match backing {
                    Some(tex) => {
                        self.rec_attach(
                            t,
                            target,
                            attach,
                            Code::FramebufferTexture2D,
                            &[attach, GL_TEXTURE_2D, tex, 0],
                        );
                        self.ops.op(Code::FramebufferTexture2D, &[target, attach, GL_TEXTURE_2D, tex, 0])
                    }
                    None => {
                        self.rec_attach(
                            t,
                            target,
                            attach,
                            Code::FramebufferRenderbuffer,
                            &[attach, GL_RENDERBUFFER, id],
                        );
                        self.ops.op(Code::FramebufferRenderbuffer, &[target, attach, GL_RENDERBUFFER, id])
                    }
                }
            }
            g::glFramebufferTextureLayer => {
                let id = self.texture(t, a.u(2));
                self.note_attachment(t, a.u(0), a.u(1), GL_TEXTURE, a.u(2), a.i(3));
                self.rec_attach(
                    t,
                    a.u(0),
                    a.u(1),
                    Code::FramebufferTextureLayer,
                    &[a.u(1), id, a.u(3), a.u(4)],
                );
                self.ops.op(Code::FramebufferTextureLayer, &[a.u(0), a.u(1), id, a.u(3), a.u(4)]);
            }
            g::glCheckFramebufferStatus => r.ret = 0x8CD5, // FRAMEBUFFER_COMPLETE
            g::glDrawBuffers => {
                let mut bufs = Self::names_in(a, 1, a.i(0).max(0) as usize);
                let default = self.ctx(t).is_some_and(|c| c.draw_default);
                if default {
                    // BACK on framebuffer 0 is the surface's COLOR_ATTACHMENT0.
                    for b in &mut bufs {
                        if *b == 0x0405 {
                            *b = 0x8CE0;
                        }
                    }
                }
                self.ops.op(Code::DrawBuffers, &bufs);
            }
            g::glReadBuffer => {
                let default = self.ctx(t).is_some_and(|c| c.read_default);
                let b = if default && a.u(0) == 0x0405 { 0x8CE0 } else { a.u(0) };
                self.ops.op(Code::ReadBuffer, &[b]);
            }
            g::glBlitFramebuffer => {
                let v: Vec<u32> = (0..10).map(|k| a.u(k)).collect();
                self.ops.op(Code::BlitFramebuffer, &v);
            }
            g::glInvalidateFramebuffer | g::glDiscardFramebufferEXT => {
                let default = self.ctx(t).is_some_and(|c| c.draw_default);
                let mut att = Self::names_in(a, 2, a.i(1).max(0) as usize);
                if default {
                    for x in &mut att {
                        *x = match *x {
                            0x1800 => 0x8CE0, // COLOR
                            0x1801 => 0x8D00, // DEPTH
                            0x1802 => 0x8D20, // STENCIL
                            o => o,
                        };
                    }
                }
                let mut args = vec![a.u(0)];
                args.extend(att);
                self.ops.op(Code::InvalidateFramebuffer, &args);
            }
            g::glInvalidateSubFramebuffer => {}
            g::glGetFramebufferAttachmentParameteriv => {
                let (target, attach, pname) = (a.u(0), a.u(1), a.u(2));
                let fbo = self.bound_fbo(t, target);
                let info = self.ctx(t).and_then(|c| c.attachments.get(&(fbo, attach)).copied());
                let v = match (pname, info) {
                    (0x8CD0, Some((ty, _, _))) => ty as i32, // OBJECT_TYPE
                    (0x8CD0, None) => 0,
                    (0x8CD1, Some((_, name, _))) => name as i32, // OBJECT_NAME
                    (0x8CD2, Some((_, _, level))) => level,      // TEXTURE_LEVEL
                    (0x8210, _) => 0x2601,                       // COLOR_ENCODING: LINEAR
                    (0x8211, _) => 0x1406,                       // COMPONENT_TYPE: FLOAT(normalized)
                    (0x8212..=0x8217, _) => 8,                   // R/G/B/A/DEPTH/STENCIL size
                    _ => 0,
                };
                r.out_i32s(0, &[v]);
            }
            g::glGetRenderbufferParameteriv => r.out_i32s(0, &[0]),
            g::glGetInternalformativ => {
                // NUM_SAMPLE_COUNTS 1, SAMPLES [4].
                let v = match a.u(2) {
                    0x9380 => vec![1],
                    0x80A9 => vec![4],
                    _ => vec![0],
                };
                r.out_i32s(0, &v);
            }
            // ---- vertex arrays and drawing ----
            g::glEnableVertexAttribArray => {
                self.track_attrib(t, a.u(0), |p| p.enabled = true);
                self.ops.op(Code::EnableVertexAttribArray, &[a.u(0)]);
            }
            g::glDisableVertexAttribArray => {
                self.track_attrib(t, a.u(0), |p| p.enabled = false);
                self.ops.op(Code::DisableVertexAttribArray, &[a.u(0)]);
            }
            g::glVertexAttribDivisor => {
                let d = a.u(1);
                self.track_attrib(t, a.u(0), |p| p.divisor = d);
                self.ops.op(Code::VertexAttribDivisor, &[a.u(0), a.u(1)]);
            }
            g::glVertexAttribPointerOffset => {
                self.ops.op(Code::VertexAttribPointer, &[a.u(0), a.u(1), a.u(2), a.u(3), a.u(4), a.u(5)]);
            }
            g::glVertexAttribIPointerOffsetAEMU => {
                self.ops.op(Code::VertexAttribIPointer, &[a.u(0), a.u(1), a.u(2), a.u(3), a.u(4)]);
            }
            g::glVertexAttribPointerData => {
                let restore = self.ctx(t).map_or(0, |c| c.state.buffers[0]);
                self.ops.op_blob_then(
                    Code::VertexData,
                    &[a.u(0), a.u(1), a.u(2), a.u(3)],
                    a.bytes(5),
                    &[restore],
                );
            }
            g::glVertexAttribIPointerDataAEMU => {
                let restore = self.ctx(t).map_or(0, |c| c.state.buffers[0]);
                self.ops.op_blob_then(Code::VertexIData, &[a.u(0), a.u(1), a.u(2)], a.bytes(4), &[restore]);
            }
            g::glDrawArrays | g::glDrawArraysNullAEMU => {
                self.ops.op(Code::DrawArrays, &[a.u(0), a.u(1), a.u(2)]);
            }
            g::glDrawArraysInstanced => {
                self.ops.op(Code::DrawArraysInstanced, &[a.u(0), a.u(1), a.u(2), a.u(3)]);
            }
            g::glDrawElementsOffset | g::glDrawElementsOffsetNullAEMU => {
                self.ops.op(Code::DrawElements, &[a.u(0), a.u(1), a.u(2), a.u(3)]);
            }
            g::glDrawElementsInstancedOffsetAEMU => {
                self.ops.op(Code::DrawElementsInstanced, &[a.u(0), a.u(1), a.u(2), a.u(3), a.u(4)]);
            }
            g::glDrawRangeElementsOffsetAEMU => {
                self.ops.op(Code::DrawRangeElements, &[a.u(0), a.u(1), a.u(2), a.u(3), a.u(4), a.u(5)]);
            }
            g::glDrawElementsData | g::glDrawElementsDataNullAEMU => {
                let restore = self.element_buffer(t);
                self.ops.op_blob_then(
                    Code::DrawElementsData,
                    &[a.u(0), a.u(1), a.u(2)],
                    a.bytes(3),
                    &[restore],
                );
            }
            g::glDrawElementsInstancedDataAEMU => {
                let restore = self.element_buffer(t);
                self.ops.op_blob_then(
                    Code::DrawElementsInstancedData,
                    &[a.u(0), a.u(1), a.u(2), a.u(4)],
                    a.bytes(3),
                    &[restore],
                );
            }
            g::glDrawRangeElementsDataAEMU => {
                let restore = self.element_buffer(t);
                self.ops.op_blob_then(
                    Code::DrawElementsData,
                    &[a.u(0), a.u(3), a.u(4)],
                    a.bytes(5),
                    &[restore],
                );
            }
            g::glClear => self.ops.op(Code::Clear, &[a.u(0)]),
            g::glClearBufferfv | g::glClearBufferiv | g::glClearBufferuiv => {
                let code = match op {
                    g::glClearBufferfv => Code::ClearBufferfv,
                    g::glClearBufferiv => Code::ClearBufferiv,
                    _ => Code::ClearBufferuiv,
                };
                self.ops.op_blob(code, &[a.u(0), a.u(1)], a.bytes(2));
            }
            g::glClearBufferfi => self.ops.op(Code::ClearBufferfi, &[a.u(0), a.u(1), a.u(2), a.u(3)]),
            g::glFlush => self.ops.op(Code::Flush, &[]),
            g::glFinish => {
                self.ops.op(Code::Finish, &[]);
                self.flush();
            }
            g::glFinishRoundTrip => {
                self.flush();
                r.ret = 0;
            }
            // ---- queries, syncs, transform feedback ----
            g::glBeginQuery => {
                let id = self.query(t, a.u(1));
                self.ops.op(Code::BeginQuery, &[a.u(0), id]);
            }
            g::glEndQuery => self.ops.op(Code::EndQuery, &[a.u(0)]),
            g::glGetQueryObjectuiv => {
                let id = self.query(t, a.u(0));
                match a.u(1) {
                    0x8867 => r.out_u32s(0, &[1]), // QUERY_RESULT_AVAILABLE
                    _ => {
                        // QUERY_RESULT: a readback.
                        self.ops.op_read(Code::GetQueryResult, &[id], 4);
                        let v = self.flush();
                        let x = v.get(..4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
                        r.out_u32s(0, &[x]);
                    }
                }
            }
            g::glGetQueryiv => r.out_i32s(0, &[0]),
            g::glFenceSyncAEMU => {
                self.next_sync += 1;
                r.ret = self.next_sync;
            }
            g::glClientWaitSyncAEMU => r.ret = 0x911A, // ALREADY_SIGNALED
            g::glWaitSyncAEMU | g::glDeleteSyncAEMU => {}
            g::glIsSyncAEMU => r.ret = 1,
            g::glGetSyncivAEMU => {
                let v = match a.u(1) {
                    0x9112 => 0x9116, // OBJECT_TYPE: SYNC_FENCE
                    0x9113 => 0x9117, // SYNC_CONDITION: GPU_COMMANDS_COMPLETE
                    0x9114 => 0x9119, // SYNC_STATUS: SIGNALED
                    _ => 0,           // SYNC_FLAGS
                };
                r.out_i32s(0, &[1]);
                r.out_i32s(1, &[v]);
            }
            g::glBeginTransformFeedback => self.ops.op(Code::BeginTransformFeedback, &[a.u(0)]),
            g::glEndTransformFeedback => self.ops.op(Code::EndTransformFeedback, &[]),
            g::glPauseTransformFeedback => self.ops.op(Code::PauseTransformFeedback, &[]),
            g::glResumeTransformFeedback => self.ops.op(Code::ResumeTransformFeedback, &[]),
            // ---- readbacks ----
            g::glReadPixels => {
                let v: Vec<u32> = (0..6).map(|k| a.u(k)).collect();
                let n = r.out(0).len();
                self.ops.op_read(Code::ReadPixels, &[v[0], v[1], v[2], v[3], v[4], v[5], 0, n as u32], n);
                let data = self.flush();
                let o = r.out(0);
                let m = o.len().min(data.len());
                o[..m].copy_from_slice(&data[..m]);
            }
            g::glReadPixelsOffsetAEMU => {
                let v: Vec<u32> = (0..7).map(|k| a.u(k)).collect();
                self.ops.op(Code::ReadPixels, &[v[0], v[1], v[2], v[3], v[4], v[5], 1 + v[6], 0]);
            }
            // ---- simple queries ----
            g::glGetError => r.ret = 0,
            g::glGetIntegerv | g::glGetFloatv | g::glGetBooleanv | g::glGetInteger64v => {
                let pname = a.u(0);
                let v = self.get_state(t, pname);
                if v.is_none() {
                    self.warn(format!("glGet pname {pname:#x}"));
                }
                let v = v.unwrap_or_default();
                match op {
                    g::glGetFloatv => r.out_f32s(0, &v.iter().map(|&x| x as f32).collect::<Vec<_>>()),
                    g::glGetBooleanv => {
                        let o = r.out(0);
                        for (b, x) in o.iter_mut().zip(&v) {
                            *b = (*x != 0.0) as u8;
                        }
                    }
                    g::glGetInteger64v => {
                        let o = r.out(0);
                        for (c, x) in o.as_chunks_mut::<8>().0.iter_mut().zip(&v) {
                            c.copy_from_slice(&(*x as i64).to_le_bytes());
                        }
                    }
                    _ => r.out_i32s(0, &v.iter().map(|&x| x as i64 as i32).collect::<Vec<_>>()),
                }
            }
            g::glGetIntegeri_v | g::glGetInteger64i_v => {
                let (pname, index) = (a.u(0), a.u(1) as usize);
                let v = self.ctx(t).and_then(|c| {
                    let s = &c.state;
                    let share = self.shares.get(&c.share)?;
                    let b = s.ubo.get(index)?;
                    Some(match pname {
                        0x8A28 => i64::from(share.buffers.guest_of(b.0)), // UNIFORM_BUFFER_BINDING
                        0x8A29 => b.1,
                        0x8A2A => b.2,
                        _ => 0,
                    })
                });
                let v = v.unwrap_or(0);
                if op == g::glGetInteger64i_v {
                    let o = r.out(0);
                    let n = o.len().min(8);
                    o[..n].copy_from_slice(&v.to_le_bytes()[..n]);
                } else {
                    r.out_i32s(0, &[v as i32]);
                }
            }
            g::glGetBufferParameteriv | g::glGetBufferParameteri64v => {
                let (target, pname) = (a.u(0), a.u(1));
                let id = self.bound_buffer(t, target);
                let v = match pname {
                    0x8764 => self.share_of(t).and_then(|s| s.buffer_sizes.get(&id).copied()).unwrap_or(0),
                    0x8765 => 0x88E4, // BUFFER_USAGE: STATIC_DRAW
                    _ => 0,
                };
                if op == g::glGetBufferParameteri64v {
                    let o = r.out(0);
                    let n = o.len().min(8);
                    o[..n].copy_from_slice(&v.to_le_bytes()[..n]);
                } else {
                    r.out_i32s(0, &[v as i32]);
                }
            }
            g::glGetTexParameteriv
            | g::glGetTexParameterfv
            | g::glGetSamplerParameteriv
            | g::glGetSamplerParameterfv
            | g::glGetVertexAttribiv
            | g::glGetVertexAttribfv
            | g::glGetVertexAttribIiv
            | g::glGetVertexAttribIuiv
            | g::glGetCompressedTextureFormats
            | g::glGetProgramBinary
            | g::glGetTransformFeedbackVarying => {
                // Zeros (not needed by the guest's UI; see the spec).
                let name = GLES2[(op - GLES2_BASE) as usize].name;
                self.warn(format!("{name} answered with zeros"));
            }
            g::glTransformFeedbackVaryingsAEMU | g::glProgramParameteri | g::glProgramBinary => {
                let name = GLES2[(op - GLES2_BASE) as usize].name;
                self.warn(format!("{name} ignored"));
            }
            _ => {
                self.stats.unhandled += 1;
                let name = GLES2.get((op - GLES2_BASE) as usize).map_or("?", |o| o.name);
                self.warn(format!("unhandled GLES call {name}"));
            }
        }
    }

    fn vertex_attrib(&mut self, t: &Thread, index: u32, kind: u8, v: [u32; 4]) {
        let i = index as usize;
        if i >= state::ATTRIBS {
            return;
        }
        self.set(t, |s| s.attribs[i] = (kind, v));
        let code = match kind {
            1 => Code::VertexAttribI4i,
            2 => Code::VertexAttribI4ui,
            _ => Code::VertexAttrib4f,
        };
        self.ops.op(code, &[index, v[0], v[1], v[2], v[3]]);
    }

    fn element_buffer(&self, t: &Thread) -> u32 {
        self.ctx(t).and_then(|c| c.vao_elements.get(&c.state.vao).copied()).unwrap_or(0)
    }

    fn bound_buffer(&self, t: &Thread, target: u32) -> u32 {
        if target == GL_ELEMENT_ARRAY_BUFFER {
            return self.element_buffer(t);
        }
        let Some(c) = self.ctx(t) else { return 0 };
        state::buffer_slot(target).map_or(0, |k| c.state.buffers[k])
    }

    fn bound_fbo(&self, t: &Thread, target: u32) -> u32 {
        let Some(c) = self.ctx(t) else { return 0 };
        if target == GL_READ_FRAMEBUFFER { c.state.read_fbo } else { c.state.draw_fbo }
    }

    fn note_buffer_usage(&mut self, t: &Thread, target: u32, usage: u32) {
        let id = self.bound_buffer(t, target);
        if id != 0
            && let Some(s) = self.share_of(t)
        {
            s.buffer_usage.insert(id, usage);
        }
    }

    /// The texture bound on the active unit to `target` (cube faces: the cube).
    fn bound_texture(&self, t: &Thread, target: u32) -> u32 {
        let Some(c) = self.ctx(t) else { return 0 };
        let slot = match target {
            0x8515..=0x851A => Some(1),
            other => state::tex_slot(other),
        };
        slot.map_or(0, |s| c.state.textures[c.state.active_texture as usize][s])
    }

    fn tex_info_mut(&mut self, t: &Thread, target: u32) -> Option<&mut TexInfo> {
        let id = self.bound_texture(t, target);
        if id == 0 {
            return None;
        }
        let bind = match target {
            0x8515..=0x851A => 0x8513,
            0x8D65 => GL_TEXTURE_2D,
            other => other,
        };
        let ti = self.share_of(t)?.tex_info.entry(id).or_default();
        if ti.target == 0 {
            ti.target = bind;
        }
        Some(ti)
    }

    fn track_level(&mut self, t: &Thread, target: u32, level: u32, spec: LevelSpec) {
        let target = if target == 0x8D65 { GL_TEXTURE_2D } else { target };
        if let Some(ti) = self.tex_info_mut(t, target) {
            ti.levels.insert((target, level), spec);
        }
    }

    fn track_tex_param(&mut self, t: &Thread, target: u32, pname: u32, float: bool, v: u32) {
        if let Some(ti) = self.tex_info_mut(t, target) {
            ti.params.insert(pname, (float, v));
        }
    }

    fn track_rb(&mut self, t: &Thread, spec: [u32; 4]) {
        let Some(rb) = self.ctx(t).map(|c| c.state.renderbuffer).filter(|&r| r != 0) else { return };
        if let Some(s) = self.share_of(t) {
            s.rb_info.insert(rb, spec);
        }
    }

    fn rec_attach(&mut self, t: &Thread, target: u32, attach: u32, code: Code, args: &[u32]) {
        let fbo = self.bound_fbo(t, target);
        if let Some(c) = self.ctxs.get_mut(&t.ctx) {
            let mut v = vec![GL_DRAW_FRAMEBUFFER];
            v.extend_from_slice(args);
            c.fb_ops.insert((fbo, attach), (code, v));
        }
    }

    fn track_attrib(&mut self, t: &Thread, index: u32, f: impl FnOnce(&mut AttribPtr)) {
        let i = index as usize;
        if i >= state::ATTRIBS {
            return;
        }
        if let Some(c) = self.ctxs.get_mut(&t.ctx) {
            let vao = c.state.vao;
            f(&mut c.vao_attribs.entry(vao).or_insert([AttribPtr::default(); state::ATTRIBS])[i]);
        }
    }

    /// Vertex attribute pointers (all four wire forms) into the vertex
    /// array's record.
    fn track_attrib_pointer(&mut self, t: &Thread, op: u32, a: &Args<'_, '_>) {
        let (int, client) = match op {
            g::glVertexAttribPointerOffset => (false, false),
            g::glVertexAttribIPointerOffsetAEMU => (true, false),
            g::glVertexAttribPointerData => (false, true),
            g::glVertexAttribIPointerDataAEMU => (true, true),
            _ => return,
        };
        let buf = self.ctx(t).map_or(0, |c| c.state.buffers[0]);
        let (size, ty) = (a.u(1), a.u(2));
        let (norm, stride, offset) = match op {
            g::glVertexAttribPointerOffset => (a.b(3), a.u(4), a.u(5)),
            g::glVertexAttribIPointerOffsetAEMU => (false, a.u(3), a.u(4)),
            g::glVertexAttribPointerData => (a.b(3), 0, 0),
            _ => (false, 0, 0),
        };
        let buffer = if client { 0 } else { buf };
        self.track_attrib(t, a.u(0), |p| {
            *p = AttribPtr { size, ty, norm, stride, offset, buffer, integer: int, client, ..*p }
        });
    }

    fn note_buffer_size(&mut self, t: &Thread, target: u32, size: i64) {
        let id = self.bound_buffer(t, target);
        if id != 0
            && let Some(s) = self.share_of(t)
        {
            s.buffer_sizes.insert(id, size);
        }
    }

    fn note_attachment(&mut self, t: &Thread, target: u32, attach: u32, ty: u32, name: u32, level: i32) {
        let fbo = self.bound_fbo(t, target);
        if let Some(c) = self.ctxs.get_mut(&t.ctx) {
            if name == 0 {
                c.attachments.remove(&(fbo, attach));
            } else {
                c.attachments.insert((fbo, attach), (ty, name, level));
            }
            if attach == 0x821A {
                // DEPTH_STENCIL_ATTACHMENT sets both.
                for x in [0x8D00, 0x8D20] {
                    if name == 0 {
                        c.attachments.remove(&(fbo, x));
                    } else {
                        c.attachments.insert((fbo, x), (ty, name, level));
                    }
                }
            }
        }
    }

    fn gen_name(&mut self, t: &Thread, op: u32) -> u32 {
        let (kind, per_ctx) = match op {
            g::glGenTextures => (Kind::Texture, false),
            g::glGenBuffers => (Kind::Buffer, false),
            g::glGenRenderbuffers => (Kind::Renderbuffer, false),
            g::glGenSamplers => (Kind::Sampler, false),
            g::glGenFramebuffers => (Kind::Framebuffer, true),
            g::glGenQueries => (Kind::Query, true),
            g::glGenTransformFeedbacks => (Kind::TransformFeedback, true),
            _ => (Kind::VertexArray, true),
        };
        let id = self.create(kind, 0);
        if per_ctx {
            let Some(c) = self.ctxs.get_mut(&t.ctx) else { return 0 };
            let names = match kind {
                Kind::Framebuffer => &mut c.fbos,
                Kind::Query => &mut c.queries,
                Kind::TransformFeedback => &mut c.tfbs,
                _ => &mut c.vaos,
            };
            let n = names.fresh();
            names.insert(n, id);
            n
        } else {
            let Some(s) = self.share_of(t) else { return 0 };
            let names = match kind {
                Kind::Texture => &mut s.textures,
                Kind::Buffer => &mut s.buffers,
                Kind::Renderbuffer => &mut s.renderbuffers,
                _ => &mut s.samplers,
            };
            let n = names.fresh();
            names.insert(n, id);
            n
        }
    }

    fn delete_name(&mut self, t: &Thread, op: u32, name: u32) {
        if name == 0 {
            return;
        }
        let (kind, id) = match op {
            g::glDeleteTextures => {
                let Some(s) = self.share_of(t) else { return };
                let Some(id) = s.textures.remove(name) else { return };
                if s.aliases.contains(&id) {
                    // A ColorBuffer's texture: only the name goes.
                    self.unbind_everywhere(id, Kind::Texture);
                    return;
                }
                (Kind::Texture, id)
            }
            g::glDeleteBuffers => {
                let Some(s) = self.share_of(t) else { return };
                let Some(id) = s.buffers.remove(name) else { return };
                s.buffer_sizes.remove(&id);
                (Kind::Buffer, id)
            }
            g::glDeleteRenderbuffers => {
                let Some(s) = self.share_of(t) else { return };
                let Some(id) = s.renderbuffers.remove(name) else { return };
                s.rb_tex.remove(&id);
                (Kind::Renderbuffer, id)
            }
            g::glDeleteSamplers => {
                let Some(s) = self.share_of(t) else { return };
                let Some(id) = s.samplers.remove(name) else { return };
                (Kind::Sampler, id)
            }
            g::glDeleteFramebuffers => {
                let Some(c) = self.ctxs.get_mut(&t.ctx) else { return };
                let Some(id) = c.fbos.remove(name) else { return };
                c.attachments.retain(|k, _| k.0 != id);
                (Kind::Framebuffer, id)
            }
            g::glDeleteQueries => {
                let Some(c) = self.ctxs.get_mut(&t.ctx) else { return };
                let Some(id) = c.queries.remove(name) else { return };
                (Kind::Query, id)
            }
            g::glDeleteTransformFeedbacks => {
                let Some(c) = self.ctxs.get_mut(&t.ctx) else { return };
                let Some(id) = c.tfbs.remove(name) else { return };
                (Kind::TransformFeedback, id)
            }
            _ => {
                let Some(c) = self.ctxs.get_mut(&t.ctx) else { return };
                let Some(id) = c.vaos.remove(name) else { return };
                c.vao_elements.remove(&id);
                (Kind::VertexArray, id)
            }
        };
        self.unbind_everywhere(id, kind);
        self.delete(kind, id);
    }

    /// GL unbinds a deleted object from the current context's bindings.
    fn unbind_everywhere(&mut self, id: u32, kind: Kind) {
        let active = self.active;
        let fix = |s: &mut GlState, default_vao: u32, surf: (u32, u32)| match kind {
            Kind::Texture => {
                for u in s.textures.iter_mut() {
                    for x in u.iter_mut() {
                        if *x == id {
                            *x = 0;
                        }
                    }
                }
            }
            Kind::Buffer => {
                for x in s.buffers.iter_mut() {
                    if *x == id {
                        *x = 0;
                    }
                }
                for b in s.ubo.iter_mut() {
                    if b.0 == id {
                        *b = (0, 0, 0);
                    }
                }
            }
            Kind::Renderbuffer => {
                if s.renderbuffer == id {
                    s.renderbuffer = 0;
                }
            }
            Kind::Sampler => {
                for x in s.samplers.iter_mut() {
                    if *x == id {
                        *x = 0;
                    }
                }
            }
            Kind::Framebuffer => {
                if s.draw_fbo == id {
                    s.draw_fbo = surf.0;
                }
                if s.read_fbo == id {
                    s.read_fbo = surf.1;
                }
            }
            Kind::VertexArray => {
                if s.vao == id {
                    s.vao = default_vao;
                }
            }
            Kind::TransformFeedback => {
                if s.transform_feedback == id {
                    s.transform_feedback = 0;
                }
            }
            _ => {}
        };
        if active == 0 {
            return;
        }
        let Some(c) = self.ctxs.get(&active) else { return };
        let surf = (self.surface_fbo(c.draw), self.surface_fbo(c.read));
        let dv = c.default_vao;
        let c = self.ctxs.get_mut(&active).unwrap();
        let before = c.state.clone();
        fix(&mut c.state, dv, surf);
        if kind == Kind::Framebuffer {
            if before.draw_fbo == id {
                c.draw_default = true;
            }
            if before.read_fbo == id {
                c.read_default = true;
            }
        }
        // WebGL unbinds deleted objects itself; the mirror follows, and a
        // framebuffer unbound back to "0" must go to the surface's.
        let mut mirror = self.applied.clone();
        fix(&mut mirror, dv, (0, 0));
        self.applied = mirror;
        let to = self.ctxs[&active].state.clone();
        state::transition(&mut self.ops.buf(), &self.applied, &to);
        self.applied = to;
    }

    /// Decodes the complete calls at the start of `buf` for thread `t`;
    /// returns the bytes consumed and appends replies to `reply`.
    pub fn decode(&mut self, t: &mut Thread, buf: &[u8], reply: &mut Vec<u8>) -> usize {
        let mut used = 0;
        let mut args: Vec<Arg<'_>> = Vec::with_capacity(16);
        while let Some((op, len)) = wire::peek(&buf[used..]) {
            if len < 8 {
                // Malformed: drop the rest (the guest stream is lost anyway).
                self.warn(format!("malformed packet length {len} for opcode {op}"));
                return buf.len();
            }
            if buf.len() - used < len {
                break;
            }
            let packet = &buf[used..used + len];
            used += len;
            let table = if (RC_BASE..RC_BASE + RC.len() as u32).contains(&op) {
                Some((&RC[(op - RC_BASE) as usize], true))
            } else if (GLES2_BASE..GLES2_BASE + GLES2.len() as u32).contains(&op) {
                Some((&GLES2[(op - GLES2_BASE) as usize], false))
            } else {
                None
            };
            let Some((spec, is_rc)) = table else {
                // GLES1 (1024..2047) or unknown: skipped by length.
                self.stats.unhandled += 1;
                self.warn(format!("unknown opcode {op}"));
                continue;
            };
            if wire::decode(spec, packet, &mut args).is_none() {
                self.warn(format!("short packet for {}", spec.name));
                continue;
            }
            let a = Args(&args);
            let mut r = Reply::new(spec, &args);
            if is_rc {
                self.rc(t, op, &a, &mut r);
            } else {
                self.gles(t, op, &a, &mut r);
            }
            if self.trace {
                self.trace_line(format!("ctx {} gl ctx {:#x}: {} ({len} bytes)", t.id, t.ctx, spec.name));
            }
            let has_reply = spec.ret > 0 || spec.params.contains(&wire::P::Out);
            if has_reply {
                reply.extend_from_slice(&r.encode(spec));
            }
        }
        used
    }
}
