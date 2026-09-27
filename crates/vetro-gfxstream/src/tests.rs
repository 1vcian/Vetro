//! The decoder driven like the guest's gfxstream libraries drive it: pipes
//! over virtio-gpu 3D resources, renderControl and GLES calls encoded with
//! the generated tables, ops checked on a recording executor.

use std::cell::RefCell;
use std::rc::Rc;

use vetro_platform::virtio::gpu::{Backing, Box3d, Create3d, Rect, Renderer3d, Transfer3d};

use crate::exec::{CODES, Code, GlExecutor};
use crate::formats;
use crate::guest::{Guest, V, call};
use crate::tables::{gles2 as g, rc};

/// Records batches; read ops get bytes 0, 1, 2… (mod 251).
#[derive(Default)]
struct Log {
    batches: Vec<(Vec<u32>, Vec<u8>)>,
}

struct FakeExec(Rc<RefCell<Log>>);

impl GlExecutor for FakeExec {
    fn execute(&mut self, words: &[u32], blob: &[u8], out: &mut [u8]) {
        for (i, b) in out.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        self.0.borrow_mut().batches.push((words.to_vec(), blob.to_vec()));
    }
}

/// Ops of all batches: (code, argument words).
fn ops(log: &Rc<RefCell<Log>>) -> Vec<(Code, Vec<u32>)> {
    let mut v = Vec::new();
    for (words, _) in &log.borrow().batches {
        let mut i = 0;
        while i < words.len() {
            let n = (words[i] & 0xffff) as usize;
            v.push((CODES[(words[i] >> 16) as usize].0, words[i + 1..i + n].to_vec()));
            i += n;
        }
    }
    v
}

fn guest() -> (Guest, Rc<RefCell<Log>>) {
    let log = Rc::new(RefCell::new(Log::default()));
    (Guest::new(Box::new(FakeExec(log.clone()))), log)
}

#[test]
fn process_pipe_and_render_control_handshake() {
    let (mut gu, _log) = guest();
    gu.open(1, "pipe:GLProcessPipe");
    gu.send(1, &100i32.to_le_bytes());
    let puid = u64::from_le_bytes(gu.recv(1, 8).try_into().unwrap());
    assert_eq!(puid, 1);

    gu.open(2, "pipe:opengles");
    let mut s = 0u32.to_le_bytes().to_vec(); // clientFlags
    s.extend(call(rc::rcGetRendererVersion, &[]));
    gu.send(2, &s);
    assert_eq!(gu.u32(2), 1);
    // The host extensions, too-small buffer first (negative size).
    gu.send(2, &call(rc::rcGetHostExtensionsString, &[V::S(4), V::O(4)]));
    let r = gu.recv(2, 8);
    let need = -i32::from_le_bytes(r[4..8].try_into().unwrap());
    assert_eq!(need as usize, crate::caps::GL_EXTENSIONS.len() + 1);
    gu.send(2, &call(rc::rcGetHostExtensionsString, &[V::S(need as u64), V::O(need as u32)]));
    let r = gu.recv(2, need as usize + 4);
    let s = String::from_utf8_lossy(&r[..need as usize - 1]).into_owned();
    assert!(s.contains("ANDROID_EMU_gles_max_version_3_0"));
    assert!(!s.contains("native_sync"), "no native sync in this slice");
    // Configs: count and attribute count, then the table.
    gu.send(2, &call(rc::rcGetNumConfigs, &[V::O(4)]));
    let r = gu.recv(2, 8);
    let (nattr, ncfg) =
        (u32::from_le_bytes(r[0..4].try_into().unwrap()), u32::from_le_bytes(r[4..8].try_into().unwrap()));
    assert_eq!((nattr, ncfg), (34, 6));
    let size = (ncfg + 1) * nattr * 4;
    gu.send(2, &call(rc::rcGetConfigs, &[V::S(u64::from(size)), V::O(size)]));
    let r = gu.recv(2, size as usize + 4);
    assert_eq!(u32::from_le_bytes(r[..4].try_into().unwrap()), 0x3025, "EGL_DEPTH_SIZE first");
    let attribs: Vec<u8> = [0x3024u32, 8, 0x3021, 8, 0x3038].iter().flat_map(|v| v.to_le_bytes()).collect();
    gu.send(2, &call(rc::rcChooseConfig, &[V::B(&attribs), V::S(20), V::O(8), V::S(2)]));
    let r = gu.recv(2, 12);
    assert_eq!(&r[8..12], &2u32.to_le_bytes(), "two RGBA8888 configs");
    assert_eq!(&r[0..4], &0u32.to_le_bytes());
    assert!(gu.gfx.gl.log.is_empty(), "{:?}", gu.gfx.gl.log);
}

