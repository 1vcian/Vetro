use super::*;
use crate::virtio::testdrv::{Driver, Transport};

fn driver_with(cfg: GpuConfig) -> Driver<VirtioMmio> {
    let mut d = Driver::new(VirtioMmio::new(Box::new(VirtioGpu::new(Box::new(MemDisplay::default()), cfg))));
    d.init(u64::MAX, 64);
    d
}

fn driver() -> Driver<VirtioMmio> {
    driver_with(GpuConfig::default())
}

fn gpu(d: &mut Driver<VirtioMmio>) -> &mut VirtioGpu {
    d.t.device_as_mut::<VirtioGpu>().unwrap()
}

fn display(d: &mut Driver<VirtioMmio>) -> &mut MemDisplay {
    gpu(d).backend_as_mut::<MemDisplay>().unwrap()
}

fn hdr(ty: u32) -> Vec<u8> {
    let mut h = vec![0u8; HDR_LEN];
    h[0..4].copy_from_slice(&ty.to_le_bytes());
    h
}

fn cmd(ty: u32, words: &[u32]) -> Vec<u8> {
    let mut c = hdr(ty);
    for w in words {
        c.extend_from_slice(&w.to_le_bytes());
    }
    c
}

/// Manda un comando sulla coda di controllo e restituisce la risposta.
fn ctrl(d: &mut Driver<VirtioMmio>, c: &[u8], resp_len: u32) -> Vec<u8> {
    let a = d.buf(c);
    let r = d.alloc(u64::from(resp_len), 8);
    let head = d.add(CTRLQ, &[(a, c.len() as u32, false), (r, resp_len, true)]);
    d.service();
    assert_eq!(d.irq() & INT_VRING, INT_VRING);
    let (h, len) = d.pop_used(CTRLQ).expect("risposta");
    assert_eq!(h, head);
    d.mem(r, len as usize)
}

fn resp_type(r: &[u8]) -> u32 {
    le32(r, 0)
}

fn ok(d: &mut Driver<VirtioMmio>, c: &[u8]) {
    let r = ctrl(d, c, 24);
    assert_eq!(resp_type(&r), RESP_OK_NODATA, "comando {:#x}", le32(c, 0));
}

fn err(d: &mut Driver<VirtioMmio>, c: &[u8]) -> u32 {
    resp_type(&ctrl(d, c, 24))
}

fn create(d: &mut Driver<VirtioMmio>, id: u32, fmt: PixelFormat, w: u32, h: u32) {
    ok(d, &cmd(CMD_RESOURCE_CREATE_2D, &[id, fmt as u32, w, h]));
}

/// Backing in `pieces` pezzi (non contigui) per `len` byte; restituisce gli
/// indirizzi dei pezzi.
fn attach(d: &mut Driver<VirtioMmio>, id: u32, len: u64, pieces: u64) -> Vec<(u64, u64)> {
    let piece = len / pieces;
    let mut ents = Vec::new();
    let mut c = cmd(CMD_RESOURCE_ATTACH_BACKING, &[id, pieces as u32]);
    for k in 0..pieces {
        let l = if k + 1 == pieces { len - piece * k } else { piece };
        let a = d.alloc(l + 64, 64); // buchi fra i pezzi
        c.extend_from_slice(&a.to_le_bytes());
        c.extend_from_slice(&(l as u32).to_le_bytes());
        c.extend_from_slice(&0u32.to_le_bytes());
        ents.push((a, l));
    }
    ok(d, &c);
    ents
}

/// Scrive `data` nel backing visto come spazio contiguo.
fn fill(d: &mut Driver<VirtioMmio>, ents: &[(u64, u64)], data: &[u8]) {
    let mut off = 0usize;
    for &(a, l) in ents {
        let n = (l as usize).min(data.len() - off);
        d.ram.write(a, &data[off..off + n]).unwrap();
        off += n;
    }
}

