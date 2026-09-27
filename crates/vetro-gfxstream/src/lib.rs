//! Host side of gfxstream GLES over virtio-gpu 3D (ADR 0037).
//!
//! The guest (AOSP's `libEGL_emulation`/`libGLESv2_emulation` with
//! `ro.boot.hardware.gltransport=virtio-gpu-pipe`) opens one virtio-gpu
//! context per GL thread and talks through a 1 MiB `PIPE_BUFFER` resource:
//! TRANSFER_TO_HOST_3D writes to the context's pipe, TRANSFER_FROM_HOST_3D
//! reads its replies. The first bytes name the service (`pipe:opengles`,
//! `pipe:GLProcessPipe`). Gralloc buffers (minigbm, virgl backend) are 3D
//! texture resources: each is a gfxstream ColorBuffer whose handle is the
//! resource id. [`Gfxstream`] implements the device's
//! [`Renderer3d`](vetro_platform::virtio::gpu::Renderer3d) and turns
//! everything into the WebGL2 op stream of [`exec`].

#![allow(clippy::too_many_arguments, clippy::collapsible_match)]

pub mod caps;
pub mod exec;
pub mod formats;
pub mod gl;
pub mod glsl;
#[doc(hidden)]
pub mod guest;
mod snapshot;
pub mod state;
pub mod tables;
pub mod wire;

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use vetro_platform::virtio::gpu::{Backing, Create3d, Rect, Renderer3d, Transfer3d};

pub use exec::{GlExecutor, NullExecutor, Recorder};
pub use gl::{Gl, Stats};

/// virtio-gpu error for a bad resource (RESP_ERR_INVALID_RESOURCE_ID).
const ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
/// RESP_ERR_UNSPEC.
const ERR_UNSPEC: u32 = 0x1200;

/// Bytes a pipe may buffer before its data is dropped (a guest bug; the
/// largest single call is a texture upload of a few MiB).
const MAX_PIPE_INPUT: usize = 256 << 20;
/// Queued ops are run at the latest when they reach this size.
const FLUSH_WORDS: usize = 1 << 20;
const FLUSH_BLOB: usize = 32 << 20;

enum Pipe {
    /// Waiting for the NUL-terminated service name.
    Connecting(Vec<u8>),
    /// `pipe:GLProcessPipe`: one confirmation int, answered with the puid.
    Process {
        input: Vec<u8>,
        puid: Option<u64>,
    },
    /// `pipe:opengles`: a render thread.
    Render {
        input: Vec<u8>,
        flags_read: bool,
        thread: gl::Thread,
    },
    Unknown,
}

struct VCtx {
    name: String,
    pipe: Pipe,
    out: VecDeque<u8>,
    resources: BTreeSet<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResKind {
    Pipe,
    ColorBuffer,
    Buffer,
}

struct Res {
    kind: ResKind,
    args: Create3d,
    /// The last context the resource was attached to (pipes).
    ctx: u32,
}

/// The gfxstream renderer.
pub struct Gfxstream {
    pub gl: Gl,
    ctxs: BTreeMap<u32, VCtx>,
    resources: BTreeMap<u32, Res>,
    /// Scanout → (resource, rectangle) of 3D scanouts.
    scanouts: BTreeMap<u32, (u32, Rect)>,
    next_puid: u64,
    /// Process pipes by puid, to clean up what a process leaves behind.
    live_puids: BTreeSet<u64>,
}

/// Resource kind as upstream's `GetResourceType` decides it.
fn kind_of(a: &Create3d) -> ResKind {
    use formats::*;
    if a.target == PIPE_BUFFER {
        return ResKind::Pipe;
    }
    if a.format != VIRGL_FORMAT_R8_UNORM
        || a.bind
            & (VIRGL_BIND_SAMPLER_VIEW | VIRGL_BIND_RENDER_TARGET | VIRGL_BIND_SCANOUT | VIRGL_BIND_CURSOR)
            != 0
        || a.bind & VIRGL_BIND_LINEAR == 0
    {
        return ResKind::ColorBuffer;
    }
    ResKind::Buffer
}

impl Gfxstream {
    /// A renderer drawing with `exec`; `display` is the screen size
    /// reported to the guest (rcGetFBParam).
    pub fn new(exec: Box<dyn GlExecutor>, display: (u32, u32)) -> Self {
        Self {
            gl: Gl::new(exec, display),
            ctxs: BTreeMap::new(),
            resources: BTreeMap::new(),
            scanouts: BTreeMap::new(),
            next_puid: 0,
            live_puids: BTreeSet::new(),
        }
    }