/// A window surface on a gralloc buffer, a GL program and a frame, like
/// SurfaceFlinger's first composition.
#[test]
fn surface_program_and_frame() {
    let (mut gu, log) = guest();
    // The gralloc buffer: an RGBA8888 render target resource (ColorBuffer 7).
    let rgba = Create3d {
        target: 2,
        format: formats::VIRGL_FORMAT_R8G8B8A8_UNORM,
        bind: formats::VIRGL_BIND_RENDER_TARGET
            | formats::VIRGL_BIND_SAMPLER_VIEW
            | formats::VIRGL_BIND_SCANOUT,
        width: 64,
        height: 32,
        depth: 1,
        array_size: 1,
        ..Create3d::default()
    };
    assert_eq!(gu.gfx.resource_create(7, &rgba).unwrap(), 64 * 32 * 4);

    gu.open(1, "pipe:opengles");
    let mut s = 0u32.to_le_bytes().to_vec();
    s.extend(call(rc::rcCreateContext, &[V::S(1), V::S(0), V::S(3)]));
    gu.send(1, &s);
    let ctx = gu.u32(1);
    gu.send(1, &call(rc::rcCreateWindowSurface, &[V::S(1), V::S(64), V::S(32)]));
    let surf = gu.u32(1);
    let mut s = call(rc::rcSetWindowColorBuffer, &[V::S(u64::from(surf)), V::S(7)]);
    s.extend(call(rc::rcMakeCurrent, &[V::S(u64::from(ctx)), V::S(u64::from(surf)), V::S(u64::from(surf))]));
    gu.send(1, &s);
    assert_eq!(gu.u32(1), 1, "EGL_TRUE");

    // Program: a vertex and a fragment shader with an external sampler.
    let vs = b"#version 300 es\nin vec2 aPos;\nin vec2 aUv;\nuniform mat4 uMvp;\nout vec2 vUv;\nvoid main() { vUv = aUv; gl_Position = uMvp * vec4(aPos, 0.0, 1.0); }\n\0";
    let fs = b"#version 300 es\n#extension GL_OES_EGL_image_external_essl3 : require\nprecision mediump float;\nuniform samplerExternalOES uTex;\nuniform vec4 uTint[2];\nin vec2 vUv;\nout vec4 o;\nvoid main() { o = texture(uTex, vUv) * uTint[1]; }\n\0";
    gu.send(1, &call(g::glCreateShader, &[V::S(0x8B31)]));
    let vsh = gu.u32(1);
    gu.send(1, &call(g::glCreateShader, &[V::S(0x8B30)]));
    let fsh = gu.u32(1);
    gu.send(1, &call(g::glCreateProgram, &[]));
    let prog = gu.u32(1);
    let mut s = Vec::new();
    s.extend(call(g::glShaderString, &[V::S(u64::from(vsh)), V::B(vs), V::S(vs.len() as u64)]));
    s.extend(call(g::glShaderString, &[V::S(u64::from(fsh)), V::B(fs), V::S(fs.len() as u64)]));
    s.extend(call(g::glCompileShader, &[V::S(u64::from(vsh))]));
    s.extend(call(g::glCompileShader, &[V::S(u64::from(fsh))]));
    s.extend(call(g::glAttachShader, &[V::S(u64::from(prog)), V::S(u64::from(vsh))]));
    s.extend(call(g::glAttachShader, &[V::S(u64::from(prog)), V::S(u64::from(fsh))]));
    s.extend(call(g::glBindAttribLocation, &[V::S(u64::from(prog)), V::S(3), V::B(b"aUv\0")]));
    s.extend(call(g::glLinkProgram, &[V::S(u64::from(prog))]));
    s.extend(call(g::glGetProgramiv, &[V::S(u64::from(prog)), V::S(0x8B82), V::O(4)]));
    gu.send(1, &s);
    assert_eq!(gu.u32(1), 1, "LINK_STATUS");
    gu.send(1, &call(g::glGetProgramiv, &[V::S(u64::from(prog)), V::S(0x8B86), V::O(4)]));
    assert_eq!(gu.u32(1), 3, "ACTIVE_UNIFORMS: uMvp, uTex, uTint");
    // glGetActiveUniform(2): "uTint[0]", size 2, FLOAT_VEC4.
    gu.send(
        1,
        &call(
            g::glGetActiveUniform,
            &[V::S(u64::from(prog)), V::S(2), V::S(32), V::O(4), V::O(4), V::O(4), V::O(32)],
        ),
    );
    let r = gu.recv(1, 44);
    assert_eq!(&r[0..4], &8u32.to_le_bytes());
    assert_eq!(&r[4..8], &2u32.to_le_bytes());
    assert_eq!(&r[8..12], &0x8B52u32.to_le_bytes());
    assert_eq!(&r[12..20], b"uTint[0]");
    gu.send(
        1,
        &call(
            g::glGetActiveUniform,
            &[V::S(u64::from(prog)), V::S(1), V::S(32), V::O(0), V::O(4), V::O(4), V::O(32)],
        ),
    );
    let r = gu.recv(1, 40);
    assert_eq!(&r[4..8], &0x8D66u32.to_le_bytes(), "the guest still sees samplerExternalOES");
    // Locations: uMvp 0, uTex 1, uTint 2..3; attributes: aUv bound to 3,
    // aPos gets the lowest free one.
    for (name, want) in
        [(&b"uMvp\0"[..], 0), (b"uTex\0", 1), (b"uTint[1]\0", 3), (b"uTint[2]\0", -1), (b"nope\0", -1)]
    {
        gu.send(1, &call(g::glGetUniformLocation, &[V::S(u64::from(prog)), V::B(name)]));
        assert_eq!(gu.u32(1) as i32, want, "{}", String::from_utf8_lossy(name));
    }
    gu.send(1, &call(g::glGetAttribLocation, &[V::S(u64::from(prog)), V::B(b"aPos\0")]));
    assert_eq!(gu.u32(1), 0);
    gu.send(1, &call(g::glGetAttribLocation, &[V::S(u64::from(prog)), V::B(b"aUv\0")]));
    assert_eq!(gu.u32(1), 3);

    // A frame: clear, draw with a client-side vertex array, present.
    let tint: Vec<u8> = [1.0f32, 0.5, 0.25, 1.0].iter().flat_map(|f| f.to_bits().to_le_bytes()).collect();
    let verts: Vec<u8> = (0..8).flat_map(|i| (i as f32).to_bits().to_le_bytes()).collect();
    let mut s = Vec::new();
    s.extend(call(g::glUseProgram, &[V::S(u64::from(prog))]));
    s.extend(call(g::glUniform4fv, &[V::S(3), V::S(1), V::B(&tint)]));
    s.extend(call(g::glClearColor, &[V::S(0), V::S(0), V::S(0), V::S(u64::from(1f32.to_bits()))]));
    s.extend(call(g::glClear, &[V::S(0x4000)]));
    s.extend(call(g::glEnableVertexAttribArray, &[V::S(0)]));
    s.extend(call(
        g::glVertexAttribPointerData,
        &[V::S(0), V::S(2), V::S(0x1406), V::S(0), V::S(8), V::B(&verts), V::S(32)],
    ));
    s.extend(call(g::glDrawArrays, &[V::S(5), V::S(0), V::S(4)]));
    s.extend(call(rc::rcFlushWindowColorBufferAsync, &[V::S(u64::from(surf))]));
    gu.send(1, &s);
    gu.gfx.scanout(0, Some((7, Rect::new(0, 0, 64, 32))));
    Renderer3d::flush(&mut gu.gfx, 0, 7, Rect::new(0, 0, 64, 32));

    let o = ops(&log);
    let codes: Vec<Code> = o.iter().map(|(c, _)| *c).collect();
    // The ColorBuffer's texture, the surface on it, then the program.
    assert_eq!(codes[0], Code::Create);
    assert_eq!(codes[1], Code::TexAlloc);
    let cb_tex = o[0].1[1];
    let attach = o.iter().find(|(c, _)| *c == Code::SurfaceAttach).unwrap();
    assert_eq!(attach.1[1], cb_tex, "surface framebuffer on the ColorBuffer");
    assert_eq!(&attach.1[3..5], &[64, 32]);
    // Viewport from the surface at the first make-current.
    assert!(o.iter().any(|(c, a)| *c == Code::Viewport && a == &[0, 0, 64, 32]));
    // The auto-assigned attribute location is bound before the link.
    let link = codes.iter().position(|c| *c == Code::LinkProgram).unwrap();
    let binds: Vec<usize> =
        codes.iter().enumerate().filter(|(_, c)| **c == Code::BindAttribLocation).map(|(i, _)| i).collect();
    assert_eq!(binds.len(), 2);
    assert!(binds.iter().all(|&b| b < link));
    assert_eq!(codes[link + 1], Code::ProgramUniforms);
    assert!(codes.contains(&Code::Uniformfv));
    assert!(codes.contains(&Code::VertexData));
    assert!(codes.contains(&Code::DrawArrays));
    let present = o.iter().find(|(c, _)| *c == Code::Present).unwrap();
    assert_eq!(present.1, [cb_tex, 64, 32]);
    // The shader text given to WebGL uses sampler2D.
    let lb = log.borrow();
    let blob = &lb.batches.last().unwrap().1;
    let text = String::from_utf8_lossy(blob);
    assert!(text.contains("uniform sampler2D uTex"));
    assert!(!text.contains("GL_OES_EGL_image_external"));
    drop(lb);
    assert!(gu.gfx.gl.log.is_empty(), "{:?}", gu.gfx.gl.log);
}

