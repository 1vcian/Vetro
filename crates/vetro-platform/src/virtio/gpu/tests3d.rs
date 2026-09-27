//! virtio-gpu 3D (ADR 0036): the device side, with a renderer that records
//! what it receives.

use super::*;
use crate::virtio::testdrv::{Driver, Transport};

/// Renderer that logs calls and echoes pipe-like data: TRANSFER_TO_HOST
/// appends the bytes it read to `written`, TRANSFER_FROM_HOST writes
/// `reply` at the transfer offset.
#[derive(Default)]
struct Mock {
    log: Vec<String>,
    written: Vec<u8>,
    reply: Vec<u8>,
    submits: Vec<Vec<u8>>,
    state: u32,
}

impl Renderer3d for Mock {
    fn capsets(&self) -> Vec<Capset> {
        vec![Capset { id: 5, max_version: 2, data: vec![1, 2, 3, 4, 5, 6] }]
    }
    fn context_create(&mut self, ctx: u32, init: u32, name: &[u8]) -> Result<(), u32> {
        self.log.push(format!("create {ctx} {init} {}", String::from_utf8_lossy(name)));
        Ok(())
    }
    fn context_destroy(&mut self, ctx: u32) {
        self.log.push(format!("destroy {ctx}"));
    }
    fn context_attach(&mut self, ctx: u32, res: u32) {
        self.log.push(format!("attach {ctx} {res}"));
    }
    fn context_detach(&mut self, ctx: u32, res: u32) {
        self.log.push(format!("detach {ctx} {res}"));
    }
    fn resource_create(&mut self, res: u32, a: &Create3d) -> Result<u64, u32> {
        self.log
            .push(format!("res {res} t{} f{} b{:#x} {}x{}", a.target, a.format, a.bind, a.width, a.height));
        Ok(u64::from(a.width) * u64::from(a.height) * 4)
    }
    fn resource_destroy(&mut self, res: u32) {
        self.log.push(format!("unref {res}"));
    }
    fn transfer_to_host(
        &mut self,
        ctx: u32,
        res: u32,
        t: &Transfer3d,
        b: &mut Backing<'_>,
    ) -> Result<(), u32> {
        let mut buf = vec![0u8; t.bx.w as usize];
        let n = b.read(t.offset, &mut buf);
        self.written.extend_from_slice(&buf[..n]);
        self.log.push(format!("to_host {ctx} {res} x{} w{} off{}", t.bx.x, t.bx.w, t.offset));
        Ok(())
    }
    fn transfer_from_host(
        &mut self,
        ctx: u32,
        res: u32,
        t: &Transfer3d,
        b: &mut Backing<'_>,
    ) -> Result<(), u32> {
        let reply = core::mem::take(&mut self.reply);
        b.write(t.offset, &reply);
        self.log.push(format!("from_host {ctx} {res} w{}", t.bx.w));
        Ok(())
    }
    fn submit(&mut self, ctx: u32, cmd: &[u8]) -> Result<(), u32> {
        self.log.push(format!("submit {ctx} {}", cmd.len()));
        self.submits.push(cmd.to_vec());
        Ok(())
    }
    fn scanout(&mut self, scanout: u32, res: Option<(u32, Rect)>) {
        self.log.push(format!("scanout {scanout} {:?}", res.map(|r| r.0)));
    }
    fn flush(&mut self, scanout: u32, res: u32, dirty: Rect) {
        self.log
            .push(format!("flush {scanout} {res} {}x{}+{}+{}", dirty.width, dirty.height, dirty.x, dirty.y));
    }
    fn save_state(&self, w: &mut vetro_snapshot::Writer) {
        w.u32(self.state);
    }
    fn restore_state(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        self.state = r.u32()?;
        Ok(())
    }
}

fn gpu3d() -> VirtioGpu {
    let mut g =
        VirtioGpu::new(Box::new(MemDisplay::default()), GpuConfig { virgl: true, ..GpuConfig::default() });
    g.set_renderer(Box::new(Mock::default()));
    g
}