    /// Runs the queued ops now.
    pub fn flush_ops(&mut self) {
        self.gl.flush();
    }

    /// The host log (unhandled calls…), taken.
    pub fn take_log(&mut self) -> Vec<String> {
        std::mem::take(&mut self.gl.log)
    }

    fn maybe_flush(&mut self) {
        let (w, b) = self.gl.ops.size();
        if w > FLUSH_WORDS || b > FLUSH_BLOB {
            self.gl.flush();
        }
    }

    /// Bytes written by the guest into context `ctx`'s pipe.
    fn pipe_write(&mut self, ctx: u32, data: &[u8]) {
        if self.gl.trace {
            let state = match self.ctxs.get(&ctx).map(|v| &v.pipe) {
                Some(Pipe::Connecting(_)) => "connecting",
                Some(Pipe::Process { .. }) => "process",
                Some(Pipe::Render { .. }) => "render",
                Some(Pipe::Unknown) => "unknown",
                None => "no context",
            };
            self.gl.trace_line(format!("ctx {ctx}: write {} bytes ({state})", data.len()));
        }
        let Some(v) = self.ctxs.get_mut(&ctx) else { return };
        let mut data = data.to_vec();
        loop {
            match &mut v.pipe {
                Pipe::Connecting(name) => match data.iter().position(|&b| b == 0) {
                    None => {
                        name.extend_from_slice(&data);
                        return;
                    }
                    Some(n) => {
                        name.extend_from_slice(&data[..n]);
                        let service = String::from_utf8_lossy(name).into_owned();
                        data.drain(..=n);
                        self.gl.trace_line(format!("ctx {ctx}: service {service:?}"));
                        v.pipe = match service.as_str() {
                            "pipe:opengles" => Pipe::Render {
                                input: Vec::new(),
                                flags_read: false,
                                thread: gl::Thread { id: ctx, ..gl::Thread::default() },
                            },
                            "pipe:GLProcessPipe" => Pipe::Process { input: Vec::new(), puid: None },
                            _ => {
                                self.gl.warn(format!("unknown pipe service {service:?}"));
                                Pipe::Unknown
                            }
                        };
                        if data.is_empty() {
                            return;
                        }
                    }
                },
                Pipe::Process { input, puid } => {
                    input.extend_from_slice(&data);
                    if puid.is_none() && input.len() >= 4 {
                        input.drain(..4);
                        self.next_puid += 1;
                        let p = self.next_puid;
                        *puid = Some(p);
                        self.live_puids.insert(p);
                        v.out.extend(p.to_le_bytes());
                    }
                    input.clear();
                    return;
                }
                Pipe::Render { input, flags_read, thread } => {
                    if input.len() + data.len() > MAX_PIPE_INPUT {
                        self.gl.warn(format!("pipe of context {ctx} overflowed: data dropped"));
                        input.clear();
                        return;
                    }
                    input.extend_from_slice(&data);
                    if !*flags_read {
                        if input.len() < 4 {
                            return;
                        }
                        input.drain(..4); // clientFlags
                        *flags_read = true;
                    }
                    let mut reply = Vec::new();
                    let used = self.gl.decode(thread, input, &mut reply);
                    input.drain(..used);
                    if self.gl.trace {
                        let pending = input.len();
                        self.gl.trace_line(format!(
                            "ctx {ctx}: reply {} bytes, {pending} input bytes pending",
                            reply.len()
                        ));
                    }
                    v.out.extend(reply);
                    self.maybe_flush();
                    return;
                }
                Pipe::Unknown => return,
            }
        }
    }