#[test]
fn color_buffer_transfers_swizzle_bgra_and_read_back() {
    let (mut gu, log) = guest();
    let bgra = Create3d {
        target: 2,
        format: formats::VIRGL_FORMAT_B8G8R8A8_UNORM,
        bind: formats::VIRGL_BIND_SAMPLER_VIEW,
        width: 4,
        height: 2,
        depth: 1,
        array_size: 1,
        ..Create3d::default()
    };
    gu.gfx.resource_create(9, &bgra).unwrap();
    let ents = [(0x10_0000u64, 32u32)];
    let px: Vec<u8> = (0..32).collect();
    {
        let mut b = Backing::new(&mut gu.ram, &ents);
        b.write(0, &px);
        // The second row, pixels 1..3 (stride 16).
        let t = Transfer3d {
            bx: Box3d { x: 1, y: 1, w: 2, h: 1, d: 1, z: 0 },
            offset: 20,
            stride: 16,
            ..Transfer3d::default()
        };
        gu.gfx.transfer_to_host(0, 9, &t, &mut b).unwrap();
    }
    gu.gfx.flush_ops();
    let o = ops(&log);
    let up = o.iter().find(|(c, _)| *c == Code::TexUpload).unwrap();
    assert_eq!(&up.1[1..5], &[1, 1, 2, 1]);
    let lb = log.borrow();
    let blob = &lb.batches[0].1;
    let (off, len) = (up.1[7] as usize, up.1[8] as usize);
    assert_eq!(&blob[off..off + len], &[22, 21, 20, 23, 26, 25, 24, 27], "BGRA → RGBA");
    drop(lb);
    // Read back into the guest: RGBA from the executor → BGRA bytes.
    {
        let mut b = Backing::new(&mut gu.ram, &ents);
        let t = Transfer3d {
            bx: Box3d { x: 0, y: 0, w: 4, h: 2, d: 1, z: 0 },
            offset: 0,
            stride: 16,
            ..Transfer3d::default()
        };
        gu.gfx.transfer_from_host(0, 9, &t, &mut b).unwrap();
    }
    let mut back = vec![0u8; 8];
    Backing::new(&mut gu.ram, &ents).read(0, &mut back);
    assert_eq!(back, [2, 1, 0, 3, 6, 5, 4, 7]);
    assert!(ops(&log).iter().any(|(c, a)| *c == Code::ReadTexture && a[1..5] == [0, 0, 4, 2]));
}

