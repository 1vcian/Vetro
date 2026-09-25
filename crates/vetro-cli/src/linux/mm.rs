//! Memoria di un processo: brk, mmap, munmap, mprotect, mremap, madvise.

use super::abi::*;
use std::cell::RefCell;
use std::rc::Rc;
use vetro_cpu::{Perm, UserMemory};

pub const PAGE: u64 = 0x1000;
pub const STACK_TOP: u64 = 0x0000_7fff_ffff_0000;
pub const STACK_SIZE: u64 = 8 << 20;
/// Le mmap senza indirizzo vanno dall'alto verso il basso sotto questa soglia.
pub const MMAP_TOP: u64 = 0x0000_7fff_0000_0000;
pub const MMAP_BOTTOM: u64 = 0x0000_0010_0000_0000;

/// Tetti di memoria: oltre, ENOMEM come su una macchina con memoria finita
/// (e l'emulatore non tenta allocazioni che lo farebbero abortire).
pub const MAX_MAPPING: u64 = 8 << 30;
pub const MAX_TOTAL: u64 = 16 << 30;

pub const MAP_SHARED: u64 = 0x01;
pub const MAP_PRIVATE: u64 = 0x02;
pub const MAP_FIXED: u64 = 0x10;
pub const MAP_ANONYMOUS: u64 = 0x20;
pub const MAP_GROWSDOWN: u64 = 0x100;
pub const MAP_POPULATE: u64 = 0x8000;

/// Memoria di una MAP_SHARED: buffer, offset e se può diventare scrivibile.
pub type SharedMap = (Rc<RefCell<Vec<u8>>>, usize, bool);
pub const MAP_FIXED_NOREPLACE: u64 = 0x100000;

#[derive(Clone)]
pub struct Mm {
    pub mem: UserMemory,
    pub brk_start: u64,
    pub brk: u64,
}

/// Come [`page_up`], ma `None` se l'arrotondamento trabocca.
fn checked_page_up(v: u64) -> Option<u64> {
    v.checked_add(PAGE - 1).map(|x| x & !(PAGE - 1))
}

fn page_up(x: u64) -> u64 {
    x.wrapping_add(PAGE - 1) & !(PAGE - 1)
}

impl Mm {
    pub fn new(mem: UserMemory, brk: u64) -> Self {
        Mm { mem, brk_start: brk, brk }
    }

    /// Vero se si possono mappare altri `extra` byte.
    fn fits(&self, extra: u64) -> bool {
        let total: u64 = self.mem.ranges().map(|(a, b, _)| b - a).sum();
        extra <= MAX_MAPPING && total.saturating_add(extra) <= MAX_TOTAL
    }

    pub fn sys_brk(&mut self, addr: u64) -> i64 {
        if addr < self.brk_start {
            return self.brk as i64;
        }
        let old_end = page_up(self.brk);
        let new_end = page_up(addr);
        if new_end > old_end {
            if !self.fits(new_end - old_end) || self.mem.ranges().any(|(s, e, _)| s < new_end && e > old_end)
            {
                return self.brk as i64;
            }
            self.mem.map_zeroed(old_end, (new_end - old_end) as usize, Perm::RW);
        } else if new_end < old_end {
            self.mem.unmap(new_end, old_end);
        }
        self.brk = addr;
        addr as i64
    }

