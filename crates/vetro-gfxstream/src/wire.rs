//! emugen wire format (see `tools/gfxstream/gen-tables.py`): one call is
//! `u32 opcode, u32 packet length, parameters`; the reply is the "out"
//! buffers in order, then the return value.

/// Kind of a parameter on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum P {
    /// Scalar of 1, 2, 4 or 8 bytes.
    S1,
    S2,
    S4,
    S8,
    /// Pointer sent by the guest: u32 size, then the bytes.
    In,
    /// Pointer filled by the host: u32 size; the bytes go in the reply.
    Out,
}

/// One entry of a generated table.
#[derive(Debug)]
pub struct Op {
    pub name: &'static str,
    pub params: &'static [P],
    /// Bytes of the return value (0 = void).
    pub ret: u8,
}

/// A decoded parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arg<'a> {
    S(u64),
    In(&'a [u8]),
    Out(u32),
}

/// Length of the call at the start of `buf`, if its header is complete:
/// (opcode, packet length). A packet length below 8 is malformed.
pub fn peek(buf: &[u8]) -> Option<(u32, usize)> {
    if buf.len() < 8 {
        return None;
    }
    let op = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    let len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
    Some((op, len))
}

/// Decodes the parameters of one complete packet (header included) with
/// its table entry. `None` if the packet is shorter than its parameters.
pub fn decode<'a>(op: &Op, packet: &'a [u8], out: &mut Vec<Arg<'a>>) -> Option<()> {
    out.clear();
    let mut o = 8usize;
    let take = |o: &mut usize, n: usize| -> Option<&'a [u8]> {
        let s = packet.get(*o..*o + n)?;
        *o += n;
        Some(s)
    };
    for p in op.params {
        let a = match p {
            P::S1 => Arg::S(u64::from(take(&mut o, 1)?[0])),
            P::S2 => Arg::S(u64::from(u16::from_le_bytes(take(&mut o, 2)?.try_into().unwrap()))),
            P::S4 => Arg::S(u64::from(u32::from_le_bytes(take(&mut o, 4)?.try_into().unwrap()))),
            P::S8 => Arg::S(u64::from_le_bytes(take(&mut o, 8)?.try_into().unwrap())),
            P::In => {
                let n = u32::from_le_bytes(take(&mut o, 4)?.try_into().unwrap()) as usize;
                Arg::In(take(&mut o, n)?)
            }
            P::Out => Arg::Out(u32::from_le_bytes(take(&mut o, 4)?.try_into().unwrap())),
        };
        out.push(a);
    }
    Some(())
}

/// Typed access to decoded arguments, lenient like the upstream decoder
/// (a mismatched kind reads as 0 / empty).
pub struct Args<'a, 'b>(pub &'b [Arg<'a>]);

impl<'a> Args<'a, '_> {
    pub fn u(&self, i: usize) -> u32 {
        match self.0.get(i) {
            Some(Arg::S(v)) => *v as u32,
            _ => 0,
        }
    }
    pub fn i(&self, i: usize) -> i32 {
        self.u(i) as i32
    }
    pub fn u64(&self, i: usize) -> u64 {
        match self.0.get(i) {
            Some(Arg::S(v)) => *v,
            _ => 0,
        }
    }
    pub fn f(&self, i: usize) -> f32 {
        f32::from_bits(self.u(i))
    }
    pub fn b(&self, i: usize) -> bool {
        self.u(i) != 0
    }
    pub fn bytes(&self, i: usize) -> &'a [u8] {
        match self.0.get(i) {
            Some(Arg::In(b)) => b,
            _ => &[],
        }
    }
    pub fn out(&self, i: usize) -> usize {
        match self.0.get(i) {
            Some(Arg::Out(n)) => *n as usize,
            _ => 0,
        }
    }
    /// A NUL-terminated (or not) string parameter.
    pub fn str(&self, i: usize) -> &'a [u8] {
        let b = self.bytes(i);
        match b.iter().position(|&c| c == 0) {
            Some(n) => &b[..n],
            None => b,
        }
    }
    /// An "in" array of u32 values.
    pub fn u32s(&self, i: usize) -> Vec<u32> {
        self.bytes(i).as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect()
    }
}

/// Reply under construction: the out buffers (sized by the guest), then the
/// return value.
pub struct Reply {
    pub outs: Vec<Vec<u8>>,
    pub ret: u64,
}

