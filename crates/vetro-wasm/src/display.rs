//! Il display di virtio-gpu per la pagina (M5): per ogni scanout
//! un'immagine RGBA (4 byte per pixel, righe da `width * 4` byte) che JS
//! legge direttamente dalla memoria del modulo, l'unione dei rettangoli
//! cambiati dall'ultima lettura, un contatore di aggiornamenti e il
//! cursore. Sostituisce `MemDisplay` nella GPU della macchina di
//! vetro-wasm; al guest non cambia niente.

use vetro_platform::virtio::gpu::Cursor;
use vetro_platform::virtio::{DisplayBackend, Frame, Rect};

/// Lato del cursore di virtio-gpu.
pub const CURSOR_SIZE: u32 = 64;

#[derive(Clone, Debug, Default)]
pub struct Screen {
    pub width: u32,
    pub height: u32,
    /// Lo scanout mostra un'immagine.
    pub on: bool,
    pub rgba: Vec<u8>,
    /// Aggiornamenti (anche lo spegnimento), per sapere se ridisegnare.
    pub updates: u64,
    /// Unione dei rettangoli cambiati non ancora letti.
    pub dirty: Option<Rect>,
    pub cursor: Cursor,
    /// Immagine del cursore in RGBA (vuota se non c'è).
    pub cursor_rgba: Vec<u8>,
    /// Cambi del cursore (forma o posizione).
    pub cursor_updates: u64,
}

fn union(a: Option<Rect>, b: Rect) -> Rect {
    let Some(a) = a else { return b };
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    let x1 = (a.x + a.width).max(b.x + b.width);
    let y1 = (a.y + a.height).max(b.y + b.height);
    Rect::new(x0, y0, x1 - x0, y1 - y0)
}

#[derive(Clone, Debug, Default)]
pub struct WebDisplay {
    pub screens: Vec<Screen>,
}

impl WebDisplay {
    pub fn screen(&self, scanout: u32) -> Option<&Screen> {
        self.screens.get(scanout as usize)
    }

    fn screen_mut(&mut self, scanout: u32) -> &mut Screen {
        let i = scanout as usize;
        if self.screens.len() <= i {
            self.screens.resize_with(i + 1, Screen::default);
        }
        &mut self.screens[i]
    }

    /// Toglie e restituisce il rettangolo cambiato.
    pub fn take_dirty(&mut self, scanout: u32) -> Option<Rect> {
        self.screens.get_mut(scanout as usize)?.dirty.take()
    }
}

impl DisplayBackend for WebDisplay {
    fn update(&mut self, scanout: u32, frame: &Frame<'_>, dirty: Rect) {
        let s = self.screen_mut(scanout);
        s.updates += 1;
        let mut dirty = dirty;
        if !s.on || (s.width, s.height) != (frame.width, frame.height) {
            s.width = frame.width;
            s.height = frame.height;
            s.rgba = vec![0; (frame.width * frame.height * 4) as usize];
            dirty = Rect::new(0, 0, frame.width, frame.height);
            s.dirty = None;
        }
        s.on = true;
        let w = frame.width as usize;
        for y in dirty.y..dirty.y + dirty.height {
            let src = &frame.data[(y * frame.stride + dirty.x * 4) as usize..][..dirty.width as usize * 4];
            let at = (y as usize * w + dirty.x as usize) * 4;
            let dst = &mut s.rgba[at..at + dirty.width as usize * 4];
            for (d, p) in dst.as_chunks_mut::<4>().0.iter_mut().zip(src.as_chunks::<4>().0) {
                *d = frame.format.to_rgba(*p);
            }
        }
        s.dirty = Some(union(s.dirty, dirty));
    }

    fn disable(&mut self, scanout: u32) {
        let s = self.screen_mut(scanout);
        s.updates += 1;
        s.on = false;
        s.dirty = None;
    }

    fn cursor(&mut self, scanout: u32, cursor: &Cursor) {
        let s = self.screen_mut(scanout);
        s.cursor_updates += 1;
        if cursor.image != s.cursor.image {
            // B8G8R8A8 come la risorsa (vedi `Cursor::image`).
            s.cursor_rgba =
                cursor.image.as_chunks::<4>().0.iter().flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
        }
        s.cursor = cursor.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vetro_platform::virtio::PixelFormat;

    /// Conversione in RGBA, rettangoli uniti fino alla lettura, cambio di
    /// dimensione che ridisegna tutto, spegnimento.
    #[test]
    fn immagine_rgba_e_rettangoli_cambiati() {
        let (w, h) = (4u32, 3u32);
        // B8G8R8X8, stride più largo della riga.
        let stride = 20u32;
        let data: Vec<u8> = (0..stride * h).map(|i| i as u8).collect();
        let f = Frame { width: w, height: h, stride, format: PixelFormat::B8G8R8X8, data: &data };
        let mut d = WebDisplay::default();
        d.update(0, &f, Rect::new(1, 1, 1, 1));
        let s = d.screen(0).unwrap();
        assert!(s.on);
        assert_eq!(s.rgba.len(), 48);
        // Pixel (1, 1): byte 24..28 = [24, 25, 26, 27] -> R=26 G=25 B=24 X->255.
        assert_eq!(&s.rgba[(4 + 1) * 4..(4 + 1) * 4 + 4], &[26, 25, 24, 255]);
        assert_eq!(d.take_dirty(0), Some(Rect::new(0, 0, 4, 3)), "prima immagine: tutto");
        d.update(0, &f, Rect::new(0, 0, 1, 1));
        d.update(0, &f, Rect::new(2, 1, 2, 2));
        assert_eq!(d.take_dirty(0), Some(Rect::new(0, 0, 4, 3)));
        d.update(0, &f, Rect::new(2, 1, 1, 1));
        assert_eq!(d.take_dirty(0), Some(Rect::new(2, 1, 1, 1)));
        assert_eq!(d.take_dirty(0), None);
        assert_eq!(d.screen(0).unwrap().updates, 4);
        d.disable(0);
        assert!(!d.screen(0).unwrap().on);
        let c = Cursor { resource_id: 5, x: 3, image: vec![1, 2, 3, 4], ..Cursor::default() };
        d.cursor(0, &c);
        assert_eq!(d.screen(0).unwrap().cursor_rgba, [3, 2, 1, 4]);
        assert_eq!(d.screen(0).unwrap().cursor_updates, 1);
    }
}