    /// mmap anonima o con contenuto già letto (`data`, per le mappature di
    /// file private), oppure condivisa su `shared` (buffer e offset).
    pub fn mmap(
        &mut self,
        addr: u64,
        len: u64,
        prot: u64,
        flags: u64,
        data: Option<Vec<u8>>,
        shared: Option<SharedMap>,
    ) -> SysResult {
        if len == 0 || addr & (PAGE - 1) != 0 && flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0 {
            return Err(EINVAL);
        }
        if flags & (MAP_SHARED | MAP_PRIVATE) == 0 {
            return Err(EINVAL);
        }
        let size = page_up(len);
        if size == 0 || !self.fits(size) {
            return Err(ENOMEM);
        }
        // TASK_SIZE con VA a 48 bit: oltre non si mappa.
        const TASK_SIZE: u64 = 1 << 48;
        if flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0
            && addr.checked_add(size).is_none_or(|e| e > TASK_SIZE)
        {
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
        if let Some((buf, off, may_write)) = shared {
            self.mem.map_shared(base, buf, off, size as usize, Perm::from_prot(prot as u32), may_write);
            return Ok(base as i64);
        }
        match data {
            None => self.mem.map_zeroed(base, size as usize, Perm::from_prot(prot as u32)),
            Some(d) => {
                let mut bytes = vec![0u8; size as usize];
                let n = d.len().min(bytes.len());
                bytes[..n].copy_from_slice(&d[..n]);
                self.mem.map_fixed(base, bytes, Perm::from_prot(prot as u32));
            }
        }
        if flags & MAP_GROWSDOWN != 0 {
            self.mem.set_grows_down(base);
        }
        if flags & MAP_POPULATE != 0 {
            self.mem.populate(base, base + size);
        }
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
        let (end, perm) = (addr + page_up(len), Perm::from_prot(prot as u32));
        if !self.mem.is_mapped(addr, end) {
            return Err(ENOMEM);
        }
        if perm.write && !self.mem.may_write(addr, end) {
            return Err(EACCES);
        }
        self.mem.protect(addr, end, perm).map_err(|_| ENOMEM)?;
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
        const MREMAP_DONTUNMAP: u64 = 4;
        /// TASK_SIZE con VA a 48 bit.
        const TASK_SIZE: u64 = 1 << 48;
        let dontunmap = flags & MREMAP_DONTUNMAP != 0;
        // Controlli di sys_mremap, nello stesso ordine.
        if flags & !(MREMAP_MAYMOVE | MREMAP_FIXED | MREMAP_DONTUNMAP) != 0
            || old & (PAGE - 1) != 0
            || flags & (MREMAP_FIXED | MREMAP_DONTUNMAP) != 0 && flags & MREMAP_MAYMOVE == 0
            || dontunmap && old_len != new_len
        {
            return Err(EINVAL);
        }
        let (Some(old_size), Some(new_size)) = (checked_page_up(old_len), checked_page_up(new_len)) else {
            return Err(EINVAL);
        };
        if new_size == 0 {
            return Err(EINVAL);
        }
        let old_end = old.checked_add(old_size).filter(|&e| e <= TASK_SIZE).ok_or(EINVAL)?;
        if flags & MREMAP_FIXED != 0 {
            let new_end = new_addr.checked_add(new_size).filter(|&e| e <= TASK_SIZE).ok_or(EINVAL)?;
            if new_addr & (PAGE - 1) != 0 || new_addr < old_end && old < new_end {
                return Err(EINVAL);
            }
        }
        if new_size > old_size && !self.fits(new_size - old_size) {
            return Err(ENOMEM);
        }
        if !self.mem.is_mapped(old, old_end) {
            return Err(EFAULT);
        }
        if flags & MREMAP_FIXED == 0 && !dontunmap {
            if new_size <= old_size {
                self.mem.unmap(old + new_size, old_end);
                return Ok(old as i64);
            }
            let tail = old_end;
            if !self.mem.ranges().any(|(s, e, _)| s < old + new_size && e > tail)
                && old + new_size <= MMAP_TOP
            {
                self.mem.extend(tail, (new_size - old_size) as usize);
                return Ok(old as i64);
            }
            if flags & MREMAP_MAYMOVE == 0 {
                return Err(ENOMEM);
            }
        }
        let dst = if flags & MREMAP_FIXED != 0 {
            new_addr
        } else {
            self.mem.find_free(new_size, MMAP_BOTTOM, MMAP_TOP).ok_or(ENOMEM)?
        };
        // Le pagine si spostano con la loro memoria (anche condivisa).
        let keep = old_size.min(new_size);
        if dontunmap {
            // Il vecchio intervallo resta mappato: vuoto se privato, sulla
            // stessa memoria se condiviso (Linux >= 5.13).
            self.mem.remap_dontunmap(old, keep, dst);
            return Ok(dst as i64);
        }
        self.mem.remap(old, keep, dst);
        self.mem.unmap(old, old_end);
        self.mem.extend(dst + keep, (new_size - keep) as usize);
        Ok(dst as i64)
    }
}