fn pattern(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::new();
    for y in 0..h {
        for x in 0..w {
            v.extend_from_slice(&[x as u8, y as u8, (x ^ y) as u8, 0x5a]); // B G R X
        }
    }
    v
}

fn rect(x: u32, y: u32, w: u32, h: u32) -> [u32; 4] {
    [x, y, w, h]
}

#[test]
fn configurazione_display_info_ed_edid() {
    let mut d = driver();
    assert_eq!(d.t.rd(DEVICE_ID), ID_GPU);
    assert_eq!(d.features & 0xFF_FFFF, F_EDID);
    assert_eq!(d.t.cfg(8, 4), 1, "num_scanouts");
    assert_eq!(d.t.cfg(12, 4), 0, "num_capsets");
    d.t.wr(QUEUE_SEL, 0);
    assert_eq!(d.t.rd(QUEUE_NUM_MAX), 64);
    d.t.wr(QUEUE_SEL, 1);
    assert_eq!(d.t.rd(QUEUE_NUM_MAX), 16);

    let r = ctrl(&mut d, &hdr(CMD_GET_DISPLAY_INFO), 408);
    assert_eq!(r.len(), 408);
    assert_eq!(resp_type(&r), RESP_OK_DISPLAY_INFO);
    // pmodes[0]: rect 0,0,1280,800, enabled 1, flags 0; gli altri a zero.
    assert_eq!(&r[24..48], &[0, 0, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0x20, 3, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]);
    assert!(r[48..].iter().all(|&b| b == 0));

    let r = ctrl(&mut d, &cmd(CMD_GET_EDID, &[0, 0]), 1056);
    assert_eq!(resp_type(&r), RESP_OK_EDID);
    assert_eq!(le32(&r, 24), 1024);
    assert_eq!(&r[32..], &edid::generate(&EdidInfo::default(), 1024)[..]);
    assert_eq!(err(&mut d, &cmd(CMD_GET_EDID, &[1, 0])), RESP_ERR_INVALID_PARAMETER);

    // Senza EDID la feature non c'è e il comando non esiste.
    let mut d = driver_with(GpuConfig { edid: false, ..GpuConfig::default() });
    assert_eq!(d.features & 0xFF_FFFF, 0);
    assert_eq!(err(&mut d, &cmd(CMD_GET_EDID, &[0, 0])), RESP_ERR_UNSPEC);
}

