//! virtio-gpu 2D (virtio v1.2, §5.7), senza virgl né blob.
//!
//! Code: 0 = controllo (comando leggibile, risposta scrivibile), 1 =
//! cursore (comandi senza risposta). Feature offerta: EDID (disattivabile).
//! Le risorse 2D stanno in memoria dell'host (come le immagini pixman di
//! QEMU); il guest ci copia i pixel dalla sua memoria di backing con
//! TRANSFER_TO_HOST_2D e le mostra con SET_SCANOUT e RESOURCE_FLUSH. Il
//! risultato arriva al [`DisplayBackend`] (nel browser: canvas o WebGPU;
//! nei test: [`MemDisplay`]).
//!
//! Comandi: GET_DISPLAY_INFO, GET_EDID, RESOURCE_CREATE_2D, RESOURCE_UNREF,
//! SET_SCANOUT, RESOURCE_FLUSH, TRANSFER_TO_HOST_2D,
//! RESOURCE_ATTACH_BACKING, RESOURCE_DETACH_BACKING; UPDATE_CURSOR e
//! MOVE_CURSOR sulla coda del cursore. Controlli ed errori come QEMU 10.0
//! (hw/display/virtio-gpu.c):
//! - id di risorsa 0 o già usato: ERR_INVALID_RESOURCE_ID; formato
//!   sconosciuto: ERR_INVALID_PARAMETER; oltre `max_hostmem` (256 MiB):
//!   ERR_OUT_OF_MEMORY;
//! - rettangoli di TRANSFER e FLUSH fuori dalla risorsa, di SET_SCANOUT
//!   fuori o più piccoli di 16x16: ERR_INVALID_PARAMETER; scanout
//!   inesistente: ERR_INVALID_SCANOUT_ID;
//! - TRANSFER, SET_SCANOUT e DETACH senza backing, ATTACH su una risorsa
//!   che ce l'ha già, più di 16384 voci o voci fuori dalla RAM: ERR_UNSPEC;
//! - GET_CAPSET*, comandi 3D e UUID: ERR_UNSPEC; blob: ERR_INVALID_PARAMETER;
//! - con VIRTIO_GPU_FLAG_FENCE la risposta riporta flag, fence_id e ctx_id:
//!   i comandi si eseguono subito, quindi il fence è già segnalato.
//!
//! Differenze volute da QEMU, solo su richieste malformate che Linux non
//! manda: un comando più corto della sua struttura riceve
//! ERR_INVALID_PARAMETER (QEMU risponde OK_NODATA senza eseguirlo, o per
//! l'intestazione blocca la coda). Una risposta che non entra nei buffer
//! scrivibili si tronca, come in QEMU.
//!
//! Il display: `set_display` cambia la risoluzione richiesta di uno
//! scanout (evento VIRTIO_GPU_EVENT_DISPLAY con interrupt di
//! configurazione, come il ridimensionamento di una finestra in QEMU).

use core::any::Any;
use std::collections::BTreeMap;

use super::edid::{self, EdidInfo};
use super::*;

pub const F_VIRGL: u64 = 1 << 0;
pub const F_EDID: u64 = 1 << 1;

// Comandi (enum virtio_gpu_ctrl_type).
pub const CMD_GET_DISPLAY_INFO: u32 = 0x0100;
pub const CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
pub const CMD_RESOURCE_UNREF: u32 = 0x0102;
pub const CMD_SET_SCANOUT: u32 = 0x0103;
pub const CMD_RESOURCE_FLUSH: u32 = 0x0104;
pub const CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
pub const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
pub const CMD_RESOURCE_DETACH_BACKING: u32 = 0x0107;
pub const CMD_GET_CAPSET_INFO: u32 = 0x0108;
pub const CMD_GET_CAPSET: u32 = 0x0109;
pub const CMD_GET_EDID: u32 = 0x010a;
pub const CMD_RESOURCE_ASSIGN_UUID: u32 = 0x010b;
pub const CMD_RESOURCE_CREATE_BLOB: u32 = 0x010c;
pub const CMD_SET_SCANOUT_BLOB: u32 = 0x010d;
pub const CMD_UPDATE_CURSOR: u32 = 0x0300;
pub const CMD_MOVE_CURSOR: u32 = 0x0301;

