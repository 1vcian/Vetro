//! Il kernel Linux del guest visto dall'esterno: processi, thread, mappe
//! di memoria, file aperti, riga di comando, pagine dei file nella page
//! cache. Si legge solo la memoria fisica (con le tabelle delle pagine del
//! guest): il guest non se ne accorge.
//!
//! Serve un [`Kernel`]: i simboli (`System.map` o kallsyms) e la
//! disposizione delle strutture (BTF). Lo spostamento KASLR si ricava da
//! VBAR_EL1, che il kernel punta a `vectors`.

use std::cell::Cell;

use super::btf::Btf;
use super::kallsyms::Symbols;
use super::layout::{Layout, MissingField};
use super::mem::{PhysMem, Space};

/// Flag di `task_struct.flags`: thread del kernel.
pub const PF_KTHREAD: u32 = 0x0020_0000;

/// Numeri magici dei file system speciali (`include/uapi/linux/magic.h`).
pub const SOCKFS_MAGIC: u64 = 0x534f_434b;
pub const PIPEFS_MAGIC: u64 = 0x5049_5045;
pub const ANON_INODE_FS_MAGIC: u64 = 0x0904_1934;

/// Limite dei cicli sulle liste (una lista rovinata non blocca).
const LIST_LIMIT: usize = 1 << 17;

/// Il profilo di un kernel: simboli e disposizione delle strutture.
#[derive(Clone, Debug)]
pub struct Kernel {
    pub syms: Symbols,
    pub layout: Layout,
    /// Valori di `enum maple_type` (dense, leaf_64, range_64, arange_64).
    pub maple_types: [u64; 4],
}

impl Kernel {
    pub fn new(syms: Symbols, btf: &Btf) -> Result<Kernel, MissingField> {
        let layout = Layout::from_btf(btf)?;
        let mut maple_types = [0, 1, 2, 3];
        for (i, n) in ["maple_dense", "maple_leaf_64", "maple_range_64", "maple_arange_64"].iter().enumerate()
        {
            if let Some(v) = btf.enum_value(n) {
                maple_types[i] = v as u64;
            }
        }
        Ok(Kernel { syms, layout, maple_types })
    }
}

/// I registri della CPU che servono a leggere il kernel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuRegs {
    pub tcr: u64,
    pub ttbr0: u64,
    pub ttbr1: u64,
    pub vbar: u64,
    /// Offset per CPU (`__per_cpu_offset` della CPU corrente).
    pub tpidr_el1: u64,
}

/// Un processo o thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    /// Indirizzo della `task_struct`.
    pub addr: u64,
    pub pid: i32,
    pub tgid: i32,
    pub comm: String,
    pub flags: u32,
    pub uid: u32,
    pub euid: u32,
    pub gid: u32,
    /// `real_parent->tgid`.
    pub ppid: i32,
    /// `mm` (0 per i thread del kernel).
    pub mm: u64,
    pub exit_state: u32,
}

impl Task {
    pub fn is_kernel_thread(&self) -> bool {
        self.flags & PF_KTHREAD != 0
    }
}

/// Una regione di memoria (`vm_area_struct`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vma {
    pub addr: u64,
    pub start: u64,
    pub end: u64,
    pub flags: u64,
    pub pgoff: u64,
    /// `struct file *` mappato (0 = anonima).
    pub file: u64,
    /// Nome come in `/proc/<pid>/maps` (percorso, `[heap]`, `[stack]`,
    /// `[vdso]`, `[anon:...]`), vuoto se non c'è.
    pub name: String,
    /// Dispositivo (`s_dev`) e inode del file.
    pub dev: u32,
    pub ino: u64,
}

pub const VM_READ: u64 = 1;
pub const VM_WRITE: u64 = 2;
pub const VM_EXEC: u64 = 4;
pub const VM_MAYSHARE: u64 = 0x80;

impl Vma {
    /// La riga di `/proc/<pid>/maps` (senza `\n`), con lo stesso
    /// allineamento del kernel (`show_map_vma`).
    pub fn maps_line(&self) -> String {
        let f = self.flags;
        let mut s = format!(
            "{:08x}-{:08x} {}{}{}{} {:08x} {:02x}:{:02x} {} ",
            self.start,
            self.end,
            if f & VM_READ != 0 { 'r' } else { '-' },
            if f & VM_WRITE != 0 { 'w' } else { '-' },
            if f & VM_EXEC != 0 { 'x' } else { '-' },
            if f & VM_MAYSHARE != 0 { 's' } else { 'p' },
            if self.file != 0 { self.pgoff << 12 } else { 0 },
            self.dev >> 20,
            self.dev & 0xf_ffff,
            self.ino,
        );
        if !self.name.is_empty() {
            while s.len() < 72 {
                s.push(' ');
            }
            s.push(' ');
            s.push_str(&self.name);
        }
        s
    }
}