#[test]
fn two_threads_switch_contexts_with_minimal_state() {
    let (mut gu, log) = guest();
    for c in [1, 2] {
        gu.open(c, "pipe:opengles");
        let mut s = 0u32.to_le_bytes().to_vec();
        s.extend(call(rc::rcCreateContext, &[V::S(0), V::S(0), V::S(3)]));
        gu.send(c, &s);
        let ctx = gu.u32(c);
        gu.send(c, &call(rc::rcMakeCurrent, &[V::S(u64::from(ctx)), V::S(0), V::S(0)]));
        assert_eq!(gu.u32(c), 1);
    }
    gu.send(1, &call(g::glEnable, &[V::S(0x0BE2)]));
    gu.send(2, &call(g::glClearColor, &[V::S(0), V::S(u64::from(1f32.to_bits())), V::S(0), V::S(0)]));
    gu.send(1, &call(g::glClear, &[V::S(0x4000)]));
    gu.gfx.flush_ops();
    let codes: Vec<Code> = ops(&log).iter().map(|(c, _)| *c).collect();
    // ctx1: Enable(BLEND); switch to ctx2: its default vertex array,
    // Disable(BLEND); ClearColor; back to ctx1: Enable, clear color back,
    // its vertex array; Clear.
    let tail: Vec<Code> = codes.iter().copied().filter(|c| *c != Code::Create).collect();
    assert_eq!(
        tail,
        [
            Code::BindVertexArray,
            Code::Enable,
            Code::Disable,
            Code::BindVertexArray,
            Code::ClearColor,
            Code::Enable,
            Code::ClearColor,
            Code::BindVertexArray,
            Code::Clear
        ]
    );
}

