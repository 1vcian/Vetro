//! Snapshots of the gfxstream renderer (ADR 0037, "Snapshots"). The decoder's
//! state (pipes, EGL objects, names, GL state, program and vertex array
//! records) is written as data; what only the GPU holds (ColorBuffers,
//! texture levels in 8-bit formats, buffer contents) is read back by the
//! executor at save time. A restore resets the executor and rebuilds every
//! object with its old id, then uploads the saved contents; the guest resumes
//! with the same names and the same state.
//!
//! Not restorable (documented in docs/specs/gfxstream.md): contents of depth
//! and stencil attachments, renderbuffers, float and packed-format textures,
//! 3D and array textures, query results and transform feedback.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use vetro_snapshot::{Error, Reader, Result, Writer};

use crate::exec::{CODES, Code, Kind, OpBuf};
use crate::formats::{self, Tex};
use crate::gl::{
    AttribPtr, ColorBuffer, Ctx, Gl, LevelSpec, Names, ProgramObj, ShaderObj, Share, Surface, TexInfo,
    Thread, UniformLoc,
};
use crate::state::{self, GlState};
use crate::{Gfxstream, Pipe, Res, ResKind, VCtx, glsl};

const VERSION: u32 = 1;

// ---- small helpers ----

fn w_names(w: &mut Writer, n: &Names) {
    w.u32(n.next_value());
    w.seq(n.pairs(), |w, (k, v)| {
        w.u32(k);
        w.u32(v);
    });
}

fn r_names(r: &mut Reader<'_>) -> Result<Names> {
    let next = r.u32()?;
    let pairs = r.seq(8, |r| Ok((r.u32()?, r.u32()?)))?;
    Ok(Names::from_parts(next, pairs))
}

fn w_u32s(w: &mut Writer, v: &[u32]) {
    w.seq(v, |w, &x| w.u32(x));
}

fn r_u32s(r: &mut Reader<'_>) -> Result<Vec<u32>> {
    r.seq(4, Reader::u32)
}

fn w_params(w: &mut Writer, p: &BTreeMap<u32, (bool, u32)>) {
    w.seq(p, |w, (&k, &(f, v))| {
        w.u32(k);
        w.bool(f);
        w.u32(v);
    });
}

fn r_params(r: &mut Reader<'_>) -> Result<BTreeMap<u32, (bool, u32)>> {
    Ok(r.seq(9, |r| Ok((r.u32()?, (r.bool()?, r.u32()?))))?.into_iter().collect())
}

fn w_var(w: &mut Writer, v: &glsl::Var) {
    w.str(&v.name);
    w.u32(v.ty);
    w.u32(v.size);
    w.bool(v.array);
    w.opt(v.location, Writer::u32);
}

fn r_var(r: &mut Reader<'_>) -> Result<glsl::Var> {
    Ok(glsl::Var {
        name: r.string()?,
        ty: r.u32()?,
        size: r.u32()?,
        array: r.bool()?,
        location: r.opt(Reader::u32)?,
    })
}

fn w_tex(w: &mut Writer, t: &Tex) {
    for v in [t.internal, t.format, t.ty, t.bpp] {
        w.u32(v);
    }
    w.bool(t.swizzle);
    w.bool(t.transferable);
}

fn r_tex(r: &mut Reader<'_>) -> Result<Tex> {
    Ok(Tex {
        internal: r.u32()?,
        format: r.u32()?,
        ty: r.u32()?,
        bpp: r.u32()?,
        swizzle: r.bool()?,
        transferable: r.bool()?,
    })
}

fn f(v: f32) -> u32 {
    v.to_bits()
}

fn w_state(w: &mut Writer, s: &GlState) {
    for &b in &s.enabled {
        w.bool(b);
    }
    for v in s.blend_color {
        w.u32(f(v));
    }
    w_u32s(w, &s.blend_eq);
    w_u32s(w, &s.blend_func);
    for &b in &s.color_mask {
        w.bool(b);
    }
    w.bool(s.depth_mask);
    w_u32s(w, &s.stencil_writemask);
    for v in s.clear_color {
        w.u32(f(v));
    }
    w.u32(f(s.clear_depth));
    w.u32(s.clear_stencil as u32);
    for v in [s.cull_face, s.front_face, s.depth_func] {
        w.u32(v);
    }
    for v in s.depth_range {
        w.u32(f(v));
    }
    w.u32(f(s.line_width));
    for v in s.polygon_offset {
        w.u32(f(v));
    }
    w.u32(f(s.sample_coverage.0));
    w.bool(s.sample_coverage.1);
    for v in s.scissor.iter().chain(&s.viewport) {
        w.u32(*v as u32);
    }
    for (a, b, c) in s.stencil_func {
        w.u32(a);
        w.u32(b as u32);
        w.u32(c);
    }
    for (a, b, c) in s.stencil_op {
        w.u32(a);
        w.u32(b);
        w.u32(c);
    }
    w_u32s(w, &s.hints);
    for v in s.pixel_store {
        w.u32(v as u32);
    }
    w.u32(s.active_texture);
    for u in &s.textures {
        w_u32s(w, u);
    }
    w_u32s(w, &s.samplers);
    for v in [s.program, s.vao] {
        w.u32(v);
    }
    w_u32s(w, &s.buffers);
    for (b, o, z) in s.ubo {
        w.u32(b);
        w.u64(o as u64);
        w.u64(z as u64);
    }
    for v in [s.draw_fbo, s.read_fbo, s.renderbuffer, s.transform_feedback] {
        w.u32(v);
    }
    for (k, v) in s.attribs {
        w.u8(k);
        w_u32s(w, &v);
    }
}