/// Un file aperto.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenFile {
    pub fd: u32,
    pub file: u64,
    pub path: String,
    pub flags: u32,
    pub pos: i64,
    pub inode: u64,
    pub ino: u64,
}

/// La vista del kernel su una memoria fisica.
pub struct Linux<'a, M: PhysMem + ?Sized> {
    pub mem: &'a M,
    pub kernel: &'a Kernel,
    /// Spazio del kernel (TTBR1) con il TTBR0 del momento.
    pub space: Space,
    /// Spostamento KASLR (0 senza).
    pub slide: u64,
    /// Indirizzo virtuale di `vmemmap` (calcolato alla prima richiesta).
    vmemmap: Cell<Option<u64>>,
}

impl<'a, M: PhysMem + ?Sized> Linux<'a, M> {
    pub fn new(mem: &'a M, kernel: &'a Kernel, regs: &CpuRegs) -> Self {
        let slide = match kernel.syms.get("vectors") {
            Some(v) if regs.vbar != 0 && regs.vbar.wrapping_sub(v) % 4096 == 0 => regs.vbar.wrapping_sub(v),
            _ => 0,
        };
        Linux {
            mem,
            kernel,
            space: Space { tcr: regs.tcr, ttbr0: regs.ttbr0, ttbr1: regs.ttbr1 },
            slide,
            vmemmap: Cell::new(None),
        }
    }

    fn l(&self) -> &Layout {
        &self.kernel.layout
    }

    /// Indirizzo di un simbolo, con lo spostamento KASLR.
    pub fn sym(&self, name: &str) -> Option<u64> {
        self.kernel.syms.get(name).map(|a| a.wrapping_add(self.slide))
    }

    pub fn read(&self, va: u64, buf: &mut [u8]) -> bool {
        self.space.read(self.mem, va, buf)
    }

    pub fn u64(&self, va: u64) -> Option<u64> {
        self.space.u64(self.mem, va)
    }

    pub fn u32(&self, va: u64) -> Option<u32> {
        self.space.u32(self.mem, va)
    }

    pub fn i32(&self, va: u64) -> Option<i32> {
        self.u32(va).map(|v| v as i32)
    }