fn driver3d() -> Driver<VirtioMmio> {
    let mut d = Driver::new(VirtioMmio::new(Box::new(gpu3d())));
    d.init(u64::MAX, 64);
    d
}

fn gpu(d: &mut Driver<VirtioMmio>) -> &mut VirtioGpu {
    d.t.device_as_mut::<VirtioGpu>().unwrap()
}

fn mock(d: &mut Driver<VirtioMmio>) -> &mut Mock {
    gpu(d).renderer_as_mut::<Mock>().unwrap()
}

fn log(d: &mut Driver<VirtioMmio>) -> Vec<String> {
    core::mem::take(&mut mock(d).log)
}

/// A command with header context `ctx`.
fn cmd_ctx(ty: u32, ctx: u32, words: &[u32]) -> Vec<u8> {
    let mut c = vec![0u8; HDR_LEN];
    c[0..4].copy_from_slice(&ty.to_le_bytes());
    c[16..20].copy_from_slice(&ctx.to_le_bytes());
    for w in words {
        c.extend_from_slice(&w.to_le_bytes());
    }
    c
}

fn ctrl(d: &mut Driver<VirtioMmio>, c: &[u8], resp_len: u32) -> Vec<u8> {
    let a = d.buf(c);
    let r = d.alloc(u64::from(resp_len), 8);
    d.add(CTRLQ, &[(a, c.len() as u32, false), (r, resp_len, true)]);
    d.service();
    let (_, len) = d.pop_used(CTRLQ).expect("response");
    d.mem(r, len as usize)
}

fn resp(d: &mut Driver<VirtioMmio>, c: &[u8]) -> u32 {
    le32(&ctrl(d, c, 24), 0)
}

fn ctx_create(d: &mut Driver<VirtioMmio>, ctx: u32, name: &str) -> u32 {
    let mut c = cmd_ctx(CMD_CTX_CREATE, ctx, &[name.len() as u32, 0]);
    let mut n = [0u8; 64];
    n[..name.len()].copy_from_slice(name.as_bytes());
    c.extend_from_slice(&n);
    resp(d, &c)
}

/// RESOURCE_CREATE_3D: id, target, format, bind, width, height (depth 1).
fn create3d(d: &mut Driver<VirtioMmio>, id: u32, target: u32, format: u32, bind: u32, w: u32, h: u32) -> u32 {
    resp(d, &cmd_ctx(CMD_RESOURCE_CREATE_3D, 0, &[id, target, format, bind, w, h, 1, 1, 0, 0, 0, 0]))
}

fn attach(d: &mut Driver<VirtioMmio>, id: u32, len: u32) -> u64 {
    let a = d.alloc(u64::from(len), 64);
    let mut c = cmd_ctx(CMD_RESOURCE_ATTACH_BACKING, 0, &[id, 1]);
    c.extend_from_slice(&a.to_le_bytes());
    c.extend_from_slice(&len.to_le_bytes());
    c.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(resp(d, &c), RESP_OK_NODATA);
    a
}

/// TRANSFER_*_HOST_3D of the byte range [x, x + w) of a buffer resource.
fn transfer(ty: u32, ctx: u32, id: u32, x: u32, w: u32) -> Vec<u8> {
    let mut c = cmd_ctx(ty, ctx, &[x, 0, 0, w, 1, 1]);
    c.extend_from_slice(&u64::from(x).to_le_bytes());
    for v in [id, 0, 0, 0] {
        c.extend_from_slice(&v.to_le_bytes());
    }
    c
}

