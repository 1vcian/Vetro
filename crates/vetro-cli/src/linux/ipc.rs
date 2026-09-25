//! IPC System V: memoria condivisa (shmget, shmat, shmdt, shmctl).

use std::cell::RefCell;
use std::rc::Rc;

use super::abi::*;
use super::mm::Mm;
use super::{Kernel, Pid};
use vetro_cpu::Perm;

const IPC_PRIVATE: i32 = 0;
const IPC_CREAT: u64 = 0o1000;
const IPC_EXCL: u64 = 0o2000;
const IPC_RMID: u64 = 0;
const IPC_SET: u64 = 1;
const IPC_STAT: u64 = 2;
const SHM_RDONLY: u64 = 0o10000;
const SHM_RND: u64 = 0o20000;
const SHMMAX: u64 = 1 << 32;

pub struct Segment {
    key: i32,
    size: usize,
    buf: Rc<RefCell<Vec<u8>>>,
    mode: u32,
    nattch: u64,
    removed: bool,
    cpid: Pid,
    lpid: Pid,
}

#[derive(Default)]
pub struct Ipc {
    segs: Vec<Option<Segment>>,
    /// Attacchi: (spazio d'indirizzamento, indirizzo, id).
    attached: Vec<(Rc<RefCell<Mm>>, u64, usize)>,
}

fn page_up(x: u64) -> u64 {
    (x + 0xfff) & !0xfff
}

impl Kernel {
    pub(super) fn sys_shmget(&mut self, key: i32, size: u64, flags: u64) -> SysResult {
        if key != IPC_PRIVATE
            && let Some(id) =
                self.ipc.segs.iter().position(|s| s.as_ref().is_some_and(|s| s.key == key && !s.removed))
        {
            if flags & IPC_CREAT != 0 && flags & IPC_EXCL != 0 {
                return Err(EEXIST);
            }
            if size > self.ipc.segs[id].as_ref().unwrap().size as u64 {
                return Err(EINVAL);
            }
            return Ok(id as i64);
        }
        if key != IPC_PRIVATE && flags & IPC_CREAT == 0 {
            return Err(ENOENT);
        }
        if size == 0 || size > SHMMAX {
            return Err(EINVAL);
        }
        let seg = Segment {
            key,
            size: size as usize,
            buf: Rc::new(RefCell::new(vec![0; page_up(size) as usize])),
            mode: (flags & 0o777) as u32,
            nattch: 0,
            removed: false,
            cpid: 0,
            lpid: 0,
        };
        self.ipc.segs.push(Some(seg));
        Ok(self.ipc.segs.len() as i64 - 1)
    }

    pub(super) fn sys_shmat(&mut self, t: usize, id: u64, addr: u64, flags: u64) -> SysResult {
        let seg = self.ipc.segs.get_mut(id as usize).and_then(|s| s.as_mut()).ok_or(EINVAL)?;
        let len = page_up(seg.size as u64);
        let addr = if flags & SHM_RND != 0 { addr & !0xfff } else { addr };
        if addr & 0xfff != 0 {
            return Err(EINVAL);
        }
        let perm = if flags & SHM_RDONLY != 0 { Perm::R } else { Perm::RW };
        let buf = seg.buf.clone();
        seg.nattch += 1;
        seg.lpid = self.tasks[t].tgid;
        let mm = self.tasks[t].mm.clone();
        let base = {
            let mut m = mm.borrow_mut();
            let base = if addr == 0 {
                m.mem.find_free(len, super::mm::MMAP_BOTTOM, super::mm::MMAP_TOP).ok_or(ENOMEM)?
            } else {
                addr
            };
            m.mem.map_shared(base, buf, 0, len as usize, perm, true);
            base
        };
        self.ipc.attached.push((mm, base, id as usize));
        Ok(base as i64)
    }

    pub(super) fn sys_shmdt(&mut self, t: usize, addr: u64) -> SysResult {
        let mm = self.tasks[t].mm.clone();
        let i =
            self.ipc.attached.iter().position(|(m, a, _)| Rc::ptr_eq(m, &mm) && *a == addr).ok_or(EINVAL)?;
        let (_, _, id) = self.ipc.attached.remove(i);
        let pid = self.tasks[t].tgid;
        if let Some(Some(seg)) = self.ipc.segs.get_mut(id) {
            mm.borrow_mut().mem.unmap(addr, addr + page_up(seg.size as u64));
            seg.nattch = seg.nattch.saturating_sub(1);
            seg.lpid = pid;
            if seg.removed && seg.nattch == 0 {
                self.ipc.segs[id] = None;
            }
        }
        Ok(0)
    }

    pub(super) fn sys_shmctl(&mut self, t: usize, id: u64, cmd: u64, buf: u64) -> SysResult {
        let cmd = cmd & 0xff; // IPC_64 ignorato: il layout è sempre quello a 64 bit
        let seg = self.ipc.segs.get_mut(id as usize).and_then(|s| s.as_mut()).ok_or(EINVAL)?;
        match cmd {
            IPC_RMID => {
                seg.removed = true;
                if seg.nattch == 0 {
                    self.ipc.segs[id as usize] = None;
                }
                Ok(0)
            }
            IPC_STAT => {
                let mut b = [0u8; 112];
                // SAFETY: getuid/getgid non hanno precondizioni.
                let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
                b[0..4].copy_from_slice(&seg.key.to_le_bytes());
                for (off, v) in [(4, uid), (8, gid), (12, uid), (16, gid), (20, seg.mode)] {
                    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
                }
                b[48..56].copy_from_slice(&(seg.size as u64).to_le_bytes());
                b[80..84].copy_from_slice(&seg.cpid.to_le_bytes());
                b[84..88].copy_from_slice(&seg.lpid.to_le_bytes());
                b[88..96].copy_from_slice(&seg.nattch.to_le_bytes());
                write_bytes(&mut self.tasks[t].mm.borrow_mut().mem, buf, &b)?;
                Ok(0)
            }
            IPC_SET => {
                let mode = read_u32(&mut self.tasks[t].mm.borrow_mut().mem, buf + 20)?;
                let seg = self.ipc.segs[id as usize].as_mut().unwrap();
                seg.mode = (seg.mode & !0o777) | (mode & 0o777);
                Ok(0)
            }
            _ => Err(EINVAL),
        }
    }
}