fn arr<const N: usize>(r: &mut Reader<'_>) -> Result<[u32; N]> {
    let v = r_u32s(r)?;
    v.try_into().map_err(|_| Error::invalid("GL state array length"))
}

fn r_state(r: &mut Reader<'_>) -> Result<GlState> {
    let mut s = GlState::default();
    for b in &mut s.enabled {
        *b = r.bool()?;
    }
    for v in &mut s.blend_color {
        *v = f32::from_bits(r.u32()?);
    }
    s.blend_eq = arr(r)?;
    s.blend_func = arr(r)?;
    for b in &mut s.color_mask {
        *b = r.bool()?;
    }
    s.depth_mask = r.bool()?;
    s.stencil_writemask = arr(r)?;
    for v in &mut s.clear_color {
        *v = f32::from_bits(r.u32()?);
    }
    s.clear_depth = f32::from_bits(r.u32()?);
    s.clear_stencil = r.u32()? as i32;
    s.cull_face = r.u32()?;
    s.front_face = r.u32()?;
    s.depth_func = r.u32()?;
    for v in &mut s.depth_range {
        *v = f32::from_bits(r.u32()?);
    }
    s.line_width = f32::from_bits(r.u32()?);
    for v in &mut s.polygon_offset {
        *v = f32::from_bits(r.u32()?);
    }
    s.sample_coverage = (f32::from_bits(r.u32()?), r.bool()?);
    for v in s.scissor.iter_mut().chain(s.viewport.iter_mut()) {
        *v = r.u32()? as i32;
    }
    for x in &mut s.stencil_func {
        *x = (r.u32()?, r.u32()? as i32, r.u32()?);
    }
    for x in &mut s.stencil_op {
        *x = (r.u32()?, r.u32()?, r.u32()?);
    }
    s.hints = arr(r)?;
    for v in &mut s.pixel_store {
        *v = r.u32()? as i32;
    }
    s.active_texture = r.u32()?;
    for u in &mut s.textures {
        *u = arr(r)?;
    }
    s.samplers = arr(r)?;
    s.program = r.u32()?;
    s.vao = r.u32()?;
    s.buffers = arr(r)?;
    for x in &mut s.ubo {
        *x = (r.u32()?, r.u64()? as i64, r.u64()? as i64);
    }
    s.draw_fbo = r.u32()?;
    s.read_fbo = r.u32()?;
    s.renderbuffer = r.u32()?;
    s.transform_feedback = r.u32()?;
    for x in &mut s.attribs {
        *x = (r.u8()?, arr(r)?);
    }
    Ok(s)
}

fn code_of(c: u16) -> Result<Code> {
    CODES.get(c as usize).map(|x| x.0).ok_or_else(|| Error::invalid("op code"))
}

// ---- what is read back at save time ----

/// Texture levels whose content can be read back as RGBA8 and uploaded again
/// in their own format.
fn readable(spec: &LevelSpec, image_target: u32) -> bool {
    use formats::*;
    let target_ok = image_target == 0x0DE1 || (0x8515..=0x851A).contains(&image_target);
    target_ok
        && spec.d <= 1
        && spec.compressed.is_none()
        && spec.ty == GL_UNSIGNED_BYTE
        && matches!(spec.fmt, GL_RGBA | GL_RGB | GL_RG | GL_RED)
        && spec.w > 0
        && spec.h > 0
}

/// RGBA8 rows → the level's format (same row order, tightly packed).
fn from_rgba(fmt: u32, rgba: &[u8]) -> Vec<u8> {
    use formats::*;
    let comps = match fmt {
        GL_RGB => 3,
        GL_RG => 2,
        GL_RED => 1,
        _ => 4,
    };
    if comps == 4 {
        return rgba.to_vec();
    }
    rgba.as_chunks::<4>().0.iter().flat_map(|p| p[..comps].to_vec()).collect()
}

enum Item {
    Cb(u32, usize),
    Level { tex: u32, image: u32, level: u32, n: usize },
    Buffer(u32, usize),
}