#[test]
fn risorsa_backing_transfer_scanout_e_flush() {
    let mut d = driver();
    let (w, h) = (64u32, 32u32);
    create(&mut d, 7, PixelFormat::B8G8R8X8, w, h);
    assert_eq!(gpu(&mut d).hostmem(), u64::from(w * h * 4));
    let ents = attach(&mut d, 7, u64::from(w * h * 4), 3);
    let px = pattern(w, h);
    fill(&mut d, &ents, &px);
    ok(&mut d, &cmd(CMD_TRANSFER_TO_HOST_2D, &[0, 0, w, h, 0, 0, 7, 0]));
    assert!(display(&mut d).screens.is_empty(), "niente scanout: niente immagine");
    // Scanout sulla parte (8, 4)+32x16 della risorsa.
    ok(&mut d, &cmd(CMD_SET_SCANOUT, &[8, 4, 32, 16, 0, 7]));
    let disp = display(&mut d);
    assert_eq!(disp.updates, 1);
    assert_eq!(disp.screens[&0].0, 32);
    // Pixel (0, 0) dello scanout = (8, 4) della risorsa: B=8 G=4 R=12, X->255.
    assert_eq!(disp.pixel(0, 0, 0), Some([12, 4, 8, 255]));
    assert_eq!(disp.pixel(0, 31, 15), Some([(39 ^ 19) as u8, 19, 39, 255]));
    let f = gpu(&mut d).frame(0).unwrap();
    assert_eq!((f.width, f.height, f.stride), (32, 16, 256));
    assert_eq!(f.rgba(1, 1), [(9 ^ 5) as u8, 5, 9, 255]);

    // Il guest ridisegna un rettangolo: TRANSFER parziale (riga per riga,
    // dall'offset del primo pixel) e FLUSH.
    let mut px2 = px.clone();
    for y in 6..10u32 {
        for x in 10..20u32 {
            let o = ((y * w + x) * 4) as usize;
            px2[o..o + 4].copy_from_slice(&[1, 2, 3, 0]);
        }
    }
    fill(&mut d, &ents, &px2);
    let off = (6 * w + 10) * 4;
    ok(&mut d, &cmd(CMD_TRANSFER_TO_HOST_2D, &[10, 6, 10, 4, off, 0, 7, 0]));
    assert_eq!(display(&mut d).pixel(0, 2, 2), Some([(10 ^ 6) as u8, 6, 10, 255]), "prima del flush");
    // Il flush fuori dallo scanout non aggiorna nulla.
    ok(&mut d, &cmd(CMD_RESOURCE_FLUSH, &[40, 20, 8, 8, 7, 0]));
    assert_eq!(display(&mut d).updates, 1);
    ok(&mut d, &cmd(CMD_RESOURCE_FLUSH, &[0, 0, w, h, 7, 0]));
    let disp = display(&mut d);
    assert_eq!(disp.updates, 2);
    assert_eq!(disp.pixel(0, 2, 2), Some([3, 2, 1, 255]));
    assert_eq!(disp.pixel(0, 1, 1), Some([(9 ^ 5) as u8, 5, 9, 255]));

    // DETACH, poi TRANSFER senza backing: ERR_UNSPEC.
    ok(&mut d, &cmd(CMD_RESOURCE_DETACH_BACKING, &[7, 0]));
    assert_eq!(err(&mut d, &cmd(CMD_TRANSFER_TO_HOST_2D, &[0, 0, w, h, 0, 0, 7, 0])), RESP_ERR_UNSPEC);
    assert_eq!(err(&mut d, &cmd(CMD_RESOURCE_DETACH_BACKING, &[7, 0])), RESP_ERR_UNSPEC);
    // UNREF di una risorsa mostrata: lo scanout si spegne.
    ok(&mut d, &cmd(CMD_RESOURCE_UNREF, &[7, 0]));
    assert!(display(&mut d).screens.is_empty());
    assert!(gpu(&mut d).frame(0).is_none());
    assert_eq!(gpu(&mut d).hostmem(), 0);
}

#[test]
fn formati_e_backing_corto() {
    let mut d = driver();
    let fmts = [
        (PixelFormat::B8G8R8A8, [3, 2, 1, 4]),
        (PixelFormat::A8R8G8B8, [2, 3, 4, 1]),
        (PixelFormat::X8R8G8B8, [2, 3, 4, 255]),
        (PixelFormat::R8G8B8A8, [1, 2, 3, 4]),
        (PixelFormat::X8B8G8R8, [4, 3, 2, 255]),
        (PixelFormat::A8B8G8R8, [4, 3, 2, 1]),
        (PixelFormat::R8G8B8X8, [1, 2, 3, 255]),
    ];
    for (i, (f, rgba)) in fmts.into_iter().enumerate() {
        let id = 10 + i as u32;
        create(&mut d, id, f, 16, 16);
        // Backing di una sola riga: il resto resta a zero (come iov_to_buf).
        let ents = attach(&mut d, id, 64, 1);
        fill(&mut d, &ents, &[1, 2, 3, 4].repeat(16));
        ok(&mut d, &cmd(CMD_TRANSFER_TO_HOST_2D, &[0, 0, 16, 16, 0, 0, id, 0]));
        ok(&mut d, &cmd(CMD_SET_SCANOUT, &[0, 0, 16, 16, 0, id]));
        let disp = display(&mut d);
        assert_eq!(disp.pixel(0, 15, 0), Some(rgba), "{f:?}");
        assert_eq!(disp.pixel(0, 0, 1).unwrap()[..3], [0, 0, 0], "{f:?}");
    }
}