// Risposte.
pub const RESP_OK_NODATA: u32 = 0x1100;
pub const RESP_OK_DISPLAY_INFO: u32 = 0x1101;
pub const RESP_OK_EDID: u32 = 0x1104;
pub const RESP_ERR_UNSPEC: u32 = 0x1200;
pub const RESP_ERR_OUT_OF_MEMORY: u32 = 0x1201;
pub const RESP_ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
pub const RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
pub const RESP_ERR_INVALID_CONTEXT_ID: u32 = 0x1204;
pub const RESP_ERR_INVALID_PARAMETER: u32 = 0x1205;

pub const FLAG_FENCE: u32 = 1 << 0;
/// Bit di `events_read`: la configurazione dei display è cambiata.
pub const EVENT_DISPLAY: u32 = 1 << 0;

/// Scanout al più (VIRTIO_GPU_MAX_SCANOUTS).
pub const MAX_SCANOUTS: u32 = 16;
/// Voci di backing al più per ATTACH_BACKING (come QEMU).
pub const MAX_BACKING_ENTRIES: u32 = 16384;
/// Lato del cursore (QEMU alloca sempre 64x64).
pub const CURSOR_SIZE: u32 = 64;

const CTRLQ: usize = 0;
const CURSORQ: usize = 1;
const HDR_LEN: usize = 24;
/// Byte del buffer EDID nella risposta.
const EDID_BLOB: usize = 1024;

/// Formati 2D, con i valori di `enum virtio_gpu_formats`. Il nome dice
/// l'ordine dei byte in memoria (B8G8R8A8: B nel primo byte).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    B8G8R8A8 = 1,
    B8G8R8X8 = 2,
    A8R8G8B8 = 3,
    X8R8G8B8 = 4,
    R8G8B8A8 = 67,
    X8B8G8R8 = 68,
    A8B8G8R8 = 121,
    R8G8B8X8 = 134,
}

impl PixelFormat {
    pub fn from_virtio(v: u32) -> Option<Self> {
        use PixelFormat::*;
        Some(match v {
            1 => B8G8R8A8,
            2 => B8G8R8X8,
            3 => A8R8G8B8,
            4 => X8R8G8B8,
            67 => R8G8B8A8,
            68 => X8B8G8R8,
            121 => A8B8G8R8,
            134 => R8G8B8X8,
            _ => return None,
        })
    }

    /// Tutti i formati sono a 32 bit.
    pub const fn bytes_per_pixel(self) -> u32 {
        4
    }

    /// Un pixel (4 byte in memoria) in RGBA; il canale X diventa 255.
    pub fn to_rgba(self, p: [u8; 4]) -> [u8; 4] {
        use PixelFormat::*;
        match self {
            B8G8R8A8 => [p[2], p[1], p[0], p[3]],
            B8G8R8X8 => [p[2], p[1], p[0], 255],
            A8R8G8B8 => [p[1], p[2], p[3], p[0]],
            X8R8G8B8 => [p[1], p[2], p[3], 255],
            R8G8B8A8 => [p[0], p[1], p[2], p[3]],
            X8B8G8R8 => [p[3], p[2], p[1], 255],
            A8B8G8R8 => [p[3], p[2], p[1], p[0]],
            R8G8B8X8 => [p[0], p[1], p[2], 255],
        }
    }
}

/// Rettangolo (`struct virtio_gpu_rect`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self { x, y, width, height }
    }

    fn parse(b: &[u8]) -> Self {
        Self { x: le32(b, 0), y: le32(b, 4), width: le32(b, 8), height: le32(b, 12) }
    }

    /// Dentro un'area `w`x`h`, con i controlli di QEMU (anche contro
    /// l'overflow di x + width, fatto a 64 bit).
    fn within(&self, w: u32, h: u32) -> bool {
        self.x <= w
            && self.y <= h
            && self.width <= w
            && self.height <= h
            && u64::from(self.x) + u64::from(self.width) <= u64::from(w)
            && u64::from(self.y) + u64::from(self.height) <= u64::from(h)
    }

    fn intersect(&self, o: &Rect) -> Option<Rect> {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = (u64::from(self.x) + u64::from(self.width)).min(u64::from(o.x) + u64::from(o.width));
        let y1 = (u64::from(self.y) + u64::from(self.height)).min(u64::from(o.y) + u64::from(o.height));
        (x1 > u64::from(x0) && y1 > u64::from(y0))
            .then(|| Rect::new(x0, y0, (x1 - u64::from(x0)) as u32, (y1 - u64::from(y0)) as u32))
    }
}