impl Gfxstream {
    /// Writes the renderer's state (see the module).
    pub(crate) fn save(&self, w: &mut Writer) {
        let gl = &self.gl;
        gl.run_ops();
        // Readbacks, in one batch.
        let mut items = Vec::new();
        let mut ops = OpBuf::default();
        for (&h, cb) in &gl.cbs {
            if cb.tex.transferable {
                let n = (cb.width * cb.height * cb.tex.bpp) as usize;
                ops.op_read(
                    Code::ReadTexture,
                    &[cb.id, 0, 0, cb.width, cb.height, cb.tex.format, cb.tex.ty, n as u32],
                    n,
                );
                items.push(Item::Cb(h, n));
            }
        }
        for s in gl.shares.values() {
            for (&id, ti) in &s.tex_info {
                if s.aliases.contains(&id) {
                    continue;
                }
                for (&(image, level), spec) in &ti.levels {
                    if readable(spec, image) {
                        let n = (spec.w * spec.h * 4) as usize;
                        ops.op_read(Code::ReadTextureLevel, &[id, image, level, spec.w, spec.h, n as u32], n);
                        items.push(Item::Level { tex: id, image, level, n });
                    }
                }
            }
            for (&id, &size) in &s.buffer_sizes {
                if size > 0 {
                    let n = size as usize;
                    ops.op_read(
                        Code::ReadBufferData,
                        &[id, s.element_buffers.contains(&id) as u32, n as u32],
                        n,
                    );
                    items.push(Item::Buffer(id, n));
                }
            }
        }
        let mut out = vec![0u8; ops.out_len];
        if !ops.is_empty() {
            gl.exec.borrow_mut().execute(&ops.words, &ops.blob, &mut out);
        }

        w.u32(VERSION);
        // Contents first (by kind and id).
        let mut at = 0usize;
        let mut cbs = BTreeMap::new();
        let mut levels = BTreeMap::new();
        let mut bufs = BTreeMap::new();
        for it in &items {
            let (n, slot) = match it {
                Item::Cb(h, n) => (*n, (0u8, *h, 0, 0)),
                Item::Level { tex, image, level, n } => (*n, (1, *tex, *image, *level)),
                Item::Buffer(id, n) => (*n, (2, *id, 0, 0)),
            };
            let bytes = &out[at..at + n];
            at += n;
            match slot.0 {
                0 => {
                    cbs.insert(slot.1, bytes);
                }
                1 => {
                    levels.insert((slot.1, slot.2, slot.3), bytes);
                }
                _ => {
                    bufs.insert(slot.1, bytes);
                }
            }
        }
        w.seq(&cbs, |w, (&h, b)| {
            w.u32(h);
            vetro_snapshot::compress(w, b);
        });
        w.seq(&levels, |w, (&(t, i, l), b)| {
            w.u32(t);
            w.u32(i);
            w.u32(l);
            vetro_snapshot::compress(w, b);
        });
        w.seq(&bufs, |w, (&id, b)| {
            w.u32(id);
            vetro_snapshot::compress(w, b);
        });

        // The renderer's own state.
        w.seq(&self.ctxs, |w, (&c, v)| {
            w.u32(c);
            w.str(&v.name);
            match &v.pipe {
                Pipe::Connecting(n) => {
                    w.u8(0);
                    w.bytes(n);
                }
                Pipe::Process { input, puid } => {
                    w.u8(1);
                    w.bytes(input);
                    w.opt_u64(*puid);
                }
                Pipe::Render { input, flags_read, thread } => {
                    w.u8(2);
                    w.bytes(input);
                    w.bool(*flags_read);
                    for x in [thread.id, thread.ctx, thread.draw, thread.read] {
                        w.u32(x);
                    }
                    w.u64(thread.puid);
                }
                Pipe::Unknown => w.u8(3),
            }
            let out: Vec<u8> = v.out.iter().copied().collect();
            w.bytes(&out);
            w.seq(&v.resources, |w, &r| w.u32(r));
        });
        w.seq(&self.resources, |w, (&id, r)| {
            w.u32(id);
            w.u8(r.kind as u8);
            let a = &r.args;
            for x in [
                a.target,
                a.format,
                a.bind,
                a.width,
                a.height,
                a.depth,
                a.array_size,
                a.last_level,
                a.nr_samples,
                a.flags,
            ] {
                w.u32(x);
            }
            w.u32(r.ctx);
        });
        w.seq(&self.scanouts, |w, (&s, (res, rect))| {
            w.u32(s);
            for x in [*res, rect.x, rect.y, rect.width, rect.height] {
                w.u32(x);
            }
        });
        w.u64(self.next_puid);
        w.seq(&self.live_puids, |w, &p| w.u64(p));
        save_gl(gl, w);
    }