#[test]
fn errori_come_qemu() {
    let mut d = driver();
    let e = |d: &mut Driver<VirtioMmio>, ty, w: &[u32]| err(d, &cmd(ty, w));
    assert_eq!(e(&mut d, CMD_RESOURCE_CREATE_2D, &[0, 1, 4, 4]), RESP_ERR_INVALID_RESOURCE_ID);
    assert_eq!(e(&mut d, CMD_RESOURCE_CREATE_2D, &[1, 5, 4, 4]), RESP_ERR_INVALID_PARAMETER);
    assert_eq!(e(&mut d, CMD_RESOURCE_CREATE_2D, &[1, 1, 1 << 14, 1 << 14]), RESP_ERR_OUT_OF_MEMORY);
    create(&mut d, 1, PixelFormat::B8G8R8A8, 32, 32);
    assert_eq!(e(&mut d, CMD_RESOURCE_CREATE_2D, &[1, 1, 4, 4]), RESP_ERR_INVALID_RESOURCE_ID);
    assert_eq!(e(&mut d, CMD_RESOURCE_UNREF, &[9, 0]), RESP_ERR_INVALID_RESOURCE_ID);
    assert_eq!(e(&mut d, CMD_RESOURCE_FLUSH, &[0, 0, 4, 4, 9, 0]), RESP_ERR_INVALID_RESOURCE_ID);
    assert_eq!(e(&mut d, CMD_RESOURCE_FLUSH, &[30, 0, 4, 4, 1, 0]), RESP_ERR_INVALID_PARAMETER);
    assert_eq!(e(&mut d, CMD_RESOURCE_ATTACH_BACKING, &[9, 0]), RESP_ERR_INVALID_RESOURCE_ID);
    // Senza backing: TRANSFER e SET_SCANOUT falliscono con ERR_UNSPEC.
    assert_eq!(e(&mut d, CMD_TRANSFER_TO_HOST_2D, &[0, 0, 4, 4, 0, 0, 1, 0]), RESP_ERR_UNSPEC);
    assert_eq!(e(&mut d, CMD_SET_SCANOUT, &[0, 0, 32, 32, 0, 1]), RESP_ERR_UNSPEC);
    // Backing fuori dalla RAM, troppe voci, voci mancanti.
    let mut c = cmd(CMD_RESOURCE_ATTACH_BACKING, &[1, 1]);
    c.extend_from_slice(&0x1000u64.to_le_bytes());
    c.extend_from_slice(&[0, 16, 0, 0, 0, 0, 0, 0]);
    assert_eq!(err(&mut d, &c), RESP_ERR_UNSPEC);
    assert_eq!(e(&mut d, CMD_RESOURCE_ATTACH_BACKING, &[1, 16385]), RESP_ERR_UNSPEC);
    assert_eq!(e(&mut d, CMD_RESOURCE_ATTACH_BACKING, &[1, 2]), RESP_ERR_UNSPEC);
    attach(&mut d, 1, 32 * 32 * 4, 1);
    assert_eq!(e(&mut d, CMD_RESOURCE_ATTACH_BACKING, &[1, 0]), RESP_ERR_UNSPEC, "già attaccato");
    // Rettangoli.
    for r in [rect(1, 0, 32, 4), rect(0, 29, 4, 4), rect(33, 0, 0, 0), rect(0, 0, 33, 1)] {
        let mut w = r.to_vec();
        w.extend([0, 0, 1, 0]);
        assert_eq!(e(&mut d, CMD_TRANSFER_TO_HOST_2D, &w), RESP_ERR_INVALID_PARAMETER, "{r:?}");
    }
    assert_eq!(e(&mut d, CMD_SET_SCANOUT, &[0, 0, 15, 32, 0, 1]), RESP_ERR_INVALID_PARAMETER, "< 16");
    assert_eq!(e(&mut d, CMD_SET_SCANOUT, &[17, 0, 16, 16, 0, 1]), RESP_ERR_INVALID_PARAMETER);
    assert_eq!(e(&mut d, CMD_SET_SCANOUT, &[0, 0, 16, 16, 1, 1]), RESP_ERR_INVALID_SCANOUT_ID);
    assert_eq!(e(&mut d, CMD_SET_SCANOUT, &[0, 0, 16, 16, 0, 9]), RESP_ERR_INVALID_RESOURCE_ID);
    // Comandi non 2D.
    assert_eq!(e(&mut d, CMD_GET_CAPSET_INFO, &[0, 0]), RESP_ERR_UNSPEC);
    assert_eq!(e(&mut d, CMD_GET_CAPSET, &[0, 0, 0, 0]), RESP_ERR_UNSPEC);
    assert_eq!(e(&mut d, 0x0200, &[0; 20]), RESP_ERR_UNSPEC, "CTX_CREATE");
    assert_eq!(e(&mut d, CMD_RESOURCE_ASSIGN_UUID, &[1, 0]), RESP_ERR_UNSPEC);
    assert_eq!(e(&mut d, CMD_RESOURCE_CREATE_BLOB, &[0; 8]), RESP_ERR_INVALID_PARAMETER);
    assert_eq!(e(&mut d, 0x1234, &[]), RESP_ERR_UNSPEC);
    // Comando corto.
    assert_eq!(e(&mut d, CMD_RESOURCE_CREATE_2D, &[2, 1]), RESP_ERR_INVALID_PARAMETER);
    assert_eq!(resp_type(&ctrl(&mut d, &[1, 1, 0, 0], 24)), RESP_ERR_INVALID_PARAMETER);
    assert!(d.t.last_error().is_none(), "nessun errore della coda");
}