#[test]
fn features_and_capsets() {
    let mut d = driver3d();
    assert_eq!(d.features & 0xFF_FFFF, F_EDID | F_VIRGL);
    assert_eq!(d.t.cfg(12, 4), 1, "num_capsets");
    let r = ctrl(&mut d, &cmd_ctx(CMD_GET_CAPSET_INFO, 0, &[0, 0]), 40);
    assert_eq!(le32(&r, 0), RESP_OK_CAPSET_INFO);
    assert_eq!((le32(&r, 24), le32(&r, 28), le32(&r, 32)), (5, 2, 6));
    assert_eq!(resp(&mut d, &cmd_ctx(CMD_GET_CAPSET_INFO, 0, &[1, 0])), RESP_ERR_INVALID_PARAMETER);
    let r = ctrl(&mut d, &cmd_ctx(CMD_GET_CAPSET, 0, &[5, 2]), 64);
    assert_eq!(le32(&r, 0), RESP_OK_CAPSET);
    assert_eq!(&r[24..], &[1, 2, 3, 4, 5, 6]);
    assert_eq!(resp(&mut d, &cmd_ctx(CMD_GET_CAPSET, 0, &[5, 3])), RESP_ERR_INVALID_PARAMETER);
    assert_eq!(resp(&mut d, &cmd_ctx(CMD_GET_CAPSET, 0, &[4, 1])), RESP_ERR_INVALID_PARAMETER);

    // With virgl but no renderer: no capsets, 3D commands ERR_UNSPEC.
    let g =
        VirtioGpu::new(Box::new(MemDisplay::default()), GpuConfig { virgl: true, ..GpuConfig::default() });
    let mut d = Driver::new(VirtioMmio::new(Box::new(g)));
    d.init(u64::MAX, 64);
    assert_eq!(d.t.cfg(12, 4), 0);
    assert_eq!(ctx_create(&mut d, 1, "x"), RESP_ERR_UNSPEC);
    assert_eq!(create3d(&mut d, 1, 2, 67, 2, 16, 16), RESP_ERR_UNSPEC);
}

#[test]
fn contexts_resources_and_errors() {
    let mut d = driver3d();
    assert_eq!(ctx_create(&mut d, 3, "surfaceflinger"), RESP_OK_NODATA);
    assert_eq!(ctx_create(&mut d, 3, "again"), RESP_ERR_INVALID_CONTEXT_ID);
    assert_eq!(ctx_create(&mut d, 0, "zero"), RESP_ERR_INVALID_CONTEXT_ID);
    assert_eq!(gpu(&mut d).context_count(), 1);
    // A 3D texture and a 2D resource share the id space.
    assert_eq!(create3d(&mut d, 10, 2, 67, 0x2 | 0x8, 64, 32), RESP_OK_NODATA);
    assert_eq!(create3d(&mut d, 10, 2, 67, 2, 8, 8), RESP_ERR_INVALID_RESOURCE_ID);
    assert_eq!(
        resp(&mut d, &cmd_ctx(CMD_RESOURCE_CREATE_2D, 0, &[10, 1, 4, 4])),
        RESP_ERR_INVALID_RESOURCE_ID
    );
    assert_eq!(gpu(&mut d).hostmem(), 64 * 32 * 4);
    assert_eq!(create3d(&mut d, 11, 2, 67, 2, 1 << 14, 1 << 14), RESP_ERR_OUT_OF_MEMORY);
    assert_eq!(
        log(&mut d),
        [
            "create 3 0 surfaceflinger",
            "res 10 t2 f67 b0xa 64x32",
            "res 11 t2 f67 b0x2 16384x16384",
            "unref 11"
        ]
    );
    assert_eq!(resp(&mut d, &cmd_ctx(CMD_CTX_ATTACH_RESOURCE, 3, &[10, 0])), RESP_OK_NODATA);
    assert_eq!(resp(&mut d, &cmd_ctx(CMD_CTX_ATTACH_RESOURCE, 4, &[10, 0])), RESP_ERR_INVALID_CONTEXT_ID);
    assert_eq!(resp(&mut d, &cmd_ctx(CMD_CTX_ATTACH_RESOURCE, 3, &[99, 0])), RESP_ERR_INVALID_RESOURCE_ID);
    assert_eq!(resp(&mut d, &cmd_ctx(CMD_CTX_DETACH_RESOURCE, 3, &[10, 0])), RESP_OK_NODATA);
    assert_eq!(resp(&mut d, &cmd_ctx(CMD_CTX_DESTROY, 3, &[])), RESP_OK_NODATA);
    assert_eq!(resp(&mut d, &cmd_ctx(CMD_CTX_DESTROY, 3, &[])), RESP_ERR_INVALID_CONTEXT_ID);
    assert_eq!(log(&mut d), ["attach 3 10", "detach 3 10", "destroy 3"]);
    assert_eq!(resp(&mut d, &cmd_ctx(CMD_RESOURCE_UNREF, 0, &[10, 0])), RESP_OK_NODATA);
    assert_eq!(log(&mut d), ["unref 10"]);
    assert_eq!(gpu(&mut d).hostmem(), 0);
    assert_eq!(gpu(&mut d).resource_3d_count(), 0);
}

