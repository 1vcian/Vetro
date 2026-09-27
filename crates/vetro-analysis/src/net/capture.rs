//! Frames captured at the virtio-net boundary.
//!
//! The timestamp is the guest virtual time in microseconds (that of the
//! network stack, converted CNTPCT): same instructions, same
//! instants. The direction is as seen by the guest.

/// Direction of a frame relative to the guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Direction {
    /// Transmitted by the guest (in pcapng: `outbound` on the guest interface).
    FromGuest,
    /// Delivered to the guest (`inbound`).
    ToGuest,
}

/// A complete Ethernet frame (without FCS).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Guest virtual time in microseconds.
    pub at_us: u64,
    pub dir: Direction,
    pub data: Vec<u8>,
}

/// A capture: frames in time order (non-decreasing).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capture {
    frames: Vec<Frame>,
}

impl Capture {
    pub fn new() -> Self {
        Capture::default()
    }

    /// Adds a frame. An instant in the past (should not happen)
    /// becomes that of the last frame, so the order stays monotonic.
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