#[test]
fn fence_e_risposta_troncata() {
    let mut d = driver();
    let mut c = cmd(CMD_RESOURCE_CREATE_2D, &[3, 1, 16, 16]);
    c[4..8].copy_from_slice(&FLAG_FENCE.to_le_bytes());
    c[8..16].copy_from_slice(&0x1122_3344_5566u64.to_le_bytes());
    c[16..20].copy_from_slice(&9u32.to_le_bytes());
    let r = ctrl(&mut d, &c, 24);
    assert_eq!(resp_type(&r), RESP_OK_NODATA);
    assert_eq!(le32(&r, 4), FLAG_FENCE);
    assert_eq!(le64(&r, 8), 0x1122_3344_5566);
    assert_eq!(le32(&r, 16), 9);
    // Una risposta più lunga del buffer si tronca (come QEMU).
    let r = ctrl(&mut d, &hdr(CMD_GET_DISPLAY_INFO), 100);
    assert_eq!(r.len(), 100);
    assert_eq!(resp_type(&r), RESP_OK_DISPLAY_INFO);
}

#[test]
fn set_scanout_0_e_cambio_di_risorsa() {
    let mut d = driver();
    for id in [1, 2] {
        create(&mut d, id, PixelFormat::B8G8R8X8, 16, 16);
        attach(&mut d, id, 1024, 1);
    }
    ok(&mut d, &cmd(CMD_SET_SCANOUT, &[0, 0, 16, 16, 0, 1]));
    ok(&mut d, &cmd(CMD_SET_SCANOUT, &[0, 0, 16, 16, 0, 2]));
    // La risorsa 1 non è più mostrata: il suo UNREF non tocca lo scanout.
    ok(&mut d, &cmd(CMD_RESOURCE_UNREF, &[1, 0]));
    assert!(display(&mut d).screens.contains_key(&0));
    ok(&mut d, &cmd(CMD_SET_SCANOUT, &[0, 0, 0, 0, 0, 0]));
    assert!(display(&mut d).screens.is_empty());
    assert!(gpu(&mut d).frame(0).is_none());
}

