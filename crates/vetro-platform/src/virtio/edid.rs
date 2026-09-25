//! EDID del monitor virtuale di virtio-gpu (VIRTIO_GPU_CMD_GET_EDID).
//!
//! Stesso contenuto del generatore di QEMU (hw/display/edid-generate.c),
//! così il guest vede lo stesso monitor e la stessa lista di modi: EDID 1.4
//! con un blocco base, un'estensione CTA-861 (DTA) e, per schermi oltre
//! 4096 punti o con clock oltre 655 MHz, un'estensione DisplayID 1.3 con il
//! modo preferito. Il blocco base contiene:
//! - produttore "RHT", modello 0x1234, settimana 42 del 2014 (come QEMU);
//! - ingresso digitale a 8 bit per canale (DisplayPort), gamma 2,2, sRGB;
//! - modi standard e stabiliti dalla tabella di QEMU (fino a `max`), più
//!   un descrittore "standard timings 3" e i modi CTA a 50/60 Hz;
//! - il modo preferito come descrittore dettagliato, con tempi inventati
//!   in proporzione alla risoluzione (margini 25%/3%/35% in orizzontale,
//!   0,5%/0,5%/3,5% in verticale) e refresh di default 75 Hz;
//! - limiti 50-125 Hz, 30-160 kHz, 2550 MHz e il nome del monitor.
//!
//! La verifica è il test `come_qemu`: i 256 byte letti dal guest sotto QEMU
//! (`/sys/class/drm/card0-Virtual-1/edid`) per 1280x800.

/// Parametri del monitor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdidInfo {
    /// Codice PNP del produttore (tre lettere maiuscole).
    pub vendor: [u8; 3],
    /// Nome del monitor (al più 12 caratteri nel descrittore).
    pub name: String,
    /// Numero di serie (testo; il campo numerico è il suo valore decimale).
    pub serial: Option<String>,
    /// Dimensioni fisiche; 0 = derivate da 100 DPI.
    pub width_mm: u16,
    pub height_mm: u16,
    /// Modo preferito.
    pub prefx: u32,
    pub prefy: u32,
    /// Risoluzione massima dei modi elencati (0 = nessun limite).
    pub maxx: u32,
    pub maxy: u32,
    /// Refresh del modo preferito in mHz (0 = 75000).
    pub refresh_mhz: u32,
}

impl Default for EdidInfo {
    fn default() -> Self {
        Self {
            vendor: *b"RHT",
            name: "QEMU Monitor".into(),
            serial: None,
            width_mm: 0,
            height_mm: 0,
            prefx: 1280,
            prefy: 800,
            maxx: 0,
            maxy: 0,
            refresh_mhz: 0,
        }
    }
}

/// Un modo della tabella: dove si dichiara (byte e bit dei modi stabiliti,
/// bit del descrittore "standard timings 3", codice CTA).
struct Mode {
    x: u32,
    y: u32,
    established: Option<(usize, u8)>,
    xtra3: Option<(usize, u8)>,
    cta: u8,
}

const fn m(x: u32, y: u32, established: Option<(usize, u8)>, xtra3: Option<(usize, u8)>, cta: u8) -> Mode {
    Mode { x, y, established, xtra3, cta }
}