/// L'immagine di uno scanout: la parte della risorsa scelta con
/// SET_SCANOUT. `data` parte dal pixel (0, 0) dello scanout; le righe
/// distano `stride` byte.
#[derive(Clone, Copy, Debug)]
pub struct Frame<'a> {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: PixelFormat,
    pub data: &'a [u8],
}

impl Frame<'_> {
    /// Il pixel (x, y) in RGBA.
    pub fn rgba(&self, x: u32, y: u32) -> [u8; 4] {
        let o = (y * self.stride + x * 4) as usize;
        self.format.to_rgba(self.data[o..o + 4].try_into().unwrap())
    }
}

/// Stato del cursore di uno scanout.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    /// Risorsa dell'immagine (0 = cursore nascosto).
    pub resource_id: u32,
    pub x: u32,
    pub y: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    /// Immagine 64x64 in B8G8R8A8 come la risorsa (vuota se la risorsa
    /// non è 64x64 o non c'è), aggiornata da UPDATE_CURSOR.
    pub image: Vec<u8>,
}

/// Dove va l'immagine. I metodi ricevono sempre dati già validati.
pub trait DisplayBackend: Any {
    /// Il rettangolo `dirty` (coordinate dello scanout) di `frame` è
    /// cambiato. Dopo SET_SCANOUT arriva con l'intero scanout.
    fn update(&mut self, scanout: u32, frame: &Frame<'_>, dirty: Rect);
    /// Lo scanout non mostra più niente.
    fn disable(&mut self, scanout: u32);
    /// Cursore definito (UPDATE_CURSOR) o spostato (MOVE_CURSOR).
    fn cursor(&mut self, _scanout: u32, _cursor: &Cursor) {}
}

/// Backend in memoria: l'ultima immagine di ogni scanout, in RGBA.
#[derive(Clone, Debug, Default)]
pub struct MemDisplay {
    /// Per scanout: (larghezza, altezza, pixel RGBA riga per riga).
    pub screens: BTreeMap<u32, (u32, u32, Vec<u8>)>,
    pub cursors: BTreeMap<u32, Cursor>,
    /// Aggiornamenti ricevuti.
    pub updates: u64,
}

impl MemDisplay {
    /// Il pixel (x, y) dello scanout, in RGBA.
    pub fn pixel(&self, scanout: u32, x: u32, y: u32) -> Option<[u8; 4]> {
        let (w, h, px) = self.screens.get(&scanout)?;
        if x >= *w || y >= *h {
            return None;
        }
        let o = ((y * w + x) * 4) as usize;
        Some(px[o..o + 4].try_into().unwrap())
    }
}

impl DisplayBackend for MemDisplay {
    fn update(&mut self, scanout: u32, frame: &Frame<'_>, dirty: Rect) {
        self.updates += 1;
        let s = self.screens.entry(scanout).or_default();
        if (s.0, s.1) != (frame.width, frame.height) {
            *s = (frame.width, frame.height, vec![0; (frame.width * frame.height * 4) as usize]);
        }
        for y in dirty.y..dirty.y + dirty.height {
            for x in dirty.x..dirty.x + dirty.width {
                let o = ((y * frame.width + x) * 4) as usize;
                s.2[o..o + 4].copy_from_slice(&frame.rgba(x, y));
            }
        }
    }

    fn disable(&mut self, scanout: u32) {
        self.screens.remove(&scanout);
    }

    fn cursor(&mut self, scanout: u32, cursor: &Cursor) {
        self.cursors.insert(scanout, cursor.clone());
    }
}

/// Configurazione del dispositivo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuConfig {
    /// Numero di scanout (1..=16).
    pub scanouts: u32,
    /// Risoluzione iniziale dello scanout 0 (quella di QEMU: 1280x800).
    pub width: u32,
    pub height: u32,
    /// Offre VIRTIO_GPU_F_EDID.
    pub edid: bool,
    /// Monitor descritto dall'EDID (le dimensioni preferite si prendono
    /// dalla risoluzione dello scanout).
    pub monitor: EdidInfo,
    /// Memoria massima delle risorse (come `max_hostmem` di QEMU).
    pub max_hostmem: u64,
}