    /// Restores a [`Gfxstream::save`] and rebuilds the executor's objects.
    pub(crate) fn restore(&mut self, r: &mut Reader<'_>) -> Result<()> {
        if r.u32()? != VERSION {
            return Err(Error::invalid("gfxstream snapshot version"));
        }
        let cbs: BTreeMap<u32, Vec<u8>> =
            r.seq(4, |r| Ok((r.u32()?, vetro_snapshot::decompress(r)?)))?.into_iter().collect();
        let levels: BTreeMap<(u32, u32, u32), Vec<u8>> = r
            .seq(12, |r| Ok(((r.u32()?, r.u32()?, r.u32()?), vetro_snapshot::decompress(r)?)))?
            .into_iter()
            .collect();
        let bufs: BTreeMap<u32, Vec<u8>> =
            r.seq(4, |r| Ok((r.u32()?, vetro_snapshot::decompress(r)?)))?.into_iter().collect();

        let n = r.len_of(4)?;
        let mut ctxs = BTreeMap::new();
        for _ in 0..n {
            let c = r.u32()?;
            let name = r.string()?;
            let pipe = match r.u8()? {
                0 => Pipe::Connecting(r.vec()?),
                1 => Pipe::Process { input: r.vec()?, puid: r.opt_u64()? },
                2 => {
                    let input = r.vec()?;
                    let flags_read = r.bool()?;
                    let thread = Thread {
                        id: r.u32()?,
                        ctx: r.u32()?,
                        draw: r.u32()?,
                        read: r.u32()?,
                        puid: r.u64()?,
                    };
                    Pipe::Render { input, flags_read, thread }
                }
                3 => Pipe::Unknown,
                _ => return Err(Error::invalid("gfxstream pipe")),
            };
            let out: VecDeque<u8> = r.vec()?.into();
            let resources: BTreeSet<u32> = r.seq(4, Reader::u32)?.into_iter().collect();
            ctxs.insert(c, VCtx { name, pipe, out, resources });
        }
        let n = r.len_of(4)?;
        let mut resources = BTreeMap::new();
        for _ in 0..n {
            let id = r.u32()?;
            let kind = match r.u8()? {
                0 => ResKind::Pipe,
                1 => ResKind::ColorBuffer,
                _ => ResKind::Buffer,
            };
            let mut v = [0u32; 10];
            for x in &mut v {
                *x = r.u32()?;
            }
            let args = vetro_platform::virtio::gpu::Create3d {
                target: v[0],
                format: v[1],
                bind: v[2],
                width: v[3],
                height: v[4],
                depth: v[5],
                array_size: v[6],
                last_level: v[7],
                nr_samples: v[8],
                flags: v[9],
            };
            resources.insert(id, Res { kind, args, ctx: r.u32()? });
        }
        let n = r.len_of(4)?;
        let mut scanouts = BTreeMap::new();
        for _ in 0..n {
            let s = r.u32()?;
            let res = r.u32()?;
            let rect = vetro_platform::virtio::gpu::Rect::new(r.u32()?, r.u32()?, r.u32()?, r.u32()?);
            scanouts.insert(s, (res, rect));
        }
        self.next_puid = r.u64()?;
        self.live_puids = r.seq(8, Reader::u64)?.into_iter().collect();
        self.ctxs = ctxs;
        self.resources = resources;
        self.scanouts = scanouts;
        restore_gl(&mut self.gl, r)?;
        rebuild(&mut self.gl, &cbs, &levels, &bufs);
        Ok(())
    }
}

fn save_gl(gl: &Gl, w: &mut Writer) {
    w.u32(gl.next_id);
    w.u32(gl.next_handle);
    w.u64(gl.next_sync);
    w.seq(&gl.shares, |w, (&h, s)| {
        w.u32(h);
        w.u32(s.refs);
        for n in [&s.textures, &s.buffers, &s.renderbuffers, &s.samplers, &s.sp] {
            w_names(w, n);
        }
        w.seq(&s.shaders, |w, (&name, sh)| {
            w.u32(name);
            w.u32(sh.id);
            w.u32(sh.ty);
            w.str(&sh.source);
            w.bool(sh.delete_pending);
            w.u32(sh.attached);
        });
        w.seq(&s.programs, |w, (&name, p)| {
            w.u32(name);
            w.u32(p.id);
            w_u32s(w, &p.shaders);
            w.seq(&p.binds, |w, (k, &v)| {
                w.str(k);
                w.u32(v);
            });
            w.bool(p.linked);
            w.seq(&p.uniforms, |w, u| {
                w_var(w, &u.var);
                w.u32(u.base as u32);
            });
            w.seq(&p.attribs, |w, (v, l)| {
                w_var(w, v);
                w.u32(*l as u32);
            });
            w.seq(&p.blocks, |w, b| {
                w.str(&b.name);
                w.u32(b.size);
                w.seq(&b.members, w_var);
                w.opt(b.binding, Writer::u32);
            });
            w.seq(&p.values, |w, (&l, v)| {
                w.u32(l as u32);
                w_u32s(w, v);
            });
            w.bool(p.delete_pending);
            w.seq(&p.block_bindings, |w, (&k, &v)| {
                w.u32(k);
                w.u32(v);
            });
        });
        w.seq(&s.buffer_sizes, |w, (&k, &v)| {
            w.u32(k);
            w.u64(v as u64);
        });
        w.seq(&s.aliases, |w, &a| w.u32(a));
        w.seq(&s.rb_tex, |w, (&k, &v)| {
            w.u32(k);
            w.u32(v);
        });
        w.seq(&s.tex_info, |w, (&id, ti)| {
            w.u32(id);
            w.u32(ti.target);
            w.seq(&ti.levels, |w, (&(t, l), sp)| {
                w.u32(t);
                w.u32(l);
                for x in [sp.ifmt, sp.w, sp.h, sp.d, sp.fmt, sp.ty] {
                    w.u32(x);
                }
                w.opt(sp.compressed.as_ref(), |w, b| w.bytes(b));
            });
            w_params(w, &ti.params);
            w.opt(ti.storage, |w, s| w_u32s(w, &s));
            w.bool(ti.mipmap);
        });
        w.seq(&s.rb_info, |w, (&k, v)| {
            w.u32(k);
            w_u32s(w, v);
        });
        w.seq(&s.sampler_params, |w, (&k, p)| {
            w.u32(k);
            w_params(w, p);
        });
        w.seq(&s.element_buffers, |w, &b| w.u32(b));
        w.seq(&s.buffer_usage, |w, (&k, &v)| {
            w.u32(k);
            w.u32(v);
        });
    });
    w.seq(&gl.ctxs, |w, (&h, c)| {
        w.u32(h);
        for x in [c.share, c.version, c.config] {
            w.u32(x);
        }
        for n in [&c.fbos, &c.vaos, &c.queries, &c.tfbs] {
            w_names(w, n);
        }
        w.u32(c.default_vao);
        w_state(w, &c.state);
        w.seq(&c.vao_elements, |w, (&k, &v)| {
            w.u32(k);
            w.u32(v);
        });
        w.u32(c.draw);
        w.u32(c.read);
        w.bool(c.draw_default);
        w.bool(c.read_default);
        w.bool(c.sized);
        w.seq(&c.attachments, |w, (&(fb, at), &(ty, name, level))| {
            for x in [fb, at, ty, name, level as u32] {
                w.u32(x);
            }
        });
        w.seq(&c.fb_ops, |w, (&(fb, at), (code, args))| {
            w.u32(fb);
            w.u32(at);
            w.u16(*code as u16);
            w_u32s(w, args);
        });
        w.seq(&c.vao_attribs, |w, (&vao, attrs)| {
            w.u32(vao);
            for p in attrs {
                w.bool(p.enabled);
                for x in [p.size, p.ty, p.stride, p.offset, p.buffer, p.divisor] {
                    w.u32(x);
                }
                w.bool(p.norm);
                w.bool(p.integer);
                w.bool(p.client);
            }
        });
    });
    w.seq(&gl.surfaces, |w, (&h, s)| {
        w.u32(h);
        for x in [s.config, s.width, s.height, s.cb, s.fbo, s.depth] {
            w.u32(x);
        }
    });
    w.seq(&gl.cbs, |w, (&h, c)| {
        w.u32(h);
        w.u32(c.width);
        w.u32(c.height);
        w_tex(w, &c.tex);
        w.u32(c.id);
        w.u32(c.refs);
        w.bool(c.resource);
    });
    w.seq(&gl.images, |w, (&k, &v)| {
        w.u32(k);
        w.u32(v);
    });
    w.seq(&gl.owners, |w, (&k, &v)| {
        w.u32(k);
        w.u64(v);
    });
}