/// La tabella dei modi, nell'ordine in cui QEMU li dichiara.
const MODES: &[Mode] = &[
    // Modi CTA a 50 Hz.
    m(5120, 2160, None, None, 125),
    m(4096, 2160, None, None, 101),
    m(3840, 2160, None, None, 96),
    m(2560, 1080, None, None, 89),
    m(2048, 1152, None, None, 0),
    m(1920, 1080, None, None, 31),
    // Modi CTA a 60 Hz.
    m(3840, 2160, None, None, 97),
    // Standard timings 3 (60 Hz).
    m(1920, 1200, None, Some((10, 0)), 0),
    m(1600, 1200, None, Some((9, 2)), 0),
    m(1680, 1050, None, Some((9, 5)), 0),
    m(1440, 900, None, Some((8, 5)), 0),
    m(1280, 1024, None, Some((7, 1)), 0),
    m(1280, 960, None, Some((7, 3)), 0),
    m(1280, 768, None, Some((7, 6)), 0),
    m(1920, 1440, None, Some((11, 5)), 0),
    m(1856, 1392, None, Some((10, 3)), 0),
    m(1792, 1344, None, Some((10, 5)), 0),
    m(1440, 1050, None, Some((8, 1)), 0),
    m(1360, 768, None, Some((8, 7)), 0),
    // Modi stabiliti (60 Hz).
    m(1024, 768, Some((36, 3)), None, 0),
    m(800, 600, Some((35, 0)), None, 0),
    m(640, 480, Some((35, 5)), None, 0),
];

struct Timings {
    xfront: u32,
    xsync: u32,
    xblank: u32,
    yfront: u32,
    ysync: u32,
    yblank: u32,
    /// In unità da 10 kHz.
    clock: u64,
}

fn timings(refresh_mhz: u32, x: u32, y: u32) -> Timings {
    let (xblank, yblank) = (x * 35 / 100, y * 35 / 1000);
    Timings {
        xfront: x * 25 / 100,
        xsync: x * 3 / 100,
        xblank,
        yfront: y * 5 / 1000,
        ysync: y * 5 / 1000,
        yblank,
        clock: u64::from(refresh_mhz) * u64::from(x + xblank) * u64::from(y + yblank) / 10_000_000,
    }
}

/// Somma di controllo: `b[len]` rende nulla la somma di `b[..=len]`.
fn checksum(b: &mut [u8], len: usize) {
    let sum = b[..len].iter().fold(0u8, |a, &v| a.wrapping_add(v));
    if sum != 0 {
        b[len] = 0u8.wrapping_sub(sum);
    }
}

/// Modo standard di 2 byte; `None` se il rapporto non è 16:10, 4:3, 5:4,
/// 16:9 o la larghezza non si rappresenta.
fn std_mode(x: u32, y: u32) -> Option<[u8; 2]> {
    let aspect = if x * 10 == y * 16 {
        0
    } else if x * 3 == y * 4 {
        1
    } else if x * 4 == y * 5 {
        2
    } else if x * 9 == y * 16 {
        3
    } else {
        return None;
    };
    let code = (x / 8).wrapping_sub(31);
    if code > 255 {
        return None;
    }
    Some([code as u8, aspect << 6])
}

fn desc_type(d: &mut [u8], ty: u8) {
    d[..5].copy_from_slice(&[0, 0, 0, ty, 0]);
}

fn desc_text(d: &mut [u8], ty: u8, text: &str) {
    desc_type(d, ty);
    d[5..18].fill(b' ');
    let t = &text.as_bytes()[..text.len().min(12)];
    d[5..5 + t.len()].copy_from_slice(t);
    d[5 + t.len()] = b'\n';
}

fn desc_timing(d: &mut [u8], t: &Timings, x: u32, y: u32, xmm: u32, ymm: u32) {
    d[0..2].copy_from_slice(&(t.clock as u16).to_le_bytes());
    d[2] = x as u8;
    d[3] = t.xblank as u8;
    d[4] = (((x & 0xf00) >> 4) | ((t.xblank & 0xf00) >> 8)) as u8;
    d[5] = y as u8;
    d[6] = t.yblank as u8;
    d[7] = (((y & 0xf00) >> 4) | ((t.yblank & 0xf00) >> 8)) as u8;
    d[8] = t.xfront as u8;
    d[9] = t.xsync as u8;
    d[10] = (((t.yfront & 0xf) << 4) | (t.ysync & 0xf)) as u8;
    d[11] = (((t.xfront & 0x300) >> 2)
        | ((t.xsync & 0x300) >> 4)
        | ((t.yfront & 0x30) >> 2)
        | ((t.ysync & 0x30) >> 4)) as u8;
    d[12] = xmm as u8;
    d[13] = ymm as u8;
    d[14] = (((xmm & 0xf00) >> 4) | ((ymm & 0xf00) >> 8)) as u8;
    d[17] = 0x18;
}