#[test]
fn transfers_and_submit_reach_the_renderer() {
    let mut d = driver3d();
    assert_eq!(ctx_create(&mut d, 1, "gl"), RESP_OK_NODATA);
    // The 1 MiB pipe buffer of gfxstream's virtio-gpu-pipe transport
    // (PIPE_BUFFER, R8, VIRGL_BIND_CUSTOM), here 4 KiB.
    assert_eq!(create3d(&mut d, 20, 0, 64, 1 << 17, 4096, 1), RESP_OK_NODATA);
    assert_eq!(
        resp(&mut d, &transfer(CMD_TRANSFER_TO_HOST_3D, 1, 20, 0, 8)),
        RESP_ERR_UNSPEC,
        "no backing yet"
    );
    let a = attach(&mut d, 20, 4096);
    d.ram.write(a + 100, b"pipe:opengles\0").unwrap();
    assert_eq!(resp(&mut d, &transfer(CMD_TRANSFER_TO_HOST_3D, 1, 20, 100, 14)), RESP_OK_NODATA);
    assert_eq!(mock(&mut d).written, b"pipe:opengles\0");
    mock(&mut d).reply = vec![9, 8, 7];
    assert_eq!(resp(&mut d, &transfer(CMD_TRANSFER_FROM_HOST_3D, 1, 20, 0, 3)), RESP_OK_NODATA);
    assert_eq!(d.mem(a, 3), [9, 8, 7]);
    assert_eq!(resp(&mut d, &transfer(CMD_TRANSFER_TO_HOST_3D, 1, 21, 0, 3)), RESP_ERR_INVALID_RESOURCE_ID);
    // SUBMIT_3D: size, padding, then the bytes.
    let mut c = cmd_ctx(CMD_SUBMIT_3D, 1, &[5, 0]);
    c.extend_from_slice(&[1, 2, 3, 4, 5]);
    assert_eq!(resp(&mut d, &c), RESP_OK_NODATA);
    assert_eq!(mock(&mut d).submits, [vec![1, 2, 3, 4, 5]]);
    let mut c = cmd_ctx(CMD_SUBMIT_3D, 1, &[50, 0]);
    c.extend_from_slice(&[1, 2]);
    assert_eq!(resp(&mut d, &c), RESP_ERR_INVALID_PARAMETER, "size beyond the command");
    assert_eq!(resp(&mut d, &cmd_ctx(CMD_SUBMIT_3D, 7, &[0, 0])), RESP_ERR_INVALID_CONTEXT_ID);
    assert_eq!(
        log(&mut d),
        [
            "create 1 0 gl",
            "res 20 t0 f64 b0x20000 4096x1",
            "to_host 1 20 x100 w14 off100",
            "from_host 1 20 w3",
            "submit 1 5"
        ]
    );
}