    /// Everything a process (puid) created and did not destroy.
    fn cleanup_process(&mut self, puid: u64) {
        self.live_puids.remove(&puid);
        self.gl.cleanup_process(puid);
    }

    /// The 3D scanout of `scanout`, if any.
    pub fn scanout_resource(&self, scanout: u32) -> Option<(u32, Rect)> {
        self.scanouts.get(&scanout).copied()
    }

    /// Reads the pixels of a ColorBuffer (RGBA-ordered guest bytes of its
    /// format, texture row 0 first): for host tools (screenshots), not a
    /// guest-visible readback.
    pub fn read_color_buffer(&mut self, res: u32) -> Option<(u32, u32, Vec<u8>)> {
        let cb = self.gl.cbs.get(&res)?.clone();
        let px = self.gl.cb_read(res, 0, 0, cb.width, cb.height);
        Some((cb.width, cb.height, px))
    }
}

impl Renderer3d for Gfxstream {
    fn context_create(&mut self, ctx: u32, _context_init: u32, name: &[u8]) -> Result<(), u32> {
        self.ctxs.insert(
            ctx,
            VCtx {
                name: String::from_utf8_lossy(name).into_owned(),
                pipe: Pipe::Connecting(Vec::new()),
                out: VecDeque::new(),
                resources: BTreeSet::new(),
            },
        );
        Ok(())
    }

    fn context_destroy(&mut self, ctx: u32) {
        let Some(v) = self.ctxs.remove(&ctx) else { return };
        match v.pipe {
            Pipe::Render { thread, .. } => {
                // The render thread ends: its context is released.
                let mut t = thread;
                self.gl.release_thread(&mut t);
            }
            Pipe::Process { puid: Some(p), .. } => self.cleanup_process(p),
            _ => {}
        }
        let _ = v.name;
    }

    fn context_attach(&mut self, ctx: u32, res: u32) {
        if let Some(v) = self.ctxs.get_mut(&ctx) {
            v.resources.insert(res);
        }
        if let Some(r) = self.resources.get_mut(&res) {
            r.ctx = ctx;
        }
    }

    fn context_detach(&mut self, ctx: u32, res: u32) {
        if let Some(v) = self.ctxs.get_mut(&ctx) {
            v.resources.remove(&res);
        }
    }

    fn resource_create(&mut self, res: u32, args: &Create3d) -> Result<u64, u32> {
        let kind = kind_of(args);
        let size = match kind {
            ResKind::Pipe | ResKind::Buffer => 0,
            ResKind::ColorBuffer => {
                let tex = formats::tex_of_virgl(args.format);
                self.gl.cb_create(res, args.width, args.height, tex, true);
                u64::from(args.width) * u64::from(args.height) * u64::from(tex.bpp)
            }
        };
        self.resources.insert(res, Res { kind, args: *args, ctx: 0 });
        Ok(size)
    }

    fn resource_destroy(&mut self, res: u32) {
        if let Some(r) = self.resources.remove(&res)
            && r.kind == ResKind::ColorBuffer
        {
            self.gl.cb_destroy(res);
        }
        self.scanouts.retain(|_, v| v.0 != res);
    }