impl Default for GpuConfig {
    fn default() -> Self {
        Self {
            scanouts: 1,
            width: 1280,
            height: 800,
            edid: true,
            monitor: EdidInfo::default(),
            max_hostmem: 256 << 20,
        }
    }
}

struct Resource {
    width: u32,
    height: u32,
    format: PixelFormat,
    /// Pixel, righe da `width * 4` byte (lo stride di pixman a 32 bit).
    data: Vec<u8>,
    /// Memoria del guest: (indirizzo, lunghezza).
    backing: Option<Vec<(u64, u32)>>,
    /// Scanout che la mostrano (bit per scanout).
    scanouts: u32,
}

impl Resource {
    fn stride(&self) -> u32 {
        self.width * 4
    }
}

#[derive(Clone, Debug, Default)]
struct Scanout {
    /// Risoluzione chiesta dall'host (0x0 = display spento).
    req_width: u32,
    req_height: u32,
    /// Risorsa mostrata (0 = nessuna) e sua parte.
    resource_id: u32,
    rect: Rect,
    cursor: Cursor,
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

pub struct VirtioGpu {
    config: GpuConfig,
    backend: Box<dyn DisplayBackend>,
    resources: BTreeMap<u32, Resource>,
    hostmem: u64,
    scanouts: Vec<Scanout>,
    events_read: u32,
    display_changed: bool,
    queue_sizes: [u16; 2],
}

impl VirtioGpu {
    /// Code da 64 (controllo) e 16 (cursore) come QEMU senza virgl.
    pub fn new(backend: Box<dyn DisplayBackend>, config: GpuConfig) -> Self {
        let n = config.scanouts.clamp(1, MAX_SCANOUTS) as usize;
        let mut scanouts = vec![Scanout::default(); n];
        scanouts[0].req_width = config.width;
        scanouts[0].req_height = config.height;
        Self {
            config,
            backend,
            resources: BTreeMap::new(),
            hostmem: 0,
            scanouts,
            events_read: 0,
            display_changed: false,
            queue_sizes: [64, 16],
        }
    }

    /// Cambia backend (es. quello del browser al posto di [`MemDisplay`]):
    /// il nuovo riceve subito l'immagine e il cursore di ogni scanout.
    pub fn set_backend(&mut self, backend: Box<dyn DisplayBackend>) {
        self.backend = backend;
        for (i, s) in self.scanouts.iter().enumerate() {
            if let Some(r) = self.resources.get(&s.resource_id) {
                let full = Rect::new(0, 0, s.rect.width, s.rect.height);
                self.backend.update(i as u32, &Self::frame_of(r, s.rect), full);
            }
            if s.cursor != Cursor::default() {
                self.backend.cursor(i as u32, &s.cursor);
            }
        }
    }

    pub fn backend_mut(&mut self) -> &mut dyn DisplayBackend {
        self.backend.as_mut()
    }

    /// Accesso tipizzato al backend.
    pub fn backend_as_mut<T: DisplayBackend>(&mut self) -> Option<&mut T> {
        let b: &mut dyn Any = self.backend.as_mut();
        b.downcast_mut()
    }

    pub fn backend_as<T: DisplayBackend>(&self) -> Option<&T> {
        let b: &dyn Any = self.backend.as_ref();
        b.downcast_ref()
    }

    /// Risoluzione chiesta per lo scanout `scanout` (0x0 = spento), come il
    /// ridimensionamento della finestra: il driver lo vede con un
    /// interrupt di configurazione e rilegge GET_DISPLAY_INFO.
    pub fn set_display(&mut self, scanout: u32, width: u32, height: u32) {
        if let Some(s) = self.scanouts.get_mut(scanout as usize) {
            s.req_width = width;
            s.req_height = height;
            self.events_read |= EVENT_DISPLAY;
            self.display_changed = true;
        }
    }

    /// L'immagine attuale dello scanout, se ne mostra una.
    pub fn frame(&self, scanout: u32) -> Option<Frame<'_>> {
        let s = self.scanouts.get(scanout as usize)?;
        let r = self.resources.get(&s.resource_id)?;
        Some(Self::frame_of(r, s.rect))
    }