    /// Stringa C del kernel.
    pub fn cstr(&self, va: u64, max: usize) -> Option<String> {
        self.space.cstr(self.mem, va, max).map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    /// Indirizzo fisico di un indirizzo virtuale del kernel.
    pub fn phys(&self, va: u64) -> Option<u64> {
        self.space.translate(self.mem, va)
    }

    /// Nodi di una `list_head` (escluso `head`), come indirizzi della
    /// struttura che contiene il nodo a `off`.
    pub fn list(&self, head: u64, off: u64) -> Vec<u64> {
        let mut out = Vec::new();
        let Some(mut node) = self.u64(head) else { return out };
        while node != head && node != 0 && out.len() < LIST_LIMIT {
            out.push(node.wrapping_sub(off));
            match self.u64(node) {
                Some(n) => node = n,
                None => break,
            }
        }
        out
    }

    /// La `task_struct` che gira sulla CPU (`__entry_task` per CPU).
    pub fn current(&self, tpidr_el1: u64) -> Option<u64> {
        self.u64(self.sym("__entry_task")?.wrapping_add(tpidr_el1))
    }

    /// Legge una `task_struct`.
    pub fn task(&self, addr: u64) -> Option<Task> {
        let l = self.l();
        let mut comm = vec![0u8; l.task_comm_len as usize];
        if !self.read(addr.wrapping_add(l.task_comm), &mut comm) {
            return None;
        }
        let end = comm.iter().position(|&c| c == 0).unwrap_or(comm.len());
        let cred = self.u64(addr.wrapping_add(l.task_real_cred))?;
        let ecred = self.u64(addr.wrapping_add(l.task_cred))?;
        let parent = self.u64(addr.wrapping_add(l.task_real_parent))?;
        Some(Task {
            addr,
            pid: self.i32(addr.wrapping_add(l.task_pid))?,
            tgid: self.i32(addr.wrapping_add(l.task_tgid))?,
            comm: String::from_utf8_lossy(&comm[..end]).into_owned(),
            flags: self.u32(addr.wrapping_add(l.task_flags))?,
            uid: self.u32(cred.wrapping_add(l.cred_uid)).unwrap_or(u32::MAX),
            euid: self.u32(ecred.wrapping_add(l.cred_euid)).unwrap_or(u32::MAX),
            gid: self.u32(cred.wrapping_add(l.cred_gid)).unwrap_or(u32::MAX),
            ppid: self.i32(parent.wrapping_add(l.task_tgid)).unwrap_or(-1),
            mm: self.u64(addr.wrapping_add(l.task_mm))?,
            exit_state: self.u32(addr.wrapping_add(l.task_exit_state))?,
        })
    }

    /// `init_task` (pid 0, `swapper/0`).
    pub fn init_task(&self) -> Option<u64> {
        self.sym("init_task")
    }

    /// I processi (capi dei gruppi di thread) in ordine di lista, senza
    /// `swapper` (pid 0): gli stessi di `/proc`.
    pub fn processes(&self) -> Vec<Task> {
        let Some(init) = self.init_task() else { return Vec::new() };
        self.list(init.wrapping_add(self.l().task_tasks), self.l().task_tasks)
            .into_iter()
            .filter_map(|a| self.task(a))
            .filter(|t| t.pid != 0)
            .collect()
    }

    /// I thread del gruppo di `leader` (lui compreso).
    pub fn threads(&self, leader: u64) -> Vec<Task> {
        let l = self.l();
        let Some(signal) = self.u64(leader.wrapping_add(l.task_signal)) else { return Vec::new() };
        self.list(signal.wrapping_add(l.signal_thread_head), l.task_thread_node)
            .into_iter()
            .filter_map(|a| self.task(a))
            .collect()
    }

    /// Tutti i thread di tutti i processi.
    pub fn all_threads(&self) -> Vec<Task> {
        self.processes().iter().flat_map(|p| self.threads(p.addr)).collect()
    }

    pub fn find_pid(&self, pid: i32) -> Option<Task> {
        self.all_threads().into_iter().find(|t| t.pid == pid)
    }

    /// Lo spazio d'indirizzi utente di `mm` (TTBR0 = `mm->pgd`).
    pub fn user_space(&self, mm: u64) -> Option<Space> {
        let pgd = self.u64(mm.wrapping_add(self.l().mm_pgd))?;
        Some(self.space.with_user(self.phys(pgd)?))
    }

    /// Legge la memoria utente di `mm`.
    pub fn read_user(&self, mm: u64, va: u64, buf: &mut [u8]) -> bool {
        self.user_space(mm).is_some_and(|s| s.read(self.mem, va, buf))
    }

    /// La riga di comando (argomenti separati da zeri, come
    /// `/proc/<pid>/cmdline`), vuota per i thread del kernel.
    pub fn cmdline(&self, t: &Task) -> Option<Vec<u8>> {
        if t.mm == 0 {
            return Some(Vec::new());
        }
        let l = self.l();
        let start = self.u64(t.mm.wrapping_add(l.mm_arg_start))?;
        let end = self.u64(t.mm.wrapping_add(l.mm_arg_end))?;
        let len = end.checked_sub(start)?.min(1 << 16) as usize;
        let mut b = vec![0u8; len];
        self.read_user(t.mm, start, &mut b).then_some(b)
    }

    /// Voci (valori non nulli) di un maple tree dalla radice `root`.
    pub fn maple_entries(&self, root: u64) -> Vec<u64> {
        let mut out = Vec::new();
        let is_node = |e: u64| e & 3 == 2 && e > 4096;
        if is_node(root) {
            self.maple_walk(root, 0, u64::MAX, 0, &mut out);
        } else if root != 0 && root & 3 == 0 {
            out.push(root);
        }
        out
    }

    fn maple_walk(&self, enode: u64, min: u64, max: u64, depth: u32, out: &mut Vec<u64>) {
        if depth > 16 || out.len() > LIST_LIMIT {
            return;
        }
        let l = self.l();
        let node = enode & !0xff;
        let ty = enode >> 3 & 0xf;
        let [_, leaf, range, arange] = self.kernel.maple_types;
        let (pivot, slot, n) = if ty == leaf || ty == range {
            (l.mr64_pivot, l.mr64_slot, l.mr64_slots)
        } else if ty == arange {
            (l.ma64_pivot, l.ma64_slot, l.ma64_slots)
        } else {
            return;
        };
        let mut lo = min;
        for i in 0..n {
            let piv = if i + 1 < n { self.u64(node.wrapping_add(pivot + 8 * i)).unwrap_or(0) } else { max };
            if i > 0 && piv == 0 {
                break;
            }
            let e = self.u64(node.wrapping_add(slot + 8 * i)).unwrap_or(0);
            if e != 0 {
                if ty == leaf {
                    out.push(e);
                } else if e & 3 == 2 && e > 4096 {
                    self.maple_walk(e, lo, piv, depth + 1, out);
                }
            }
            if piv >= max {
                break;
            }
            lo = piv.wrapping_add(1);
        }
    }

    /// Le regioni di memoria di `mm`, in ordine di indirizzo.
    pub fn vmas(&self, mm: u64) -> Vec<Vma> {
        if mm == 0 {
            return Vec::new();
        }
        let l = self.l();
        let Some(root) = self.u64(mm.wrapping_add(l.mm_mt_root)) else { return Vec::new() };
        let brk = self.u64(mm.wrapping_add(l.mm_brk)).unwrap_or(0);
        let start_brk = self.u64(mm.wrapping_add(l.mm_start_brk)).unwrap_or(0);
        let start_stack = self.u64(mm.wrapping_add(l.mm_start_stack)).unwrap_or(0);
        let special = [self.sym("special_mapping_vmops"), self.sym("legacy_special_mapping_vmops")];
        let mut out: Vec<Vma> = self
            .maple_entries(root)
            .into_iter()
            .filter_map(|v| {
                let start = self.u64(v.wrapping_add(l.vma_start))?;
                let end = self.u64(v.wrapping_add(l.vma_end))?;
                if start >= end || self.u64(v.wrapping_add(l.vma_mm))? != mm {
                    return None;
                }
                let file = self.u64(v.wrapping_add(l.vma_file))?;
                let mut vma = Vma {
                    addr: v,
                    start,
                    end,
                    flags: self.u64(v.wrapping_add(l.vma_flags))?,
                    pgoff: self.u64(v.wrapping_add(l.vma_pgoff))?,
                    file,
                    name: String::new(),
                    dev: 0,
                    ino: 0,
                };
                if file != 0 {
                    let inode = self.u64(file.wrapping_add(l.file_inode)).unwrap_or(0);
                    vma.ino = self.u64(inode.wrapping_add(l.inode_ino)).unwrap_or(0);
                    let sb = self.u64(inode.wrapping_add(l.inode_sb)).unwrap_or(0);
                    vma.dev = self.u32(sb.wrapping_add(l.sb_dev)).unwrap_or(0);
                    vma.name = self.file_path(file);
                } else {
                    let ops = self.u64(v.wrapping_add(l.vma_ops)).unwrap_or(0);
                    if ops != 0 && special.contains(&Some(ops)) {
                        let sm = self.u64(v.wrapping_add(l.vma_private_data)).unwrap_or(0);
                        if let Some(n) = self.u64(sm.wrapping_add(l.special_mapping_name)) {
                            vma.name = self.cstr(n, 64).unwrap_or_default();
                        }
                    } else if start <= brk && end >= start_brk {
                        vma.name = "[heap]".into();
                    } else if start <= start_stack && end >= start_stack {
                        vma.name = "[stack]".into();
                    } else if let (Some(an), Some(nn)) = (l.vma_anon_name, l.anon_name_name) {
                        let p = self.u64(v.wrapping_add(an)).unwrap_or(0);
                        if p != 0
                            && let Some(n) = self.cstr(p.wrapping_add(nn), 256)
                        {
                            vma.name = format!("[anon:{n}]");
                        }
                    }
                }
                Some(vma)
            })
            .collect();
        out.sort_by_key(|v| v.start);
        out
    }

    /// Il testo di `/proc/<pid>/maps` di un processo.
    pub fn maps(&self, t: &Task) -> String {
        self.vmas(t.mm).iter().map(|v| v.maps_line() + "\n").collect()
    }

    /// Nome di una dentry.
    fn dentry_name(&self, d: u64) -> Option<String> {
        let l = self.l();
        let p = self.u64(d.wrapping_add(l.dentry_name + l.qstr_name))?;
        self.cstr(p, 256)
    }

    /// Percorso di una `struct path` (mnt, dentry) fino alla radice
    /// globale, come `d_path`; i file dei file system speciali come in
    /// `/proc/<pid>/fd` (`socket:[ino]`, `pipe:[ino]`, `anon_inode:nome`).
    pub fn path(&self, mnt: u64, dentry: u64) -> String {
        let l = self.l();
        let sb = self.u64(dentry.wrapping_add(l.dentry_sb)).unwrap_or(0);
        let magic = self.u64(sb.wrapping_add(l.sb_magic)).unwrap_or(0);
        let inode = self.u64(dentry.wrapping_add(l.dentry_inode)).unwrap_or(0);
        let ino = self.u64(inode.wrapping_add(l.inode_ino)).unwrap_or(0);
        match magic {
            SOCKFS_MAGIC => return format!("socket:[{ino}]"),
            PIPEFS_MAGIC => return format!("pipe:[{ino}]"),
            ANON_INODE_FS_MAGIC => {
                return format!("anon_inode:{}", self.dentry_name(dentry).unwrap_or_default());
            }
            _ => {}
        }
        let mut parts: Vec<String> = Vec::new();
        let mut d = dentry;
        let mut m = mnt.wrapping_sub(l.mount_mnt);
        for _ in 0..4096 {
            let root = self.u64(m.wrapping_add(l.mount_mnt + l.vfsmount_root)).unwrap_or(0);
            let parent = self.u64(d.wrapping_add(l.dentry_parent)).unwrap_or(d);
            if d == root || parent == d {
                let mp = self.u64(m.wrapping_add(l.mount_parent)).unwrap_or(m);
                if d != root || mp == m {
                    break;
                }
                d = self.u64(m.wrapping_add(l.mount_mountpoint)).unwrap_or(0);
                m = mp;
                continue;
            }
            parts.push(self.dentry_name(d).unwrap_or_default());
            d = parent;
        }
        if parts.is_empty() {
            return "/".into();
        }
        parts.reverse();
        let mut s = String::new();
        for p in parts {
            s.push('/');
            s.push_str(&p);
        }
        s
    }

    /// Percorso di una `struct file`.
    pub fn file_path(&self, file: u64) -> String {
        let l = self.l();
        let mnt = self.u64(file.wrapping_add(l.file_path + l.path_mnt)).unwrap_or(0);
        let d = self.u64(file.wrapping_add(l.file_path + l.path_dentry)).unwrap_or(0);
        self.path(mnt, d)
    }

    /// `struct file *` del descrittore `fd` di un task.
    pub fn fd_file(&self, task: u64, fd: u32) -> Option<u64> {
        let l = self.l();
        let files = self.u64(task.wrapping_add(l.task_files))?;
        let fdt = self.u64(files.wrapping_add(l.files_fdt))?;
        let max = self.u32(fdt.wrapping_add(l.fdt_max_fds))?;
        if fd >= max {
            return None;
        }
        let arr = self.u64(fdt.wrapping_add(l.fdt_fd))?;
        let f = self.u64(arr.wrapping_add(8 * u64::from(fd)))?;
        (f != 0).then_some(f)
    }

    /// I file aperti di un task, come `/proc/<pid>/fd`.
    pub fn files(&self, task: u64) -> Vec<OpenFile> {
        let l = self.l();
        let mut out = Vec::new();
        let Some(files) = self.u64(task.wrapping_add(l.task_files)) else { return out };
        let Some(fdt) = self.u64(files.wrapping_add(l.files_fdt)) else { return out };
        let max = self.u32(fdt.wrapping_add(l.fdt_max_fds)).unwrap_or(0).min(1 << 16);
        let Some(arr) = self.u64(fdt.wrapping_add(l.fdt_fd)) else { return out };
        for fd in 0..max {
            let Some(f) = self.u64(arr.wrapping_add(8 * u64::from(fd))) else { break };
            if f == 0 {
                continue;
            }
            let inode = self.u64(f.wrapping_add(l.file_inode)).unwrap_or(0);
            out.push(OpenFile {
                fd,
                file: f,
                path: self.file_path(f),
                flags: self.u32(f.wrapping_add(l.file_flags)).unwrap_or(0),
                pos: self.u64(f.wrapping_add(l.file_pos)).unwrap_or(0) as i64,
                inode,
                ino: self.u64(inode.wrapping_add(l.inode_ino)).unwrap_or(0),
            });
        }
        out
    }

    /// `vmemmap` (la `struct page` del pfn 0), calibrato con la pila di un
    /// thread (`stack_vm_area->pages[0]` e l'indirizzo fisico di
    /// `task->stack`): senza costanti che cambiano con la versione.
    pub fn vmemmap(&self) -> Option<u64> {
        if let Some(v) = self.vmemmap.get() {
            return Some(v);
        }
        let l = self.l();
        let vm_area = l.task_stack_vm_area?;
        for t in self.processes() {
            let Some(area) = self.u64(t.addr.wrapping_add(vm_area)) else { continue };
            if area == 0 {
                continue;
            }
            let Some(stack) = self.u64(t.addr.wrapping_add(l.task_stack)) else { continue };
            let Some(pages) = self.u64(area.wrapping_add(l.vm_struct_pages)) else { continue };
            let Some(page0) = self.u64(pages) else { continue };
            let Some(pa) = self.phys(stack) else { continue };
            let v = page0.wrapping_sub((pa >> 12).wrapping_mul(l.page_size));
            self.vmemmap.set(Some(v));
            return Some(v);
        }
        None
    }

    /// Indirizzo fisico della pagina di `page` (una `struct page *`).
    pub fn page_phys(&self, page: u64) -> Option<u64> {
        let pfn = page.wrapping_sub(self.vmemmap()?) / self.l().page_size;
        Some(pfn << 12)
    }

    /// Pagina `index` del file di `inode` nella page cache: indirizzo
    /// fisico, o `None` se non c'è.
    pub fn cached_page(&self, inode: u64, index: u64) -> Option<u64> {
        let l = self.l();
        let mapping = self.u64(inode.wrapping_add(l.inode_mapping))?;
        let mut e = self.u64(mapping.wrapping_add(l.mapping_i_pages + l.xarray_head))?;
        let is_node = |e: u64| e & 3 == 2 && e > 4096;
        let mut parent = 0u64;
        for _ in 0..16 {
            if !is_node(e) {
                break;
            }
            let node = e.wrapping_sub(2);
            let shift = u64::from(self.read_u8(node.wrapping_add(l.xa_node_shift))?);
            let off = (index >> shift) & 63;
            parent = node;
            e = self.u64(node.wrapping_add(l.xa_node_slots + 8 * off))?;
        }
        // Voce "sorella" di una folio grande: rimanda allo slot del capo.
        if e & 3 == 2 && e < 256 << 2 && parent != 0 {
            e = self.u64(parent.wrapping_add(l.xa_node_slots + 8 * (e >> 2)))?;
        }
        if e == 0 || e & 3 != 0 {
            return None;
        }
        if parent == 0 && index != 0 {
            return None;
        }
        let first = self.u64(e.wrapping_add(l.folio_index))?;
        let pa = self.page_phys(e)?;
        Some(pa.wrapping_add(index.checked_sub(first)? << 12))
    }

    fn read_u8(&self, va: u64) -> Option<u8> {
        let mut b = [0u8; 1];
        self.read(va, &mut b).then_some(b[0])
    }

    /// Dimensione del file di `inode`.
    pub fn inode_size(&self, inode: u64) -> Option<u64> {
        self.u64(inode.wrapping_add(self.l().inode_size))
    }

    /// Legge dal file di `inode` attraverso la page cache: falso se una
    /// pagina non c'è.
    pub fn read_file(&self, inode: u64, offset: u64, buf: &mut [u8]) -> bool {
        let mut done = 0usize;
        while done < buf.len() {
            let at = offset.wrapping_add(done as u64);
            let n = (4096 - (at & 4095) as usize).min(buf.len() - done);
            let Some(pa) = self.cached_page(inode, at >> 12) else { return false };
            if !self.mem.read_phys(pa.wrapping_add(at & 4095), &mut buf[done..done + n]) {
                return false;
            }
            done += n;
        }
        true
    }

    /// Tutto il file di `inode` dalla page cache (al più `max` byte).
    pub fn file_bytes(&self, inode: u64, max: u64) -> Option<Vec<u8>> {
        let size = self.inode_size(inode)?.min(max) as usize;
        let mut b = vec![0u8; size];
        self.read_file(inode, 0, &mut b).then_some(b)
    }

    /// `inode` di una `struct file`.
    pub fn file_inode(&self, file: u64) -> Option<u64> {
        self.u64(file.wrapping_add(self.l().file_inode))
    }
}