fn restore_gl(gl: &mut Gl, r: &mut Reader<'_>) -> Result<()> {
    gl.next_id = r.u32()?;
    gl.next_handle = r.u32()?;
    gl.next_sync = r.u64()?;
    let n = r.len_of(8)?;
    let mut shares = BTreeMap::new();
    for _ in 0..n {
        let h = r.u32()?;
        let mut s = Share { refs: r.u32()?, ..Share::default() };
        s.textures = r_names(r)?;
        s.buffers = r_names(r)?;
        s.renderbuffers = r_names(r)?;
        s.samplers = r_names(r)?;
        s.sp = r_names(r)?;
        for _ in 0..r.len_of(8)? {
            let name = r.u32()?;
            let (id, ty) = (r.u32()?, r.u32()?);
            let source = r.string()?;
            let scan = glsl::scan(&source, ty == 0x8B31);
            let sh = ShaderObj { id, ty, source, scan, delete_pending: r.bool()?, attached: r.u32()? };
            s.shaders.insert(name, sh);
        }
        for _ in 0..r.len_of(8)? {
            let name = r.u32()?;
            let mut p = ProgramObj { id: r.u32()?, shaders: r_u32s(r)?, ..ProgramObj::default() };
            p.binds = r.seq(5, |r| Ok((r.string()?, r.u32()?)))?.into_iter().collect();
            p.linked = r.bool()?;
            p.uniforms = r.seq(8, |r| Ok(UniformLoc { var: r_var(r)?, base: r.u32()? as i32 }))?;
            p.attribs = r.seq(8, |r| Ok((r_var(r)?, r.u32()? as i32)))?;
            p.blocks = r.seq(8, |r| {
                Ok(glsl::Block {
                    name: r.string()?,
                    size: r.u32()?,
                    members: r.seq(8, r_var)?,
                    binding: r.opt(Reader::u32)?,
                })
            })?;
            p.values = r.seq(8, |r| Ok((r.u32()? as i32, r_u32s(r)?)))?.into_iter().collect();
            p.delete_pending = r.bool()?;
            p.block_bindings = r.seq(8, |r| Ok((r.u32()?, r.u32()?)))?.into_iter().collect();
            s.programs.insert(name, p);
        }
        s.buffer_sizes = r.seq(12, |r| Ok((r.u32()?, r.u64()? as i64)))?.into_iter().collect();
        s.aliases = r.seq(4, Reader::u32)?.into_iter().collect();
        s.rb_tex = r.seq(8, |r| Ok((r.u32()?, r.u32()?)))?.into_iter().collect();
        for _ in 0..r.len_of(8)? {
            let id = r.u32()?;
            let mut ti = TexInfo { target: r.u32()?, ..TexInfo::default() };
            for _ in 0..r.len_of(8)? {
                let key = (r.u32()?, r.u32()?);
                let mut v = [0u32; 6];
                for x in &mut v {
                    *x = r.u32()?;
                }
                let compressed = r.opt(Reader::vec)?;
                ti.levels.insert(
                    key,
                    LevelSpec { ifmt: v[0], w: v[1], h: v[2], d: v[3], fmt: v[4], ty: v[5], compressed },
                );
            }
            ti.params = r_params(r)?;
            ti.storage = r.opt(arr::<5>)?;
            ti.mipmap = r.bool()?;
            s.tex_info.insert(id, ti);
        }
        s.rb_info = r.seq(8, |r| Ok((r.u32()?, arr::<4>(r)?)))?.into_iter().collect();
        s.sampler_params = r.seq(8, |r| Ok((r.u32()?, r_params(r)?)))?.into_iter().collect();
        s.element_buffers = r.seq(4, Reader::u32)?.into_iter().collect();
        s.buffer_usage = r.seq(8, |r| Ok((r.u32()?, r.u32()?)))?.into_iter().collect();
        shares.insert(h, s);
    }
    let n = r.len_of(8)?;
    let mut ctxs = BTreeMap::new();
    for _ in 0..n {
        let h = r.u32()?;
        let (share, version, config) = (r.u32()?, r.u32()?, r.u32()?);
        let (fbos, vaos, queries, tfbs) = (r_names(r)?, r_names(r)?, r_names(r)?, r_names(r)?);
        let default_vao = r.u32()?;
        let st = r_state(r)?;
        let vao_elements = r.seq(8, |r| Ok((r.u32()?, r.u32()?)))?.into_iter().collect();
        let (draw, read) = (r.u32()?, r.u32()?);
        let (draw_default, read_default, sized) = (r.bool()?, r.bool()?, r.bool()?);
        let attachments = r
            .seq(20, |r| Ok(((r.u32()?, r.u32()?), (r.u32()?, r.u32()?, r.u32()? as i32))))?
            .into_iter()
            .collect();
        let fb_ops = r
            .seq(10, |r| Ok(((r.u32()?, r.u32()?), (code_of(r.u16()?)?, r_u32s(r)?))))?
            .into_iter()
            .collect();
        let mut vao_attribs = BTreeMap::new();
        for _ in 0..r.len_of(4)? {
            let vao = r.u32()?;
            let mut attrs = [AttribPtr::default(); state::ATTRIBS];
            for p in &mut attrs {
                p.enabled = r.bool()?;
                let mut v = [0u32; 6];
                for x in &mut v {
                    *x = r.u32()?;
                }
                (p.size, p.ty, p.stride, p.offset, p.buffer, p.divisor) =
                    (v[0], v[1], v[2], v[3], v[4], v[5]);
                p.norm = r.bool()?;
                p.integer = r.bool()?;
                p.client = r.bool()?;
            }
            vao_attribs.insert(vao, attrs);
        }
        let c = Ctx {
            share,
            version,
            config,
            fbos,
            vaos,
            queries,
            tfbs,
            default_vao,
            state: st,
            vao_elements,
            draw,
            read,
            draw_default,
            read_default,
            sized,
            attachments,
            fb_ops,
            vao_attribs,
        };
        ctxs.insert(h, c);
    }
    let mut surfaces = BTreeMap::new();
    for _ in 0..r.len_of(28)? {
        let h = r.u32()?;
        let s = Surface {
            config: r.u32()?,
            width: r.u32()?,
            height: r.u32()?,
            cb: r.u32()?,
            fbo: r.u32()?,
            depth: r.u32()?,
        };
        surfaces.insert(h, s);
    }
    let mut cbs = BTreeMap::new();
    for _ in 0..r.len_of(12)? {
        let h = r.u32()?;
        let (width, height) = (r.u32()?, r.u32()?);
        let tex = r_tex(r)?;
        let c = ColorBuffer { width, height, tex, id: r.u32()?, refs: r.u32()?, resource: r.bool()? };
        cbs.insert(h, c);
    }
    gl.images = r.seq(8, |r| Ok((r.u32()?, r.u32()?)))?.into_iter().collect();
    gl.owners = r.seq(12, |r| Ok((r.u32()?, r.u64()?)))?.into_iter().collect();
    gl.shares = shares;
    gl.ctxs = ctxs;
    gl.surfaces = surfaces;
    gl.cbs = cbs;
    gl.applied = GlState::default();
    gl.active = 0;
    Ok(())
}

