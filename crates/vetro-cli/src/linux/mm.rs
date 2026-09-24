//! Memoria di un processo: brk, mmap, munmap, mprotect, mremap, madvise.

use super::abi::*;
use vetro_cpu::{Memory, Perm, UserMemory};

pub const PAGE: u64 = 0x1000;
pub const STACK_TOP: u64 = 0x0000_7fff_ffff_0000;
pub const STACK_SIZE: u64 = 8 << 20;
/// Le mmap senza indirizzo vanno dall'alto verso il basso sotto questa soglia.
pub const MMAP_TOP: u64 = 0x0000_7fff_0000_0000;
pub const MMAP_BOTTOM: u64 = 0x0000_0010_0000_0000;

pub const MAP_SHARED: u64 = 0x01;
pub const MAP_PRIVATE: u64 = 0x02;
pub const MAP_FIXED: u64 = 0x10;
pub const MAP_ANONYMOUS: u64 = 0x20;
pub const MAP_FIXED_NOREPLACE: u64 = 0x100000;

#[derive(Clone)]
pub struct Mm {
    pub mem: UserMemory,
    pub brk_start: u64,
    pub brk: u64,
}

fn page_up(x: u64) -> u64 {
    x.wrapping_add(PAGE - 1) & !(PAGE - 1)
}

impl Mm {
    pub fn new(mem: UserMemory, brk: u64) -> Self {
        Mm { mem, brk_start: brk, brk }
    }

    pub fn sys_brk(&mut self, addr: u64) -> i64 {
        if addr < self.brk_start {
            return self.brk as i64;
        }
        let old_end = page_up(self.brk);
        let new_end = page_up(addr);
        if new_end > old_end {
            if self.mem.ranges().any(|(s, e, _)| s < new_end && e > old_end) {
                return self.brk as i64;
            }
            self.mem.map_fixed(old_end, vec![0; (new_end - old_end) as usize], Perm::RW);
        } else if new_end < old_end {
            self.mem.unmap(new_end, old_end);
        }
        self.brk = addr;
        addr as i64
    }

    /// mmap anonima o con contenuto già letto (`data`, per le mappature di
    /// file private).
    pub fn mmap(&mut self, addr: u64, len: u64, prot: u64, flags: u64, data: Option<Vec<u8>>) -> SysResult {
        if len == 0 || addr & (PAGE - 1) != 0 && flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0 {
            return Err(EINVAL);
        }
        if flags & (MAP_SHARED | MAP_PRIVATE) == 0 {
            return Err(EINVAL);
        }
        let size = page_up(len);
        if size == 0 {
            return Err(ENOMEM);
        }
        let base = if flags & MAP_FIXED != 0 {
            addr
        } else if flags & MAP_FIXED_NOREPLACE != 0 {
            if self.mem.is_mapped(addr, addr + size)
                || self.mem.ranges().any(|(s, e, _)| s < addr + size && e > addr)
            {
                return Err(EEXIST);
            }
            addr
        } else {
            let hint = addr & !(PAGE - 1);
            let free = |m: &UserMemory, a: u64| a != 0 && !m.ranges().any(|(s, e, _)| s < a + size && e > a);
            if free(&self.mem, hint) && hint >= MMAP_BOTTOM && hint + size <= MMAP_TOP {
                hint
            } else {
                self.mem.find_free(size, MMAP_BOTTOM, MMAP_TOP).ok_or(ENOMEM)?
            }
        };
        let mut bytes = vec![0u8; size as usize];
        if let Some(d) = data {
            let n = d.len().min(bytes.len());
            bytes[..n].copy_from_slice(&d[..n]);
        }
        self.mem.map_fixed(base, bytes, Perm::from_prot(prot as u32));
        Ok(base as i64)
    }

    pub fn munmap(&mut self, addr: u64, len: u64) -> SysResult {
        if addr & (PAGE - 1) != 0 || len == 0 {
            return Err(EINVAL);
        }
        self.mem.unmap(addr, addr + page_up(len));
        Ok(0)
    }

    pub fn mprotect(&mut self, addr: u64, len: u64, prot: u64) -> SysResult {
        if addr & (PAGE - 1) != 0 {
            return Err(EINVAL);
        }
        if len == 0 {
            return Ok(0);
        }
        self.mem.protect(addr, addr + page_up(len), Perm::from_prot(prot as u32)).map_err(|_| ENOMEM)?;
        Ok(0)
    }

    pub fn madvise(&mut self, addr: u64, len: u64, advice: u64) -> SysResult {
        const MADV_DONTNEED: u64 = 4;
        if addr & (PAGE - 1) != 0 {
            return Err(EINVAL);
        }
        let end = addr + page_up(len);
        if !self.mem.is_mapped(addr, end) {
            return Err(ENOMEM);
        }
        if advice == MADV_DONTNEED {
            // Le pagine anonime private tornano a zero.
            let zeros = vec![0u8; (end - addr) as usize];
            self.mem.poke(addr, &zeros).map_err(|_| ENOMEM)?;
        }
        Ok(0)
    }

    pub fn mremap(&mut self, old: u64, old_len: u64, new_len: u64, flags: u64, new_addr: u64) -> SysResult {
        const MREMAP_MAYMOVE: u64 = 1;
        const MREMAP_FIXED: u64 = 2;
        if old & (PAGE - 1) != 0 || new_len == 0 {
            return Err(EINVAL);
        }
        let (old_size, new_size) = (page_up(old_len), page_up(new_len));
        if !self.mem.is_mapped(old, old + old_size) {
            return Err(EFAULT);
        }
        let perm = self.mem.perm_at(old).unwrap_or(Perm::RW);
        if flags & MREMAP_FIXED == 0 {
            if new_size <= old_size {
                self.mem.unmap(old + new_size, old + old_size);
                return Ok(old as i64);
            }
            let tail = old + old_size;
            if !self.mem.ranges().any(|(s, e, _)| s < old + new_size && e > tail)
                && old + new_size <= MMAP_TOP
            {
                self.mem.map_fixed(tail, vec![0; (new_size - old_size) as usize], perm);
                return Ok(old as i64);
            }
            if flags & MREMAP_MAYMOVE == 0 {
                return Err(ENOMEM);
            }
        }
        let mut data = vec![0u8; old_size.min(new_size) as usize];
        self.mem.read(old, &mut data).map_err(|_| EFAULT)?;
        let dst = if flags & MREMAP_FIXED != 0 {
            new_addr
        } else {
            self.mem.find_free(new_size, MMAP_BOTTOM, MMAP_TOP).ok_or(ENOMEM)?
        };
        self.mem.unmap(old, old + old_size);
        let mut bytes = vec![0u8; new_size as usize];
        bytes[..data.len()].copy_from_slice(&data);
        self.mem.map_fixed(dst, bytes, perm);
        Ok(dst as i64)
    }
}