    pub fn cursor(&self, scanout: u32) -> Option<&Cursor> {
        self.scanouts.get(scanout as usize).map(|s| &s.cursor)
    }

    /// Risorse 2D esistenti.
    pub fn resource_count(&self) -> usize {
        self.resources.len()
    }

    /// Memoria occupata dalle risorse.
    pub fn hostmem(&self) -> u64 {
        self.hostmem
    }

    fn frame_of(r: &Resource, rect: Rect) -> Frame<'_> {
        let stride = r.stride();
        let start = (rect.y * stride + rect.x * 4) as usize;
        Frame { width: rect.width, height: rect.height, stride, format: r.format, data: &r.data[start..] }
    }

    fn config_bytes(&self) -> [u8; 16] {
        let mut c = [0u8; 16];
        c[0..4].copy_from_slice(&self.events_read.to_le_bytes());
        c[8..12].copy_from_slice(&(self.scanouts.len() as u32).to_le_bytes());
        // num_capsets = 0: niente 3D.
        c
    }

    fn display_info(&self) -> Vec<u8> {
        let mut r = vec![0u8; HDR_LEN + 16 * 24];
        r[0..4].copy_from_slice(&RESP_OK_DISPLAY_INFO.to_le_bytes());
        for (i, s) in self.scanouts.iter().enumerate() {
            if s.req_width != 0 && s.req_height != 0 {
                let o = HDR_LEN + 24 * i;
                r[o + 8..o + 12].copy_from_slice(&s.req_width.to_le_bytes());
                r[o + 12..o + 16].copy_from_slice(&s.req_height.to_le_bytes());
                r[o + 16..o + 20].copy_from_slice(&1u32.to_le_bytes());
            }
        }
        r
    }

    fn get_edid(&self, cmd: &[u8]) -> Result<Vec<u8>, u32> {
        let scanout = le32(cmd, HDR_LEN);
        let s = self.scanouts.get(scanout as usize).ok_or(RESP_ERR_INVALID_PARAMETER)?;
        let info = EdidInfo { prefx: s.req_width, prefy: s.req_height, ..self.config.monitor.clone() };
        let mut r = vec![0u8; HDR_LEN + 8 + EDID_BLOB];
        r[0..4].copy_from_slice(&RESP_OK_EDID.to_le_bytes());
        r[HDR_LEN..HDR_LEN + 4].copy_from_slice(&(EDID_BLOB as u32).to_le_bytes());
        r[HDR_LEN + 8..].copy_from_slice(&edid::generate(&info, EDID_BLOB));
        Ok(r)
    }

    fn create_2d(&mut self, cmd: &[u8]) -> Result<(), u32> {
        let (id, format, width, height) =
            (le32(cmd, HDR_LEN), le32(cmd, HDR_LEN + 4), le32(cmd, HDR_LEN + 8), le32(cmd, HDR_LEN + 12));
        if id == 0 || self.resources.contains_key(&id) {
            return Err(RESP_ERR_INVALID_RESOURCE_ID);
        }
        let format = PixelFormat::from_virtio(format).ok_or(RESP_ERR_INVALID_PARAMETER)?;
        let size = u64::from(width) * 4 * u64::from(height);
        if size + self.hostmem >= self.config.max_hostmem {
            return Err(RESP_ERR_OUT_OF_MEMORY);
        }
        self.hostmem += size;
        let data = vec![0u8; size as usize];
        self.resources.insert(id, Resource { width, height, format, data, backing: None, scanouts: 0 });
        Ok(())
    }

    fn disable_scanout(&mut self, i: usize) {
        let s = &mut self.scanouts[i];
        if s.resource_id == 0 {
            return;
        }
        if let Some(r) = self.resources.get_mut(&s.resource_id) {
            r.scanouts &= !(1 << i);
        }
        s.resource_id = 0;
        s.rect = Rect::default();
        self.backend.disable(i as u32);
    }

    fn destroy(&mut self, id: u32) {
        let Some(mask) = self.resources.get(&id).map(|r| r.scanouts) else { return };
        for i in 0..self.scanouts.len() {
            if mask & (1 << i) != 0 {
                self.disable_scanout(i);
            }
        }
        let r = self.resources.remove(&id).unwrap();
        self.hostmem -= r.data.len() as u64;
    }

    fn unref(&mut self, cmd: &[u8]) -> Result<(), u32> {
        let id = le32(cmd, HDR_LEN);
        if !self.resources.contains_key(&id) {
            return Err(RESP_ERR_INVALID_RESOURCE_ID);
        }
        self.destroy(id);
        Ok(())
    }

    /// Risorsa con backing (`virtio_gpu_find_check_resource` con
    /// require_backing).
    fn with_backing(&mut self, id: u32) -> Result<&mut Resource, u32> {
        let r = self.resources.get_mut(&id).ok_or(RESP_ERR_INVALID_RESOURCE_ID)?;
        if r.backing.is_none() {
            return Err(RESP_ERR_UNSPEC);
        }
        Ok(r)
    }

    fn set_scanout(&mut self, cmd: &[u8]) -> Result<(), u32> {
        let rect = Rect::parse(&cmd[HDR_LEN..]);
        let (scanout, id) = (le32(cmd, HDR_LEN + 16), le32(cmd, HDR_LEN + 20));
        let i = scanout as usize;
        if i >= self.scanouts.len() {
            return Err(RESP_ERR_INVALID_SCANOUT_ID);
        }
        if id == 0 {
            self.disable_scanout(i);
            return Ok(());
        }
        let r = self.with_backing(id)?;
        if rect.width < 16 || rect.height < 16 || !rect.within(r.width, r.height) {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        r.scanouts |= 1 << i;
        let old = self.scanouts[i].resource_id;
        if old != id
            && let Some(o) = self.resources.get_mut(&old)
        {
            o.scanouts &= !(1 << i);
        }
        let s = &mut self.scanouts[i];
        s.resource_id = id;
        s.rect = rect;
        let r = &self.resources[&id];
        let full = Rect::new(0, 0, rect.width, rect.height);
        self.backend.update(scanout, &Self::frame_of(r, rect), full);
        Ok(())
    }

    fn flush(&mut self, cmd: &[u8]) -> Result<(), u32> {
        let rect = Rect::parse(&cmd[HDR_LEN..]);
        let id = le32(cmd, HDR_LEN + 16);
        let r = self.resources.get(&id).ok_or(RESP_ERR_INVALID_RESOURCE_ID)?;
        if !rect.within(r.width, r.height) {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        for (i, s) in self.scanouts.iter().enumerate() {
            if r.scanouts & (1 << i) == 0 {
                continue;
            }
            if let Some(d) = rect.intersect(&s.rect) {
                let dirty = Rect::new(d.x - s.rect.x, d.y - s.rect.y, d.width, d.height);
                self.backend.update(i as u32, &Self::frame_of(r, s.rect), dirty);
            }
        }
        Ok(())
    }

    fn transfer(&mut self, cmd: &[u8], ram: &dyn GuestRam) -> Result<(), u32> {
        let rect = Rect::parse(&cmd[HDR_LEN..]);
        let offset = le64(cmd, HDR_LEN + 16);
        let id = le32(cmd, HDR_LEN + 24);
        let r = self.with_backing(id)?;
        if !rect.within(r.width, r.height) {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        let stride = u64::from(r.stride());
        let backing = r.backing.as_ref().unwrap();
        // Come QEMU: righe intere copiate in un colpo solo se il
        // rettangolo copre tutta la larghezza, altrimenti riga per riga
        // (sorgente a offset + stride * riga).
        if rect.x == 0 && rect.width == r.width {
            let dst = (u64::from(rect.y) * stride) as usize;
            let len = (stride * u64::from(rect.height)) as usize;
            read_backing(ram, backing, offset, &mut r.data[dst..dst + len]);
        } else {
            let row = (rect.width * 4) as usize;
            for h in 0..u64::from(rect.height) {
                let dst = ((u64::from(rect.y) + h) * stride + u64::from(rect.x) * 4) as usize;
                read_backing(ram, backing, offset + stride * h, &mut r.data[dst..dst + row]);
            }
        }
        Ok(())
    }

    fn attach_backing(&mut self, c: &DescChain, cmd: &[u8], ram: &dyn GuestRam) -> Result<(), u32> {
        let (id, n) = (le32(cmd, HDR_LEN), le32(cmd, HDR_LEN + 4));
        let r = self.resources.get_mut(&id).ok_or(RESP_ERR_INVALID_RESOURCE_ID)?;
        if r.backing.is_some() || n > MAX_BACKING_ENTRIES {
            return Err(RESP_ERR_UNSPEC);
        }
        let mut ents = vec![0u8; 16 * n as usize];
        if c.read(ram, 32, &mut ents).map_err(|_| RESP_ERR_UNSPEC)? != ents.len() {
            return Err(RESP_ERR_UNSPEC);
        }
        let mut backing = Vec::with_capacity(n as usize);
        for e in ents.chunks(16) {
            let (addr, len) = (le64(e, 0), le32(e, 8));
            // Come dma_memory_map: ogni voce deve stare tutta in RAM.
            let mut b = [0u8; 1];
            let last = addr.checked_add(u64::from(len).saturating_sub(1)).ok_or(RESP_ERR_UNSPEC)?;
            if len > 0 && (ram.read(addr, &mut b).is_err() || ram.read(last, &mut b).is_err()) {
                return Err(RESP_ERR_UNSPEC);
            }
            backing.push((addr, len));
        }
        r.backing = Some(backing);
        Ok(())
    }

    fn detach_backing(&mut self, cmd: &[u8]) -> Result<(), u32> {
        let id = le32(cmd, HDR_LEN);
        self.with_backing(id)?.backing = None;
        Ok(())
    }

    /// Esegue un comando della coda di controllo; restituisce la risposta.
    fn command(&mut self, c: &DescChain, ram: &dyn GuestRam) -> Vec<u8> {
        let cmd = c.read_to_vec(ram, 0).unwrap_or_default();
        let mut hdr = [0u8; HDR_LEN];
        let n = cmd.len().min(HDR_LEN);
        hdr[..n].copy_from_slice(&cmd[..n]);
        let ty = le32(&hdr, 0);
        let result = if cmd.len() < HDR_LEN {
            Err(RESP_ERR_INVALID_PARAMETER)
        } else {
            self.dispatch(ty, c, &cmd, ram)
        };
        let mut resp = match result {
            Ok(Some(r)) => r,
            Ok(None) => RESP_OK_NODATA.to_le_bytes().into_iter().chain([0; HDR_LEN - 4]).collect(),
            Err(e) => e.to_le_bytes().into_iter().chain([0; HDR_LEN - 4]).collect(),
        };
        if le32(&hdr, 4) & FLAG_FENCE != 0 {
            let flags = le32(&resp, 4) | FLAG_FENCE;
            resp[4..8].copy_from_slice(&flags.to_le_bytes());
            resp[8..20].copy_from_slice(&hdr[8..20]); // fence_id, ctx_id
        }
        resp
    }

    fn dispatch(
        &mut self,
        ty: u32,
        c: &DescChain,
        cmd: &[u8],
        ram: &dyn GuestRam,
    ) -> Result<Option<Vec<u8>>, u32> {
        // Lunghezza di ogni comando (struct virtio_gpu_*).
        let need = match ty {
            CMD_GET_DISPLAY_INFO => HDR_LEN,
            CMD_GET_EDID => HDR_LEN + 8,
            CMD_RESOURCE_CREATE_2D => HDR_LEN + 16,
            CMD_RESOURCE_UNREF | CMD_RESOURCE_DETACH_BACKING => HDR_LEN + 8,
            CMD_SET_SCANOUT | CMD_RESOURCE_FLUSH => HDR_LEN + 24,
            CMD_TRANSFER_TO_HOST_2D => HDR_LEN + 32,
            CMD_RESOURCE_ATTACH_BACKING => HDR_LEN + 8,
            _ => HDR_LEN,
        };
        if cmd.len() < need {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        match ty {
            CMD_GET_DISPLAY_INFO => Ok(Some(self.display_info())),
            CMD_GET_EDID if self.config.edid => self.get_edid(cmd).map(Some),
            CMD_RESOURCE_CREATE_2D => self.create_2d(cmd).map(|_| None),
            CMD_RESOURCE_UNREF => self.unref(cmd).map(|_| None),
            CMD_SET_SCANOUT => self.set_scanout(cmd).map(|_| None),
            CMD_RESOURCE_FLUSH => self.flush(cmd).map(|_| None),
            CMD_TRANSFER_TO_HOST_2D => self.transfer(cmd, ram).map(|_| None),
            CMD_RESOURCE_ATTACH_BACKING => self.attach_backing(c, cmd, ram).map(|_| None),
            CMD_RESOURCE_DETACH_BACKING => self.detach_backing(cmd).map(|_| None),
            CMD_RESOURCE_CREATE_BLOB | CMD_SET_SCANOUT_BLOB => Err(RESP_ERR_INVALID_PARAMETER),
            // GET_EDID senza la feature, capset, 3D, UUID: come il default
            // di QEMU.
            _ => Err(RESP_ERR_UNSPEC),
        }
    }

    fn cursor_command(&mut self, cmd: &[u8]) {
        if cmd.len() < 56 {
            return;
        }
        let ty = le32(cmd, 0);
        let (scanout, x, y) = (le32(cmd, HDR_LEN), le32(cmd, HDR_LEN + 4), le32(cmd, HDR_LEN + 8));
        let (id, hot_x, hot_y) = (le32(cmd, HDR_LEN + 16), le32(cmd, HDR_LEN + 20), le32(cmd, HDR_LEN + 24));
        let Some(s) = self.scanouts.get_mut(scanout as usize) else { return };
        if ty == CMD_MOVE_CURSOR {
            s.cursor.x = x;
            s.cursor.y = y;
        } else {
            s.cursor =
                Cursor { resource_id: id, x, y, hot_x, hot_y, image: core::mem::take(&mut s.cursor.image) };
            if id > 0
                && let Some(r) = self.resources.get(&id)
                && (r.width, r.height) == (CURSOR_SIZE, CURSOR_SIZE)
            {
                s.cursor.image = r.data.clone();
            }
        }
        let c = s.cursor.clone();
        self.backend.cursor(scanout, &c);
    }
}

/// Copia dal backing (voci in ordine, viste come spazio contiguo) a
/// partire da `offset`; ciò che il backing non copre resta com'è, come
/// `iov_to_buf` di QEMU.
fn read_backing(ram: &dyn GuestRam, backing: &[(u64, u32)], mut offset: u64, out: &mut [u8]) {
    let mut done = 0usize;
    for &(addr, len) in backing {
        if done == out.len() {
            break;
        }
        let len = u64::from(len);
        if offset >= len {
            offset -= len;
            continue;
        }
        let n = ((len - offset) as usize).min(out.len() - done);
        // Le voci sono state controllate all'ATTACH: la RAM non si restringe.
        let _ = ram.read(addr + offset, &mut out[done..done + n]);
        done += n;
        offset = 0;
    }
}

impl VirtioDevice for VirtioGpu {
    fn device_id(&self) -> u32 {
        ID_GPU
    }

    fn features(&self) -> u64 {
        if self.config.edid { F_EDID } else { 0 }
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.queue_sizes
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        read_config_bytes(&self.config_bytes(), offset, data);
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        // events_clear (offset 4) azzera i bit di events_read.
        let mut v = [0u8; 4];
        for (i, &b) in data.iter().enumerate() {
            if let Some(k) = (offset + i as u64).checked_sub(4).filter(|&k| k < 4) {
                v[k as usize] = b;
            }
        }
        self.events_read &= !u32::from_le_bytes(v);
    }

    fn reset(&mut self) {
        let ids: Vec<u32> = self.resources.keys().copied().collect();
        for id in ids {
            self.destroy(id);
        }
        for s in &mut self.scanouts {
            s.cursor = Cursor::default();
        }
        self.display_changed = false;
    }

    fn service(&mut self, ctx: &mut ServiceCtx<'_>) -> Result<(), QueueError> {
        if core::mem::take(&mut self.display_changed) {
            ctx.config_changed();
        }
        let (queues, ram) = (&mut *ctx.queues, &mut *ctx.ram);
        let q = &mut queues[CTRLQ];
        while let Some(c) = q.pop(ram)? {
            let resp = self.command(&c, ram);
            let n = c.write(ram, 0, &resp)?;
            q.push_used(ram, c.head, n as u32)?;
        }
        let q = &mut queues[CURSORQ];
        while let Some(c) = q.pop(ram)? {
            let mut cmd = [0u8; 56];
            let n = c.read(ram, 0, &mut cmd)?;
            self.cursor_command(&cmd[..n]);
            q.push_used(ram, c.head, 0)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