/// The uniform call of a GLSL type: (code, components, columns, rows).
fn uniform_call(ty: u32) -> (Code, u32, u32, u32) {
    match ty {
        0x1406 => (Code::Uniformfv, 1, 0, 0),
        0x8B50 => (Code::Uniformfv, 2, 0, 0),
        0x8B51 => (Code::Uniformfv, 3, 0, 0),
        0x8B52 => (Code::Uniformfv, 4, 0, 0),
        0x1405 => (Code::Uniformuiv, 1, 0, 0),
        0x8DC6 => (Code::Uniformuiv, 2, 0, 0),
        0x8DC7 => (Code::Uniformuiv, 3, 0, 0),
        0x8DC8 => (Code::Uniformuiv, 4, 0, 0),
        0x8B53 | 0x8B57 => (Code::Uniformiv, 2, 0, 0),
        0x8B54 | 0x8B58 => (Code::Uniformiv, 3, 0, 0),
        0x8B55 | 0x8B59 => (Code::Uniformiv, 4, 0, 0),
        0x8B5A => (Code::UniformMatrixfv, 4, 2, 2),
        0x8B5B => (Code::UniformMatrixfv, 9, 3, 3),
        0x8B5C => (Code::UniformMatrixfv, 16, 4, 4),
        0x8B65 => (Code::UniformMatrixfv, 6, 2, 3),
        0x8B66 => (Code::UniformMatrixfv, 8, 2, 4),
        0x8B67 => (Code::UniformMatrixfv, 6, 3, 2),
        0x8B68 => (Code::UniformMatrixfv, 12, 3, 4),
        0x8B69 => (Code::UniformMatrixfv, 8, 4, 2),
        0x8B6A => (Code::UniformMatrixfv, 12, 4, 3),
        // int, bool, samplers
        _ => (Code::Uniformiv, 1, 0, 0),
    }
}