#[test]
fn cursore() {
    let mut d = driver();
    create(&mut d, 5, PixelFormat::B8G8R8A8, 64, 64);
    let ents = attach(&mut d, 5, 64 * 64 * 4, 2);
    let img: Vec<u8> = (0..64 * 64 * 4).map(|i| i as u8).collect();
    fill(&mut d, &ents, &img);
    ok(&mut d, &cmd(CMD_TRANSFER_TO_HOST_2D, &[0, 0, 64, 64, 0, 0, 5, 0]));
    let upd = cmd(CMD_UPDATE_CURSOR, &[0, 100, 50, 0, 5, 3, 4, 0]);
    let a = d.buf(&upd);
    let head = d.add(CURSORQ, &[(a, 56, false)]);
    d.service();
    assert_eq!(d.pop_used(CURSORQ), Some((head, 0)));
    let c = display(&mut d).cursors[&0].clone();
    assert_eq!((c.resource_id, c.x, c.y, c.hot_x, c.hot_y), (5, 100, 50, 3, 4));
    assert_eq!(c.image, img);
    let mv = cmd(CMD_MOVE_CURSOR, &[0, 7, 8, 0, 0, 0, 0, 0]);
    let a = d.buf(&mv);
    d.add(CURSORQ, &[(a, 56, false)]);
    // Scanout inesistente e comando corto: ignorati, buffer restituito.
    let bad = d.buf(&cmd(CMD_MOVE_CURSOR, &[3, 1, 1, 0, 0, 0, 0, 0]));
    d.add(CURSORQ, &[(bad, 56, false)]);
    d.add(CURSORQ, &[(bad, 20, false)]);
    d.service();
    let c = gpu(&mut d).cursor(0).unwrap().clone();
    assert_eq!((c.resource_id, c.x, c.y, c.hot_x), (5, 7, 8, 3), "MOVE cambia solo la posizione");
    assert_eq!(c.image, img);
    let mut n = 0;
    while d.pop_used(CURSORQ).is_some() {
        n += 1;
    }
    assert_eq!(n, 3);
}

#[test]
fn cambio_di_risoluzione_dall_host() {
    let mut d = driver();
    gpu(&mut d).set_display(0, 1080, 1920);
    d.service();
    assert_eq!(d.irq(), INT_CONFIG);
    assert_eq!(d.t.cfg(0, 4), u64::from(EVENT_DISPLAY), "events_read");
    let r = ctrl(&mut d, &hdr(CMD_GET_DISPLAY_INFO), 408);
    assert_eq!((le32(&r, 32), le32(&r, 36), le32(&r, 40)), (1080, 1920, 1));
    let r = ctrl(&mut d, &cmd(CMD_GET_EDID, &[0, 0]), 1056);
    let info = EdidInfo { prefx: 1080, prefy: 1920, ..EdidInfo::default() };
    assert_eq!(&r[32..], &edid::generate(&info, 1024)[..]);
    d.t.cfg_wr(4, 4, u64::from(EVENT_DISPLAY));
    assert_eq!(d.t.cfg(0, 4), 0, "events_clear");
    // Display spento: enabled = 0.
    gpu(&mut d).set_display(0, 0, 0);
    let r = ctrl(&mut d, &hdr(CMD_GET_DISPLAY_INFO), 408);
    assert!(r[24..48].iter().all(|&b| b == 0));
}

#[test]
fn reset_libera_le_risorse() {
    let mut d = driver();
    create(&mut d, 1, PixelFormat::B8G8R8X8, 16, 16);
    attach(&mut d, 1, 1024, 1);
    ok(&mut d, &cmd(CMD_SET_SCANOUT, &[0, 0, 16, 16, 0, 1]));
    d.init(u64::MAX, 64);
    assert_eq!(gpu(&mut d).resource_count(), 0);
    assert_eq!(gpu(&mut d).hostmem(), 0);
    assert!(display(&mut d).screens.is_empty());
    create(&mut d, 1, PixelFormat::B8G8R8X8, 16, 16);
}
