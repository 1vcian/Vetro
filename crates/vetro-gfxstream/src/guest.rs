//! A guest stand-in for tests and tools: encodes renderControl/GLES calls
//! like emugen's encoders and talks through pipes on virtio-gpu 3D
//! resources, as `libEGL_emulation` over `virtio-gpu-pipe` does. Also the
//! synthetic scene of the rendering test (`examples/scene.rs`,
//! `tests/web/gl.mjs`).

use vetro_platform::virtio::gpu::{Backing, Box3d, Create3d, Rect, Renderer3d, Transfer3d};
use vetro_platform::virtio::{GuestRam, RamError};

use crate::exec::GlExecutor;
use crate::tables::{GLES2, GLES2_BASE, RC, RC_BASE, gles2 as g, rc};
use crate::wire::P;
use crate::{Gfxstream, formats};

/// Guest memory for the pipes' backing.
pub struct Ram(pub Vec<u8>);

impl GuestRam for Ram {
    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), RamError> {
        let a = addr as usize;
        buf.copy_from_slice(self.0.get(a..a + buf.len()).ok_or(RamError { addr, len: buf.len() })?);
        Ok(())
    }
    fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), RamError> {
        let a = addr as usize;
        self.0.get_mut(a..a + data.len()).ok_or(RamError { addr, len: data.len() })?.copy_from_slice(data);
        Ok(())
    }
}

/// An argument of [`call`].
pub enum V<'a> {
    S(u64),
    B(&'a [u8]),
    O(u32),
}