#[test]
fn synthetic_scene_binds_the_cpu_buffer_as_external_texture() {
    let (mut gu, log) = guest();
    let back = crate::guest::scene(&mut gu);
    assert_eq!(back.len(), 64 * 64 * 4);
    assert_eq!(crate::guest::scene_expected().len(), back.len());
    let o = ops(&log);
    let cb8 = gu.gfx.gl.cbs[&8].id;
    // rcBindTexture: the texture bound on unit 0 becomes buffer 8's.
    assert!(o.iter().any(|(c, a)| *c == Code::BindTexture && a == &[0x0DE1, cb8]));
    assert!(o.iter().any(|(c, _)| *c == Code::Present));
    assert!(o.iter().any(|(c, a)| *c == Code::ReadTexture && a[1..5] == [0, 0, 64, 64]));
    assert!(gu.gfx.gl.log.is_empty(), "{:?}", gu.gfx.gl.log);
}

/// Save after the scene, restore into a new renderer: the executor gets the
/// objects back with their ids and the read-back contents, and the guest's
/// render thread goes on with its context, program and pipe.
#[test]
fn snapshot_rebuilds_objects_and_the_guest_goes_on() {
    let (mut gu, log) = guest();
    crate::guest::scene(&mut gu);
    // A buffer object with content, to see it come back.
    let mut s = call(g::glGenBuffers, &[V::S(1), V::O(4)]);
    s.extend(call(g::glFinishRoundTrip, &[]));
    gu.send(1, &s);
    let r = gu.recv(1, 8);
    let buf = u32::from_le_bytes(r[..4].try_into().unwrap());
    let mut s = call(g::glBindBuffer, &[V::S(0x8892), V::S(u64::from(buf))]);
    s.extend(call(g::glBufferData, &[V::S(0x8892), V::S(12), V::B(&[1; 12]), V::S(0x88E4)]));
    gu.send(1, &s);
    let cb7 = gu.gfx.gl.cbs[&7].id;
    let prog_ids: Vec<u32> =
        gu.gfx.gl.shares.values().flat_map(|s| s.programs.values().map(|p| p.id)).collect();

    let mut w = vetro_snapshot::Writer::new();
    gu.gfx.save_state(&mut w);
    let bytes = w.into_bytes();
    let saves = log.borrow().batches.len();

    let (mut other, log2) = guest();
    other.gfx.restore_state(&mut vetro_snapshot::Reader::new(&bytes)).unwrap();
    let o = ops(&log2);
    assert_eq!(o[0].0, Code::ResetAll);
    assert_eq!(o.last().unwrap().0, Code::ResetState);
    // ColorBuffer 7 with the bytes the save read back (the fake executor's
    // 0, 1, 2… pattern, whose position depends on the batch layout).
    let up = o.iter().find(|(c, a)| *c == Code::TexUpload && a[0] == cb7).expect("ColorBuffer 7 uploaded");
    assert_eq!(&up.1[1..5], &[0, 0, 64, 64]);
    for id in &prog_ids {
        assert!(o.iter().any(|(c, a)| *c == Code::LinkProgram && a == &[*id]), "program {id} relinked");
    }
    assert!(o.iter().any(|(c, a)| *c == Code::BufferUpload && a[2] == 0x88E4), "buffer content back");
    assert!(o.iter().any(|(c, _)| *c == Code::SurfaceAttach));
    assert!(saves > 0);

    // The guest goes on: same pipe (context 1), same GL context and names.
    let mut s = call(g::glClear, &[V::S(0x4000)]);
    s.extend(call(g::glFinishRoundTrip, &[]));
    other.send(1, &s);
    assert_eq!(other.recv(1, 4), [0, 0, 0, 0]);
    let o = ops(&log2);
    let n = o.len();
    assert!(o[n - 3..].iter().any(|(c, _)| *c == Code::Clear), "{:?}", &o[n - 6..]);
    assert!(other.gfx.gl.log.is_empty(), "{:?}", other.gfx.gl.log);
}