    fn transfer_to_host(
        &mut self,
        ctx: u32,
        res: u32,
        t: &Transfer3d,
        backing: &mut Backing<'_>,
    ) -> Result<(), u32> {
        let r = self.resources.get(&res).ok_or(ERR_INVALID_RESOURCE_ID)?;
        match r.kind {
            ResKind::Pipe => {
                let ctx = if self.ctxs.contains_key(&ctx) { ctx } else { r.ctx };
                let mut buf = vec![0u8; t.bx.w as usize];
                let n = backing.read(t.offset, &mut buf);
                buf.truncate(n);
                self.pipe_write(ctx, &buf);
            }
            ResKind::ColorBuffer => {
                let tex = formats::tex_of_virgl(r.args.format);
                let row = (t.bx.w * tex.bpp) as usize;
                let stride =
                    if t.stride != 0 { t.stride as usize } else { (r.args.width * tex.bpp) as usize };
                let mut data = vec![0u8; stride * t.bx.h.saturating_sub(1) as usize + row];
                backing.read(t.offset, &mut data);
                self.gl.cb_upload(res, t.bx.x, t.bx.y, t.bx.w, t.bx.h, &data, stride);
                self.maybe_flush();
            }
            ResKind::Buffer => {}
        }
        Ok(())
    }

    fn transfer_from_host(
        &mut self,
        ctx: u32,
        res: u32,
        t: &Transfer3d,
        backing: &mut Backing<'_>,
    ) -> Result<(), u32> {
        let r = self.resources.get(&res).ok_or(ERR_INVALID_RESOURCE_ID)?;
        match r.kind {
            ResKind::Pipe => {
                let ctx = if self.ctxs.contains_key(&ctx) { ctx } else { r.ctx };
                let v = self.ctxs.get_mut(&ctx).ok_or(ERR_UNSPEC)?;
                let n = (t.bx.w as usize).min(v.out.len());
                let bytes: Vec<u8> = v.out.drain(..n).collect();
                if self.gl.trace {
                    self.gl.trace_line(format!("ctx {ctx} res {res}: read {} bytes, {n} available", t.bx.w));
                }
                if n < t.bx.w as usize {
                    self.gl.warn(format!("pipe read of {} bytes with {n} available", t.bx.w));
                }
                backing.write(t.offset, &bytes);
            }
            ResKind::ColorBuffer => {
                let tex = formats::tex_of_virgl(r.args.format);
                let row = (t.bx.w * tex.bpp) as usize;
                let stride =
                    if t.stride != 0 { t.stride as usize } else { (r.args.width * tex.bpp) as usize };
                let px = self.gl.cb_read(res, t.bx.x, t.bx.y, t.bx.w, t.bx.h);
                for (k, line) in px.chunks(row.max(1)).enumerate().take(t.bx.h as usize) {
                    backing.write(t.offset + (k * stride) as u64, line);
                }
            }
            ResKind::Buffer => {}
        }
        Ok(())
    }

    fn submit(&mut self, _ctx: u32, _cmd: &[u8]) -> Result<(), u32> {
        // No gfxstream context commands in this slice (native sync is not
        // advertised): execution is synchronous, fences are already done.
        Ok(())
    }

    fn scanout(&mut self, scanout: u32, resource: Option<(u32, Rect)>) {
        match resource {
            Some(r) => {
                self.scanouts.insert(scanout, r);
            }
            None => {
                self.scanouts.remove(&scanout);
            }
        }
    }

    fn flush(&mut self, scanout: u32, res: u32, _dirty: Rect) {
        if self.scanouts.get(&scanout).map(|s| s.0) == Some(res) {
            self.gl.present(res);
        }
    }

    fn reset(&mut self) {
        let ctxs: Vec<u32> = self.ctxs.keys().copied().collect();
        for c in ctxs {
            self.context_destroy(c);
        }
        let res: Vec<u32> = self.resources.keys().copied().collect();
        for r in res {
            self.resource_destroy(r);
        }
        self.gl.flush();
    }

    fn save_state(&self, w: &mut vetro_snapshot::Writer) {
        self.save(w);
    }

    fn restore_state(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        self.restore(r)
    }
}

#[cfg(test)]
mod tests;