#[test]
fn scanout_of_a_3d_resource() {
    let mut d = driver3d();
    // A render target shown without backing: the renderer presents it.
    assert_eq!(create3d(&mut d, 30, 2, 67, 0x2 | 0x8 | 0x40000, 1280, 800), RESP_OK_NODATA);
    assert_eq!(resp(&mut d, &cmd_ctx(CMD_SET_SCANOUT, 0, &[0, 0, 1280, 800, 0, 30])), RESP_OK_NODATA);
    assert_eq!(gpu(&mut d).scanout_3d(0), Some((30, Rect::new(0, 0, 1280, 800))));
    assert!(gpu(&mut d).frame(0).is_none(), "no pixels in the device");
    assert_eq!(
        resp(&mut d, &cmd_ctx(CMD_SET_SCANOUT, 0, &[0, 0, 1281, 800, 0, 30])),
        RESP_ERR_INVALID_PARAMETER
    );
    assert_eq!(resp(&mut d, &cmd_ctx(CMD_RESOURCE_FLUSH, 0, &[10, 20, 30, 40, 30, 0])), RESP_OK_NODATA);
    assert_eq!(
        resp(&mut d, &cmd_ctx(CMD_RESOURCE_FLUSH, 0, &[0, 0, 1300, 40, 30, 0])),
        RESP_ERR_INVALID_PARAMETER
    );
    assert_eq!(log(&mut d)[1..], ["scanout 0 Some(30)", "flush 0 30 30x40+10+20"]);
    let disp = gpu(&mut d).backend_as::<MemDisplay>().unwrap();
    assert_eq!(disp.updates_3d, 2);
    assert_eq!(disp.last_3d, Some((0, 1280, 800, Rect::new(10, 20, 30, 40))));
    // UNREF of the shown resource turns the scanout off.
    assert_eq!(resp(&mut d, &cmd_ctx(CMD_RESOURCE_UNREF, 0, &[30, 0])), RESP_OK_NODATA);
    assert_eq!(log(&mut d), ["scanout 0 None", "unref 30"]);
    assert_eq!(gpu(&mut d).scanout_3d(0), None);
}

#[test]
fn snapshot_keeps_contexts_resources_and_renderer_state() {
    let mut d = driver3d();
    assert_eq!(ctx_create(&mut d, 2, "a"), RESP_OK_NODATA);
    assert_eq!(create3d(&mut d, 40, 2, 67, 2, 64, 64), RESP_OK_NODATA);
    attach(&mut d, 40, 64 * 64 * 4);
    assert_eq!(resp(&mut d, &cmd_ctx(CMD_SET_SCANOUT, 0, &[0, 0, 64, 64, 0, 40])), RESP_OK_NODATA);
    mock(&mut d).state = 1234;
    let mut w = vetro_snapshot::Writer::new();
    gpu(&mut d).save_state(&mut w);
    let bytes = w.into_bytes();

    let mut other = gpu3d();
    other.restore_state(&mut vetro_snapshot::Reader::new(&bytes)).unwrap();
    assert_eq!(other.context_count(), 1);
    assert_eq!(other.resource_3d_count(), 1);
    assert_eq!(other.hostmem(), 64 * 64 * 4);
    assert_eq!(other.scanout_3d(0), Some((40, Rect::new(0, 0, 64, 64))));
    assert_eq!(other.renderer_as::<Mock>().unwrap().state, 1234);
    assert_eq!(other.backend_as::<MemDisplay>().unwrap().updates_3d, 1, "display told again");

    // A 2D-only device keeps the old format: its snapshot doesn't restore
    // into a 3D one (the configuration differs, the key already says so).
    let plain = VirtioGpu::new(Box::new(MemDisplay::default()), GpuConfig::default());
    let mut w = vetro_snapshot::Writer::new();
    plain.save_state(&mut w);
    let bytes = w.into_bytes();
    let mut again = VirtioGpu::new(Box::new(MemDisplay::default()), GpuConfig::default());
    again.restore_state(&mut vetro_snapshot::Reader::new(&bytes)).unwrap();
}

#[test]
fn without_virgl_3d_commands_do_not_exist() {
    let mut d = Driver::new(VirtioMmio::new(Box::new(VirtioGpu::new(
        Box::new(MemDisplay::default()),
        GpuConfig::default(),
    ))));
    d.init(u64::MAX, 64);
    gpu(&mut d).set_renderer(Box::new(Mock::default()));
    assert_eq!(d.features & F_VIRGL, 0);
    assert_eq!(d.t.cfg(12, 4), 0, "no capsets without virgl");
    assert_eq!(ctx_create(&mut d, 1, "x"), RESP_ERR_UNSPEC);
    assert_eq!(create3d(&mut d, 1, 2, 67, 2, 16, 16), RESP_ERR_UNSPEC);
    assert!(log(&mut d).is_empty());
}
