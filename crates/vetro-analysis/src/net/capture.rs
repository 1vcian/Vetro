//! Frame catturati al confine di virtio-net.
//!
//! La marca temporale è il tempo virtuale del guest in microsecondi (quello
//! dello stack di rete, CNTPCT convertito): stesse istruzioni, stessi
//! istanti. Il verso è visto dal guest.

/// Verso di un frame rispetto al guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Direction {
    /// Trasmesso dal guest (in pcapng: `outbound` sull'interfaccia del guest).
    FromGuest,
    /// Consegnato al guest (`inbound`).
    ToGuest,
}

/// Un frame Ethernet completo (senza FCS).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Tempo virtuale del guest in microsecondi.
    pub at_us: u64,
    pub dir: Direction,
    pub data: Vec<u8>,
}

/// Una cattura: frame in ordine di tempo (non decrescente).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capture {
    frames: Vec<Frame>,
}

impl Capture {
    pub fn new() -> Self {
        Capture::default()
    }

    /// Aggiunge un frame. Un istante nel passato (non dovrebbe succedere)
    /// diventa quello dell'ultimo frame, così l'ordine resta monotono.
    pub fn push(&mut self, at_us: u64, dir: Direction, data: Vec<u8>) {
        let at_us = self.frames.last().map_or(at_us, |f| at_us.max(f.at_us));
        self.frames.push(Frame { at_us, dir, data });
    }

    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn into_frames(self) -> Vec<Frame> {
        self.frames
    }
}

impl From<Vec<Frame>> for Capture {
    fn from(frames: Vec<Frame>) -> Self {
        let mut c = Capture::new();
        for f in frames {
            c.push(f.at_us, f.dir, f.data);
        }
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tempo_monotono() {
        let mut c = Capture::new();
        c.push(10, Direction::FromGuest, vec![1]);
        c.push(5, Direction::ToGuest, vec![2]);
        c.push(20, Direction::ToGuest, vec![3]);
        let t: Vec<u64> = c.frames().iter().map(|f| f.at_us).collect();
        assert_eq!(t, [10, 10, 20]);
        assert_eq!(c.len(), 3);
    }
}