/// Coordinata di cromaticità a 10 bit (come il C di QEMU: float per 1024,
/// più 0,5 in double, troncato).
fn to_10bit(v: f32) -> u32 {
    (f64::from(v * 1024.0) + 0.5) as u32
}

fn colorspace(e: &mut [u8], c: [f32; 8]) {
    let [rx, ry, gx, gy, bx, by, wx, wy] = c.map(to_10bit);
    e[25] = (((rx & 3) << 6) | ((ry & 3) << 4) | ((gx & 3) << 2) | (gy & 3)) as u8;
    e[26] = (((bx & 3) << 6) | ((by & 3) << 4) | ((wx & 3) << 2) | (wy & 3)) as u8;
    for (i, v) in [rx, ry, gx, gy, bx, by, wx, wy].into_iter().enumerate() {
        e[27 + i] = (v >> 2) as u8;
    }
}

/// Prossimo descrittore da 18 byte dopo quello a `desc` (offset nel
/// buffer): i quattro del blocco base, poi quelli dell'estensione CTA dopo
/// i suoi blocchi di dati.
fn desc_next(e: &[u8], has_dta: bool, desc: Option<usize>) -> Option<usize> {
    let d = desc?;
    if d + 36 < 127 {
        return Some(d + 18);
    }
    if has_dta {
        if d < 127 {
            return Some(128 + usize::from(e[128 + 2]));
        }
        if d + 36 < 128 + 127 {
            return Some(d + 18);
        }
    }
    None
}