/// Recreates every object in a reset executor and uploads the contents.
fn rebuild(
    gl: &mut Gl,
    cbs: &BTreeMap<u32, Vec<u8>>,
    levels: &BTreeMap<(u32, u32, u32), Vec<u8>>,
    bufs: &BTreeMap<u32, Vec<u8>>,
) {
    let o = &gl.ops;
    o.op(Code::ResetAll, &[]);
    let create = |kind: Kind, id: u32, extra: u32| {
        if id != 0 {
            o.op(Code::Create, &[kind as u32, id, extra]);
        }
    };
    for (h, cb) in &gl.cbs {
        create(Kind::Texture, cb.id, 0);
        o.op(Code::TexAlloc, &[cb.id, cb.tex.internal, cb.width, cb.height, cb.tex.format, cb.tex.ty]);
        if let Some(px) = cbs.get(h) {
            o.op_blob(Code::TexUpload, &[cb.id, 0, 0, cb.width, cb.height, cb.tex.format, cb.tex.ty], px);
        }
    }
    // Pixel store: tightly packed uploads.
    o.op(Code::PixelStorei, &[0x0CF5, 1]);
    for s in gl.shares.values() {
        for (_, id) in s.textures.pairs() {
            if s.aliases.contains(&id) {
                // A ColorBuffer's texture (rebuilt above): only the sampling
                // parameters the guest set through its alias.
                if let Some(ti) = s.tex_info.get(&id) {
                    o.op(Code::BindTexture, &[0x0DE1, id]);
                    for (&p, &(float, v)) in &ti.params {
                        o.op(if float { Code::TexParameterf } else { Code::TexParameteri }, &[0x0DE1, p, v]);
                    }
                    o.op(Code::BindTexture, &[0x0DE1, 0]);
                }
                continue;
            }
            create(Kind::Texture, id, 0);
            let Some(ti) = s.tex_info.get(&id) else { continue };
            let target = if ti.target == 0 { 0x0DE1 } else { ti.target };
            o.op(Code::BindTexture, &[target, id]);
            if let Some([lv, ifmt, w, h, d]) = ti.storage {
                if target == 0x806F || target == 0x8C1A {
                    o.op(Code::TexStorage3D, &[target, lv, ifmt, w, h, d]);
                } else {
                    o.op(Code::TexStorage2D, &[target, lv, ifmt, w, h]);
                }
            }
            for (&(image, level), spec) in &ti.levels {
                let data = levels.get(&(id, image, level)).map(|px| from_rgba(spec.fmt, px));
                if let Some(c) = &spec.compressed {
                    o.op_blob(Code::CompressedTexImage2D, &[image, level, spec.ifmt, spec.w, spec.h, 0], c);
                } else if ti.storage.is_some() {
                    if let Some(px) = data {
                        o.op_blob(
                            Code::TexSubImage2D,
                            &[image, level, 0, 0, spec.w, spec.h, spec.fmt, spec.ty, 0],
                            &px,
                        );
                    }
                } else if spec.d > 1 || target == 0x806F || target == 0x8C1A {
                    o.op_blob(
                        Code::TexImage3D,
                        &[image, level, spec.ifmt, spec.w, spec.h, spec.d, 0, spec.fmt, spec.ty, 0],
                        &[],
                    );
                } else {
                    let px = data.unwrap_or_default();
                    o.op_blob(
                        Code::TexImage2D,
                        &[image, level, spec.ifmt, spec.w, spec.h, 0, spec.fmt, spec.ty, 0],
                        &px,
                    );
                }
            }
            for (&p, &(float, v)) in &ti.params {
                o.op(if float { Code::TexParameterf } else { Code::TexParameteri }, &[target, p, v]);
            }
            o.op(Code::BindTexture, &[target, 0]);
        }
        for (_, id) in s.buffers.pairs() {
            create(Kind::Buffer, id, 0);
            let size = s.buffer_sizes.get(&id).copied().unwrap_or(0) as usize;
            if size > 0 {
                let usage = s.buffer_usage.get(&id).copied().unwrap_or(0x88E4);
                let data = bufs.get(&id).cloned().unwrap_or_else(|| vec![0; size]);
                o.op_blob(Code::BufferUpload, &[id, s.element_buffers.contains(&id) as u32, usage], &data);
            }
        }
        for (_, id) in s.renderbuffers.pairs() {
            create(Kind::Renderbuffer, id, 0);
            if let Some(&[ifmt, w, h, samples]) = s.rb_info.get(&id) {
                o.op(Code::BindRenderbuffer, &[id]);
                if samples > 0 {
                    o.op(Code::RenderbufferStorageMultisample, &[samples, ifmt, w, h]);
                } else {
                    o.op(Code::RenderbufferStorage, &[ifmt, w, h]);
                }
                o.op(Code::BindRenderbuffer, &[0]);
            }
        }
        for (_, id) in s.samplers.pairs() {
            create(Kind::Sampler, id, 0);
            for (&p, &(float, v)) in s.sampler_params.get(&id).into_iter().flatten() {
                o.op(if float { Code::SamplerParameterf } else { Code::SamplerParameteri }, &[id, p, v]);
            }
        }
        for sh in s.shaders.values() {
            create(Kind::Shader, sh.id, sh.ty);
            o.op_blob(Code::ShaderSource, &[sh.id], glsl::rewrite_for_webgl(&sh.source).as_bytes());
            o.op(Code::CompileShader, &[sh.id]);
        }
        for p in s.programs.values() {
            create(Kind::Program, p.id, 0);
            for name in &p.shaders {
                if let Some(sh) = s.shaders.get(name) {
                    o.op(Code::AttachShader, &[p.id, sh.id]);
                }
            }
            if !p.linked {
                continue;
            }
            for (v, l) in &p.attribs {
                if *l >= 0 && v.location.is_none() {
                    o.op_blob(Code::BindAttribLocation, &[p.id, *l as u32], v.name.as_bytes());
                }
            }
            o.op(Code::LinkProgram, &[p.id]);
            let mut words = vec![p.id];
            let mut names = Vec::new();
            for u in &p.uniforms {
                let (off, len) = (names.len() as u32, u.var.name.len() as u32);
                names.extend_from_slice(u.var.name.as_bytes());
                words.extend_from_slice(&[u.base as u32, u.var.size, off, len, u.var.array as u32]);
            }
            o.op_blob(Code::ProgramUniforms, &words, &names);
            for (&idx, &b) in &p.block_bindings {
                o.op(Code::UniformBlockBinding, &[p.id, idx, b]);
            }
            if !p.values.is_empty() {
                o.op(Code::UseProgram, &[p.id]);
                for (&loc, v) in &p.values {
                    let Some(u) = p.uniform_at(loc) else { continue };
                    let (code, comps, c, rw) = uniform_call(u.var.ty);
                    let data: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
                    if code == Code::UniformMatrixfv {
                        o.op_blob(code, &[loc as u32, 1, comps, c, rw, 0], &data);
                    } else {
                        o.op_blob(code, &[loc as u32, 1, comps], &data);
                    }
                }
                o.op(Code::UseProgram, &[0]);
            }
        }
    }
    for c in gl.ctxs.values() {
        create(Kind::VertexArray, c.default_vao, 0);
        for (_, id) in c.fbos.pairs() {
            create(Kind::Framebuffer, id, 0);
        }
        for (_, id) in c.vaos.pairs() {
            create(Kind::VertexArray, id, 0);
        }
        for (_, id) in c.queries.pairs() {
            create(Kind::Query, id, 0);
        }
        for (_, id) in c.tfbs.pairs() {
            create(Kind::TransformFeedback, id, 0);
        }
        for (&(fbo, _), (code, args)) in &c.fb_ops {
            o.op(Code::BindFramebuffer, &[0x8CA9, fbo]);
            o.op(*code, args);
        }
        o.op(Code::BindFramebuffer, &[0x8CA9, 0]);
        for (&vao, attrs) in &c.vao_attribs {
            o.op(Code::BindVertexArray, &[vao]);
            for (i, p) in attrs.iter().enumerate() {
                let i = i as u32;
                if !p.client && p.buffer != 0 {
                    o.op(Code::BindBuffer, &[0x8892, p.buffer]);
                    if p.integer {
                        o.op(Code::VertexAttribIPointer, &[i, p.size, p.ty, p.stride, p.offset]);
                    } else {
                        o.op(
                            Code::VertexAttribPointer,
                            &[i, p.size, p.ty, p.norm as u32, p.stride, p.offset],
                        );
                    }
                }
                if p.enabled {
                    o.op(Code::EnableVertexAttribArray, &[i]);
                }
                if p.divisor != 0 {
                    o.op(Code::VertexAttribDivisor, &[i, p.divisor]);
                }
            }
            if let Some(&e) = c.vao_elements.get(&vao) {
                o.op(Code::BindBuffer, &[0x8893, e]);
            }
        }
        o.op(Code::BindVertexArray, &[0]);
    }
    for s in gl.surfaces.values() {
        create(Kind::Framebuffer, s.fbo, 0);
        create(Kind::Renderbuffer, s.depth, 0);
        let tex = gl.cbs.get(&s.cb).map_or(0, |c| c.id);
        o.op(Code::SurfaceAttach, &[s.fbo, tex, s.depth, s.width, s.height]);
    }
    o.op(Code::ResetState, &[]);
    gl.applied = GlState::default();
    gl.active = 0;
    gl.flush();
}