/// One call encoded like emugen's encoders.
pub fn call(op: u32, args: &[V<'_>]) -> Vec<u8> {
    let spec = if op >= RC_BASE { &RC[(op - RC_BASE) as usize] } else { &GLES2[(op - GLES2_BASE) as usize] };
    assert_eq!(spec.params.len(), args.len(), "{}", spec.name);
    let mut body = Vec::new();
    for (p, a) in spec.params.iter().zip(args) {
        match (p, a) {
            (P::S1, V::S(v)) => body.push(*v as u8),
            (P::S2, V::S(v)) => body.extend_from_slice(&(*v as u16).to_le_bytes()),
            (P::S4, V::S(v)) => body.extend_from_slice(&(*v as u32).to_le_bytes()),
            (P::S8, V::S(v)) => body.extend_from_slice(&v.to_le_bytes()),
            (P::In, V::B(b)) => {
                body.extend_from_slice(&(b.len() as u32).to_le_bytes());
                body.extend_from_slice(b);
            }
            (P::Out, V::O(n)) => body.extend_from_slice(&n.to_le_bytes()),
            _ => panic!("argument kind for {}", spec.name),
        }
    }
    let mut p = op.to_le_bytes().to_vec();
    p.extend_from_slice(&((body.len() + 8) as u32).to_le_bytes());
    p.extend(body);
    p
}

const PIPE_BASE: u64 = 0x1000;
const PIPE_SIZE: u32 = 1 << 20;
/// Backing of the ColorBuffer resources (after the pipes).
const CB_BASE: u64 = 40 << 20;

/// A guest with one pipe per virtio context.
pub struct Guest {
    pub gfx: Gfxstream,
    pub ram: Ram,
    /// Per context, where the next write goes in the pipe buffer: like
    /// upstream `VirtioGpuPipeStream`, writes follow each other in the
    /// buffer (the transfer box's x is the offset) until a read or a wrap.
    written: std::collections::BTreeMap<u32, u32>,
}

impl Guest {
    pub fn new(exec: Box<dyn GlExecutor>) -> Self {
        Self {
            gfx: Gfxstream::new(exec, (1280, 800)),
            ram: Ram(vec![0; 64 << 20]),
            written: Default::default(),
        }
    }

    /// Opens virtio context `ctx` with its pipe resource (id 100 + ctx) and
    /// connects to `service`.
    pub fn open(&mut self, ctx: u32, service: &str) {
        self.gfx.context_create(ctx, 0, b"test").unwrap();
        let res = 100 + ctx;
        let args = Create3d {
            target: 0,
            format: 64,
            bind: 1 << 17,
            width: PIPE_SIZE,
            height: 1,
            ..Create3d::default()
        };
        self.gfx.resource_create(res, &args).unwrap();
        self.gfx.context_attach(ctx, res);
        let mut s = service.as_bytes().to_vec();
        s.push(0);
        self.send(ctx, &s);
    }

    fn ents(ctx: u32) -> [(u64, u32); 1] {
        [(PIPE_BASE + u64::from(ctx) * u64::from(PIPE_SIZE), PIPE_SIZE)]
    }

    pub fn send(&mut self, ctx: u32, data: &[u8]) {
        let ents = Self::ents(ctx);
        let mut pos = self.written.get(&ctx).copied().unwrap_or(0);
        if data.len() as u32 > PIPE_SIZE - pos {
            pos = 0;
        }
        let mut b = Backing::new(&mut self.ram, &ents);
        b.write(u64::from(pos), data);
        let t = Transfer3d {
            bx: Box3d { x: pos, w: data.len() as u32, h: 1, d: 1, ..Box3d::default() },
            ..Transfer3d::default()
        };
        self.gfx.transfer_to_host(ctx, 100 + ctx, &t, &mut b).unwrap();
        self.written.insert(ctx, pos + data.len() as u32);
    }

    pub fn recv(&mut self, ctx: u32, n: usize) -> Vec<u8> {
        self.written.insert(ctx, 0);
        let ents = Self::ents(ctx);
        let mut b = Backing::new(&mut self.ram, &ents);
        let t = Transfer3d {
            bx: Box3d { x: 0, w: n as u32, h: 1, d: 1, ..Box3d::default() },
            ..Transfer3d::default()
        };
        self.gfx.transfer_from_host(ctx, 100 + ctx, &t, &mut b).unwrap();
        let mut out = vec![0; n];
        b.read(0, &mut out);
        out
    }

    pub fn u32(&mut self, ctx: u32) -> u32 {
        u32::from_le_bytes(self.recv(ctx, 4).try_into().unwrap())
    }

    /// A gralloc-like ColorBuffer resource `res` of `w`x`h`.
    pub fn color_buffer(&mut self, res: u32, format: u32, w: u32, h: u32) {
        let args = Create3d {
            target: 2,
            format,
            bind: formats::VIRGL_BIND_RENDER_TARGET
                | formats::VIRGL_BIND_SAMPLER_VIEW
                | formats::VIRGL_BIND_SCANOUT,
            width: w,
            height: h,
            depth: 1,
            array_size: 1,
            ..Create3d::default()
        };
        self.gfx.resource_create(res, &args).unwrap();
    }

    fn cb_ents(res: u32) -> [(u64, u32); 1] {
        [(CB_BASE + u64::from(res % 16) * (1 << 20), 1 << 20)]
    }

    /// The guest writes `px` (rows of `stride` bytes) into the buffer and
    /// flushes it to the host, like gralloc unlock after a CPU write.
    pub fn upload(&mut self, res: u32, w: u32, h: u32, stride: u32, px: &[u8]) {
        let ents = Self::cb_ents(res);
        let mut b = Backing::new(&mut self.ram, &ents);
        b.write(0, px);
        let t = Transfer3d { bx: Box3d { x: 0, y: 0, z: 0, w, h, d: 1 }, stride, ..Transfer3d::default() };
        self.gfx.transfer_to_host(0, res, &t, &mut b).unwrap();
    }

    /// The buffer read back into guest memory (gralloc lock for a CPU read).
    pub fn download(&mut self, res: u32, w: u32, h: u32, bpp: u32) -> Vec<u8> {
        let ents = Self::cb_ents(res);
        let mut b = Backing::new(&mut self.ram, &ents);
        let t = Transfer3d {
            bx: Box3d { x: 0, y: 0, z: 0, w, h, d: 1 },
            stride: w * bpp,
            ..Transfer3d::default()
        };
        self.gfx.transfer_from_host(0, res, &t, &mut b).unwrap();
        let mut out = vec![0; (w * h * bpp) as usize];
        b.read(0, &mut out);
        out
    }
}

/// Size of the scene's window.
pub const SCENE_SIZE: u32 = 64;

/// The synthetic scene of the rendering test, driven the way SurfaceFlinger
/// drives gfxstream: a window surface on an RGBA8888 buffer (resource 7), a
/// clear, a scissored clear of the bottom-left quadrant, and a BGRA8888
/// buffer (resource 8, written by the CPU) sampled as an external texture
/// through rcBindTexture onto the top-right quadrant with a client-side
/// vertex array; then the window is presented on scanout 0 and read back
/// into guest memory. Returns the bytes the guest reads back (texture row 0
/// first) — zeros unless the executor renders.
pub fn scene(gu: &mut Guest) -> Vec<u8> {
    let n = SCENE_SIZE;
    gu.color_buffer(7, formats::VIRGL_FORMAT_R8G8B8A8_UNORM, n, n);
    gu.color_buffer(8, formats::VIRGL_FORMAT_B8G8R8A8_UNORM, 16, 16);
    // The CPU buffer: left half green, right half blue (B, G, R, A bytes).
    let mut px = Vec::new();
    for _y in 0..16 {
        for x in 0..16 {
            px.extend_from_slice(if x < 8 { &[0, 255, 0, 255] } else { &[255, 0, 0, 255] });
        }
    }
    gu.upload(8, 16, 16, 64, &px);

    gu.open(1, "pipe:opengles");
    let mut s = 0u32.to_le_bytes().to_vec();
    s.extend(call(rc::rcCreateContext, &[V::S(0), V::S(0), V::S(3)]));
    gu.send(1, &s);
    let ctx = gu.u32(1);
    gu.send(1, &call(rc::rcCreateWindowSurface, &[V::S(0), V::S(u64::from(n)), V::S(u64::from(n))]));
    let surf = gu.u32(1);
    let mut s = call(rc::rcSetWindowColorBuffer, &[V::S(u64::from(surf)), V::S(7)]);
    s.extend(call(rc::rcMakeCurrent, &[V::S(u64::from(ctx)), V::S(u64::from(surf)), V::S(u64::from(surf))]));
    gu.send(1, &s);
    assert_eq!(gu.u32(1), 1);

    let vs = b"#version 300 es\nin vec2 aPos;\nout vec2 vUv;\nvoid main() { vUv = aPos; gl_Position = vec4(aPos, 0.0, 1.0); }\n\0";
    let fs = b"#version 300 es\n#extension GL_OES_EGL_image_external_essl3 : require\nprecision mediump float;\nuniform samplerExternalOES uTex;\nin vec2 vUv;\nout vec4 o;\nvoid main() { o = texture(uTex, vUv); }\n\0";
    gu.send(1, &call(g::glCreateShader, &[V::S(0x8B31)]));
    let vsh = gu.u32(1);
    gu.send(1, &call(g::glCreateShader, &[V::S(0x8B30)]));
    let fsh = gu.u32(1);
    gu.send(1, &call(g::glCreateProgram, &[]));
    let prog = gu.u32(1);
    gu.send(1, &call(g::glGenTextures, &[V::S(1), V::O(4)]));
    let tex = gu.u32(1);
    let (p, v, f) = (u64::from(prog), u64::from(vsh), u64::from(fsh));
    let mut s = Vec::new();
    s.extend(call(g::glShaderString, &[V::S(v), V::B(vs), V::S(vs.len() as u64)]));
    s.extend(call(g::glShaderString, &[V::S(f), V::B(fs), V::S(fs.len() as u64)]));
    s.extend(call(g::glCompileShader, &[V::S(v)]));
    s.extend(call(g::glCompileShader, &[V::S(f)]));
    s.extend(call(g::glAttachShader, &[V::S(p), V::S(v)]));
    s.extend(call(g::glAttachShader, &[V::S(p), V::S(f)]));
    s.extend(call(g::glLinkProgram, &[V::S(p)]));
    // Background, then the bottom-left quadrant red.
    let c = |x: f32| V::S(u64::from(x.to_bits()));
    s.extend(call(g::glViewport, &[V::S(0), V::S(0), V::S(u64::from(n)), V::S(u64::from(n))]));
    s.extend(call(g::glClearColor, &[c(0.25), c(0.5), c(0.75), c(1.0)]));
    s.extend(call(g::glClear, &[V::S(0x4000)]));
    s.extend(call(g::glEnable, &[V::S(0x0C11)]));
    s.extend(call(g::glScissor, &[V::S(0), V::S(0), V::S(u64::from(n / 2)), V::S(u64::from(n / 2))]));
    s.extend(call(g::glClearColor, &[c(1.0), c(0.0), c(0.0), c(1.0)]));
    s.extend(call(g::glClear, &[V::S(0x4000)]));
    s.extend(call(g::glDisable, &[V::S(0x0C11)]));
    // The external texture: the CPU buffer bound through rcBindTexture.
    s.extend(call(g::glActiveTexture, &[V::S(0x84C0)]));
    s.extend(call(g::glBindTexture, &[V::S(0x8D65), V::S(u64::from(tex))]));
    s.extend(call(rc::rcBindTexture, &[V::S(8)]));
    s.extend(call(g::glTexParameteri, &[V::S(0x8D65), V::S(0x2801), V::S(0x2600)]));
    s.extend(call(g::glTexParameteri, &[V::S(0x8D65), V::S(0x2800), V::S(0x2600)]));
    s.extend(call(g::glUseProgram, &[V::S(p)]));
    s.extend(call(g::glUniform1i, &[V::S(0), V::S(0)]));
    // Quad on the top-right quadrant (NDC 0..1), client-side array.
    let quad: Vec<u8> =
        [0.0f32, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0].iter().flat_map(|x| x.to_bits().to_le_bytes()).collect();
    s.extend(call(g::glEnableVertexAttribArray, &[V::S(0)]));
    s.extend(call(
        g::glVertexAttribPointerData,
        &[V::S(0), V::S(2), V::S(0x1406), V::S(0), V::S(8), V::B(&quad), V::S(32)],
    ));
    s.extend(call(g::glDrawArrays, &[V::S(5), V::S(0), V::S(4)]));
    s.extend(call(rc::rcFlushWindowColorBufferAsync, &[V::S(u64::from(surf))]));
    gu.send(1, &s);
    gu.gfx.scanout(0, Some((7, Rect::new(0, 0, n, n))));
    Renderer3d::flush(&mut gu.gfx, 0, 7, Rect::new(0, 0, n, n));
    gu.download(7, n, n, 4)
}

/// The image [`scene`] must produce (texture row 0 first, RGBA): the
/// quadrant colors, with the texture's quad sampling the buffer's left half
/// (green) on the left of the quadrant and the right half (blue) on the
/// right.
pub fn scene_expected() -> Vec<u8> {
    let n = SCENE_SIZE;
    let mut v = Vec::new();
    for y in 0..n {
        for x in 0..n {
            let px: [u8; 4] = if x < n / 2 && y < n / 2 {
                [255, 0, 0, 255]
            } else if x >= n / 2 && y >= n / 2 {
                // u = (x - n/2) / (n/2): left half of the buffer is green.
                if x < n / 2 + n / 4 { [0, 255, 0, 255] } else { [0, 0, 255, 255] }
            } else {
                [64, 128, 191, 255]
            };
            v.extend_from_slice(&px);
        }
    }
    v
}

/// After [`scene`] (possibly across a snapshot): the same render thread
/// clears the window to black and draws the textured quad again with the
/// program, external texture and vertex array it already had, then reads the
/// window back. Checks that programs, uniforms, texture aliases and state
/// survive.
pub fn scene_redraw(gu: &mut Guest) -> Vec<u8> {
    let n = SCENE_SIZE;
    let c = |x: f32| V::S(u64::from(x.to_bits()));
    let quad: Vec<u8> =
        [0.0f32, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0].iter().flat_map(|x| x.to_bits().to_le_bytes()).collect();
    let mut s = Vec::new();
    s.extend(call(g::glClearColor, &[c(0.0), c(0.0), c(0.0), c(1.0)]));
    s.extend(call(g::glClear, &[V::S(0x4000)]));
    s.extend(call(
        g::glVertexAttribPointerData,
        &[V::S(0), V::S(2), V::S(0x1406), V::S(0), V::S(8), V::B(&quad), V::S(32)],
    ));
    s.extend(call(g::glDrawArrays, &[V::S(5), V::S(0), V::S(4)]));
    gu.send(1, &s);
    Renderer3d::flush(&mut gu.gfx, 0, 7, Rect::new(0, 0, n, n));
    gu.download(7, n, n, 4)
}

/// The image of [`scene_redraw`]: black, with the quad on the top-right
/// quadrant.
pub fn scene_redraw_expected() -> Vec<u8> {
    let n = SCENE_SIZE;
    scene_expected()
        .as_chunks::<4>()
        .0
        .iter()
        .enumerate()
        .flat_map(|(i, p)| {
            let (x, y) = (i as u32 % n, i as u32 / n);
            if x >= n / 2 && y >= n / 2 { p.to_vec() } else { vec![0, 0, 0, 255] }
        })
        .collect()
}