/// Genera l'EDID in un buffer di `size` byte (1024 per virtio-gpu): 128
/// byte, 256 con l'estensione CTA (size >= 256), 384 con DisplayID.
pub fn generate(info: &EdidInfo, size: usize) -> Vec<u8> {
    let mut e = vec![0u8; size.max(128)];
    let refresh = if info.refresh_mhz == 0 { 75_000 } else { info.refresh_mhz };
    let (px, py) =
        (if info.prefx == 0 { 1280 } else { info.prefx }, if info.prefy == 0 { 800 } else { info.prefy });
    let dpi_to_mm = |res: u32| res * 254 / 10 / 100;
    let (wmm, hmm) = if info.width_mm != 0 && info.height_mm != 0 {
        (u32::from(info.width_mm), u32::from(info.height_mm))
    } else {
        (dpi_to_mm(px), dpi_to_mm(py))
    };
    let t = timings(refresh, px, py);
    let large = px >= 4096 || py >= 4096 || t.clock >= 65536;

    let has_dta = size >= 256;
    if has_dta {
        e[126] += 1;
        e[128..133].copy_from_slice(&[0x02, 0x03, 0x05, 0x00, 0x40]);
    }
    let has_did = size >= 384 && large;
    if has_did {
        e[126] += 1;
        e[256..260].copy_from_slice(&[0x70, 0x13, 4, 0x03]);
        checksum(&mut e[257..], 8);
    }

    e[0..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
    let v = info.vendor.map(|c| u16::from(c.wrapping_sub(b'@') & 0x1f));
    e[8..10].copy_from_slice(&((v[0] << 10) | (v[1] << 5) | v[2]).to_be_bytes());
    e[10..12].copy_from_slice(&0x1234u16.to_le_bytes());
    let serial = info.serial.as_deref().map_or(0, atoi);
    e[12..16].copy_from_slice(&serial.to_le_bytes());
    e[16] = 42;
    e[17] = (2014 - 1990) as u8;
    e[18] = 1;
    e[19] = 4;
    e[20] = 0xa5;
    e[21] = (wmm / 10) as u8;
    e[22] = (hmm / 10) as u8;
    e[23] = 220 - 100;
    e[24] = 0x06;
    colorspace(&mut e, [0.6400, 0.3300, 0.3000, 0.6000, 0.1500, 0.0600, 0.3127, 0.3290]);

    let mut desc = Some(54);
    if !large {
        desc_timing(&mut e[54..72], &t, px, py, wmm, hmm);
        desc = desc_next(&e, has_dta, desc);
    }
    let xtra3 = desc.expect("c'è sempre posto per il secondo descrittore");
    desc_type(&mut e[xtra3..], 0xf7);
    e[xtra3 + 5] = 10;
    desc = desc_next(&e, has_dta, desc);

    // Modi: stabiliti, standard (8 posti da 38 a 53), standard timings 3, CTA.
    let mut std = 38;
    for mode in MODES {
        if (info.maxx != 0 && mode.x > info.maxx) || (info.maxy != 0 && mode.y > info.maxy) {
            continue;
        }
        if let Some((byte, bit)) = mode.established {
            e[byte] |= 1 << bit;
        } else if std < 54 {
            if let Some(s) = std_mode(mode.x, mode.y) {
                e[std..std + 2].copy_from_slice(&s);
                std += 2;
            }
        } else if let Some((byte, bit)) = mode.xtra3 {
            e[xtra3 + byte] |= 1 << bit;
        }
        if has_dta && mode.cta != 0 {
            let at = 128 + usize::from(e[130]);
            e[at] = mode.cta;
            e[130] += 1;
            e[132] += 1;
        }
    }
    while std < 54 {
        e[std..std + 2].copy_from_slice(&[0x01, 0x01]);
        std += 2;
    }

    // Limiti del monitor.
    if let Some(d) = desc {
        desc_type(&mut e[d..], 0xfd);
        e[d + 5..d + 12].copy_from_slice(&[50, 125, 30, 160, (2550 / 10) as u8, 0x01, b'\n']);
        e[d + 12..d + 18].fill(b' ');
    }
    desc = desc_next(&e, has_dta, desc);
    if let Some(d) = desc {
        desc_text(&mut e[d..d + 18], 0xfc, &info.name);
        desc = desc_next(&e, has_dta, desc);
    }
    if let (Some(d), Some(s)) = (desc, &info.serial) {
        desc_text(&mut e[d..d + 18], 0xff, s);
        desc = desc_next(&e, has_dta, desc);
    }
    while let Some(d) = desc {
        desc_type(&mut e[d..], 0x10);
        desc = desc_next(&e, has_dta, desc);
    }

    if has_did {
        let d = &mut e[256..384];
        d[0..4].copy_from_slice(&[0x70, 0x13, 23, 0x03]);
        d[5..8].copy_from_slice(&[0x03, 0x00, 0x14]);
        d[8..11].copy_from_slice(&(t.clock as u32).to_le_bytes()[..3]);
        d[11] = 0x88;
        for (i, v) in
            [px, t.xblank, t.xfront, t.xsync, py, t.yblank, t.yfront, t.ysync].into_iter().enumerate()
        {
            d[12 + 2 * i..14 + 2 * i].copy_from_slice(&(v.wrapping_sub(1) as u16).to_le_bytes());
        }
        let n = usize::from(d[2]) + 4;
        checksum(&mut d[1..], n);
    }

    checksum(&mut e, 127);
    if has_dta {
        checksum(&mut e[128..], 127);
    }
    if has_did {
        checksum(&mut e[256..], 127);
    }
    e
}

/// `atoi` di C: cifre decimali iniziali (con segno), il resto si ignora.
fn atoi(s: &str) -> u32 {
    let s = s.trim_start();
    let (neg, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let v = digits
        .bytes()
        .take_while(u8::is_ascii_digit)
        .fold(0u32, |a, c| a.wrapping_mul(10).wrapping_add(u32::from(c - b'0')));
    if neg { v.wrapping_neg() } else { v }
}

/// Lunghezza dell'EDID nel buffer (128 per blocco), 0 se non è un EDID.
pub fn size(e: &[u8]) -> usize {
    if e.len() < 128 || e[0] != 0 || e[1] != 0xff {
        return 0;
    }
    128 * (usize::from(e[126]) + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `od -An -tx1 /sys/class/drm/card0-Virtual-1/edid` nel guest sotto
    /// QEMU 10.0 e 8.2 (`-device virtio-gpu-device`, 1280x800 di default).
    const QEMU_1280X800: &str = "
 00 ff ff ff ff ff ff 00 49 14 34 12 00 00 00 00
 2a 18 01 04 a5 20 14 78 06 ee 91 a3 54 4c 99 26
 0f 50 54 21 08 00 e1 c0 d1 c0 d1 00 a9 40 b3 00
 95 00 81 80 81 40 ea 29 00 c0 51 20 1c 30 40 26
 44 40 45 cb 10 00 00 18 00 00 00 f7 00 0a 00 40
 82 00 28 20 00 00 00 00 00 00 00 00 00 fd 00 32
 7d 1e a0 ff 01 0a 20 20 20 20 20 20 00 00 00 fc
 00 51 45 4d 55 20 4d 6f 6e 69 74 6f 72 0a 01 3a
 02 03 0b 00 46 7d 65 60 59 1f 61 00 00 00 10 00
 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
 10 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
 00 00 10 00 00 00 00 00 00 00 00 00 00 00 00 00
 00 00 00 00 10 00 00 00 00 00 00 00 00 00 00 00
 00 00 00 00 00 00 10 00 00 00 00 00 00 00 00 00
 00 00 00 00 00 00 00 00 10 00 00 00 00 00 00 00
 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 2f";

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace().map(|b| u8::from_str_radix(b, 16).unwrap()).collect()
    }

    #[test]
    fn come_qemu() {
        let e = generate(&EdidInfo::default(), 1024);
        assert_eq!(size(&e), 256);
        assert_eq!(&e[..256], &hex(QEMU_1280X800)[..]);
        assert!(e[256..].iter().all(|&b| b == 0));
    }

    #[test]
    fn somme_di_controllo_e_estensioni() {
        for (x, y) in [(1080, 1920), (640, 480), (5120, 2880), (4096, 2160), (1920, 1080)] {
            let info = EdidInfo { prefx: x, prefy: y, ..EdidInfo::default() };
            let e = generate(&info, 1024);
            let n = size(&e);
            assert!(n == 256 || n == 384, "{x}x{y}: {n}");
            for blk in e[..n].chunks(128) {
                assert_eq!(blk.iter().fold(0u8, |a, &b| a.wrapping_add(b)), 0, "{x}x{y}");
            }
            // Il descrittore dettagliato ha 12 bit: oltre 4096 va in DisplayID.
            assert_eq!(n == 384, x >= 4096 || y >= 4096, "{x}x{y}");
        }
        assert_eq!(size(&generate(&EdidInfo::default(), 128)), 128);
    }

    #[test]
    fn nome_seriale_e_limiti() {
        let info = EdidInfo {
            vendor: *b"VTR",
            name: "Vetro Display Panel".into(),
            serial: Some("1234abc".into()),
            maxx: 1024,
            maxy: 768,
            ..EdidInfo::default()
        };
        let e = generate(&info, 256);
        assert_eq!(u32::from_le_bytes(e[12..16].try_into().unwrap()), 1234);
        // Nome troncato a 12 caratteri, seriale nel primo descrittore CTA.
        let name = e.windows(13).any(|w| w == b"Vetro Displa\n");
        assert!(name);
        assert!(e[128..].windows(8).any(|w| w == b"1234abc\n"));
        // Con max 1024x768 restano solo i modi fino a quella risoluzione.
        assert_eq!(e[35], 0x21);
        assert_eq!(e[36], 0x08);
        assert!(e[38..54].chunks(2).all(|s| s == [1, 1]));
        assert_eq!(atoi("  -12x"), (-12i32) as u32);
    }
}