impl Reply {
    /// Out buffers of the sizes the guest asked for, zeroed.
    pub fn new(op: &Op, args: &[Arg<'_>]) -> Self {
        let outs = args
            .iter()
            .filter_map(|a| match a {
                Arg::Out(n) => Some(vec![0u8; *n as usize]),
                _ => None,
            })
            .collect();
        let _ = op;
        Self { outs, ret: 0 }
    }

    /// Out buffer `k` (counting only out parameters).
    pub fn out(&mut self, k: usize) -> &mut [u8] {
        self.outs.get_mut(k).map(|v| v.as_mut_slice()).unwrap_or(&mut [])
    }

    /// Writes u32 values into out buffer `k` (as many as fit).
    pub fn out_u32s(&mut self, k: usize, vals: &[u32]) {
        let b = self.out(k);
        for (c, v) in b.as_chunks_mut::<4>().0.iter_mut().zip(vals) {
            *c = v.to_le_bytes();
        }
    }

    pub fn out_i32s(&mut self, k: usize, vals: &[i32]) {
        let b = self.out(k);
        for (c, v) in b.as_chunks_mut::<4>().0.iter_mut().zip(vals) {
            *c = v.to_le_bytes();
        }
    }

    pub fn out_f32s(&mut self, k: usize, vals: &[f32]) {
        let b = self.out(k);
        for (c, v) in b.as_chunks_mut::<4>().0.iter_mut().zip(vals) {
            *c = v.to_le_bytes();
        }
    }

    /// Copies a string with its NUL into out buffer `k`, cut to fit.
    pub fn out_str(&mut self, k: usize, s: &[u8]) -> usize {
        let b = self.out(k);
        if b.is_empty() {
            return 0;
        }
        let n = s.len().min(b.len() - 1);
        b[..n].copy_from_slice(&s[..n]);
        b[n] = 0;
        n
    }

    /// The bytes sent back.
    pub fn encode(self, op: &Op) -> Vec<u8> {
        let mut v: Vec<u8> = self.outs.into_iter().flatten().collect();
        v.extend_from_slice(&self.ret.to_le_bytes()[..op.ret as usize]);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::{GLES2, GLES2_BASE, RC, RC_BASE, gles2, rc};

    fn packet(op: u32, body: &[u8]) -> Vec<u8> {
        let mut p = op.to_le_bytes().to_vec();
        p.extend_from_slice(&((body.len() + 8) as u32).to_le_bytes());
        p.extend_from_slice(body);
        p
    }

    #[test]
    fn decodes_scalars_in_and_out_pointers() {
        // glTexImage2D(target, level, ifmt, w, h, border, fmt, type, pixels[4])
        let mut body = Vec::new();
        for v in [0x0DE1u32, 0, 0x1908, 1, 1, 0, 0x1908, 0x1401] {
            body.extend_from_slice(&v.to_le_bytes());
        }
        body.extend_from_slice(&4u32.to_le_bytes());
        body.extend_from_slice(&[1, 2, 3, 4]);
        let p = packet(gles2::glTexImage2D, &body);
        let op = &GLES2[(gles2::glTexImage2D - GLES2_BASE) as usize];
        assert_eq!(peek(&p), Some((gles2::glTexImage2D, p.len())));
        let mut args = Vec::new();
        decode(op, &p, &mut args).unwrap();
        let a = Args(&args);
        assert_eq!((a.u(0), a.u(2), a.u(7)), (0x0DE1, 0x1908, 0x1401));
        assert_eq!(a.bytes(8), [1, 2, 3, 4]);
        assert!(decode(op, &p[..p.len() - 1], &mut args).is_none(), "truncated");

        // glIsEnabled: a 1-byte return value.
        let op = &GLES2[(gles2::glIsEnabled - GLES2_BASE) as usize];
        assert_eq!(op.ret, 1);

        // rcChooseConfig(attribs in, attribs_size, configs out, configs_size) -> EGLint
        let mut body = Vec::new();
        body.extend_from_slice(&8u32.to_le_bytes());
        body.extend_from_slice(&[0x24, 0x30, 0, 0, 8, 0, 0, 0]);
        body.extend_from_slice(&8u32.to_le_bytes());
        body.extend_from_slice(&12u32.to_le_bytes());
        body.extend_from_slice(&3u32.to_le_bytes());
        let p = packet(rc::rcChooseConfig, &body);
        let op = &RC[(rc::rcChooseConfig - RC_BASE) as usize];
        decode(op, &p, &mut args).unwrap();
        let a = Args(&args);
        assert_eq!(a.u32s(0), [0x3024, 8]);
        assert_eq!((a.out(2), a.u(3)), (12, 3));
        let mut r = Reply::new(op, &args);
        r.out_u32s(0, &[7, 8, 9]);
        r.ret = 3;
        assert_eq!(r.encode(op), [7, 0, 0, 0, 8, 0, 0, 0, 9, 0, 0, 0, 3, 0, 0, 0]);
    }
}
