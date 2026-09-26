//! Disposizione delle strutture del kernel che servono all'introspezione,
//! ricavata dal BTF ([`super::btf`]).
//!
//! Ogni offset viene dal BTF del kernel che gira: niente tabelle scritte a
//! mano per versione. I campi opzionali (`Option`) mancano in alcune
//! configurazioni (per esempio `anon_name` senza `CONFIG_ANON_VMA_NAME`).

use super::btf::Btf;

/// Offset (in byte) dei campi usati, e dimensioni.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    // task_struct
    pub task_tasks: u64,
    pub task_pid: u64,
    pub task_tgid: u64,
    pub task_comm: u64,
    pub task_comm_len: u64,
    pub task_flags: u64,
    pub task_mm: u64,
    pub task_real_parent: u64,
    pub task_real_cred: u64,
    pub task_cred: u64,
    pub task_files: u64,
    pub task_fs: u64,
    pub task_signal: u64,
    pub task_thread_node: u64,
    pub task_stack: u64,
    pub task_exit_state: u64,
    /// `stack_vm_area` (con `CONFIG_VMAP_STACK`).
    pub task_stack_vm_area: Option<u64>,
    // signal_struct
    pub signal_thread_head: u64,
    // cred
    pub cred_uid: u64,
    pub cred_euid: u64,
    pub cred_gid: u64,
    // mm_struct
    pub mm_mt_root: u64,
    pub mm_pgd: u64,
    pub mm_start_brk: u64,
    pub mm_brk: u64,
    pub mm_start_stack: u64,
    pub mm_arg_start: u64,
    pub mm_arg_end: u64,
    // maple tree
    pub maple_node_size: u64,
    pub mr64_pivot: u64,
    pub mr64_slot: u64,
    pub mr64_slots: u64,
    pub ma64_pivot: u64,
    pub ma64_slot: u64,
    pub ma64_slots: u64,
    // vm_area_struct
    pub vma_start: u64,
    pub vma_end: u64,
    pub vma_mm: u64,
    pub vma_flags: u64,
    pub vma_pgoff: u64,
    pub vma_file: u64,
    pub vma_ops: u64,
    pub vma_private_data: u64,
    pub vma_anon_name: Option<u64>,
    /// `anon_vma_name.name` (dopo il kref).
    pub anon_name_name: Option<u64>,
    pub special_mapping_name: u64,
    // file, path, dentry, inode
    pub file_path: u64,
    pub file_inode: u64,
    pub file_flags: u64,
    pub file_pos: u64,
    pub path_mnt: u64,
    pub path_dentry: u64,
    pub dentry_parent: u64,
    pub dentry_name: u64,
    pub qstr_name: u64,
    pub dentry_inode: u64,
    pub dentry_sb: u64,
    pub inode_ino: u64,
    pub inode_sb: u64,
    pub inode_mode: u64,
    pub inode_size: u64,
    pub inode_mapping: u64,
    pub sb_dev: u64,
    pub sb_magic: u64,
    // mount
    pub mount_mnt: u64,
    pub mount_parent: u64,
    pub mount_mountpoint: u64,
    pub vfsmount_root: u64,
    // files_struct, fdtable, fs_struct
    pub files_fdt: u64,
    pub fdt_max_fds: u64,
    pub fdt_fd: u64,
    pub fs_root: u64,
    // vm_struct, page, folio, xarray
    pub vm_struct_addr: u64,
    pub vm_struct_pages: u64,
    pub page_size: u64,
    pub folio_index: u64,
    pub mapping_i_pages: u64,
    pub xarray_head: u64,
    pub xa_node_shift: u64,
    pub xa_node_slots: u64,
    /// Socket (per gli hook TLS: descrittore -> quadrupla), se il BTF li ha.
    pub sock: Option<SockLayout>,
}

/// `file->private_data` -> `struct socket` -> `sk` -> `sock_common`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SockLayout {
    pub file_private_data: u64,
    pub socket_sk: u64,
    pub skc_daddr: u64,
    pub skc_rcv_saddr: u64,
    pub skc_dport: u64,
    pub skc_num: u64,
    pub skc_family: u64,
}

impl SockLayout {
    pub fn from_btf(b: &Btf) -> Option<SockLayout> {
        Some(SockLayout {
            file_private_data: b.offset_of("file", "private_data")?,
            socket_sk: b.offset_of("socket", "sk")?,
            skc_daddr: b.offset_of("sock_common", "skc_daddr")?,
            skc_rcv_saddr: b.offset_of("sock_common", "skc_rcv_saddr")?,
            skc_dport: b.offset_of("sock_common", "skc_dport")?,
            skc_num: b.offset_of("sock_common", "skc_num")?,
            skc_family: b.offset_of("sock_common", "skc_family")?,
        })
    }
}

/// Il campo che manca, per il messaggio d'errore.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingField(pub String);

impl core::fmt::Display for MissingField {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "campo del kernel non trovato nel BTF: {}", self.0)
    }
}

impl Layout {
    /// Tutti gli offset dal BTF: errore al primo campo obbligatorio che
    /// manca.
    pub fn from_btf(b: &Btf) -> Result<Layout, MissingField> {
        let off = |s: &str, p: &str| b.offset_of(s, p).ok_or_else(|| MissingField(format!("{s}.{p}")));
        let size = |s: &str| b.struct_size(s).ok_or_else(|| MissingField(format!("sizeof({s})")));
        let array_len = |s: &str, p: &str| -> Result<u64, MissingField> {
            let (_, ty) = b.field(s, p).ok_or_else(|| MissingField(format!("{s}.{p}")))?;
            b.array(ty).map(|(_, _, n)| u64::from(n)).ok_or_else(|| MissingField(format!("{s}.{p}[]")))
        };
        let (comm, comm_ty) =
            b.field("task_struct", "comm").ok_or_else(|| MissingField("task_struct.comm".into()))?;
        let anon_name = b.offset_of("vm_area_struct", "anon_name");
        Ok(Layout {
            task_tasks: off("task_struct", "tasks")?,
            task_pid: off("task_struct", "pid")?,
            task_tgid: off("task_struct", "tgid")?,
            task_comm: comm,
            task_comm_len: b.size_of(comm_ty).unwrap_or(16),
            task_flags: off("task_struct", "flags")?,
            task_mm: off("task_struct", "mm")?,
            task_real_parent: off("task_struct", "real_parent")?,
            task_real_cred: off("task_struct", "real_cred")?,
            task_cred: off("task_struct", "cred")?,
            task_files: off("task_struct", "files")?,
            task_fs: off("task_struct", "fs")?,
            task_signal: off("task_struct", "signal")?,
            task_thread_node: off("task_struct", "thread_node")?,
            task_stack: off("task_struct", "stack")?,
            task_exit_state: off("task_struct", "exit_state")?,
            task_stack_vm_area: b.offset_of("task_struct", "stack_vm_area"),
            signal_thread_head: off("signal_struct", "thread_head")?,
            cred_uid: off("cred", "uid")?,
            cred_euid: off("cred", "euid")?,
            cred_gid: off("cred", "gid")?,
            mm_mt_root: off("mm_struct", "mm_mt.ma_root")?,
            mm_pgd: off("mm_struct", "pgd")?,
            mm_start_brk: off("mm_struct", "start_brk")?,
            mm_brk: off("mm_struct", "brk")?,
            mm_start_stack: off("mm_struct", "start_stack")?,
            mm_arg_start: off("mm_struct", "arg_start")?,
            mm_arg_end: off("mm_struct", "arg_end")?,
            maple_node_size: size("maple_node")?,
            mr64_pivot: off("maple_range_64", "pivot")?,
            mr64_slot: off("maple_range_64", "slot")?,
            mr64_slots: array_len("maple_range_64", "slot")?,
            ma64_pivot: off("maple_arange_64", "pivot")?,
            ma64_slot: off("maple_arange_64", "slot")?,
            ma64_slots: array_len("maple_arange_64", "slot")?,
            vma_start: off("vm_area_struct", "vm_start")?,
            vma_end: off("vm_area_struct", "vm_end")?,
            vma_mm: off("vm_area_struct", "vm_mm")?,
            vma_flags: off("vm_area_struct", "vm_flags")?,
            vma_pgoff: off("vm_area_struct", "vm_pgoff")?,
            vma_file: off("vm_area_struct", "vm_file")?,
            vma_ops: off("vm_area_struct", "vm_ops")?,
            vma_private_data: off("vm_area_struct", "vm_private_data")?,
            vma_anon_name: anon_name,
            anon_name_name: anon_name.and(b.offset_of("anon_vma_name", "name")),
            special_mapping_name: off("vm_special_mapping", "name")?,
            file_path: off("file", "f_path")?,
            file_inode: off("file", "f_inode")?,
            file_flags: off("file", "f_flags")?,
            file_pos: off("file", "f_pos")?,
            path_mnt: off("path", "mnt")?,
            path_dentry: off("path", "dentry")?,
            dentry_parent: off("dentry", "d_parent")?,
            dentry_name: off("dentry", "d_name")?,
            qstr_name: off("qstr", "name")?,
            dentry_inode: off("dentry", "d_inode")?,
            dentry_sb: off("dentry", "d_sb")?,
            inode_ino: off("inode", "i_ino")?,
            inode_sb: off("inode", "i_sb")?,
            inode_mode: off("inode", "i_mode")?,
            inode_size: off("inode", "i_size")?,
            inode_mapping: off("inode", "i_mapping")?,
            sb_dev: off("super_block", "s_dev")?,
            sb_magic: off("super_block", "s_magic")?,
            mount_mnt: off("mount", "mnt")?,
            mount_parent: off("mount", "mnt_parent")?,
            mount_mountpoint: off("mount", "mnt_mountpoint")?,
            vfsmount_root: off("vfsmount", "mnt_root")?,
            files_fdt: off("files_struct", "fdt")?,
            fdt_max_fds: off("fdtable", "max_fds")?,
            fdt_fd: off("fdtable", "fd")?,
            fs_root: off("fs_struct", "root")?,
            vm_struct_addr: off("vm_struct", "addr")?,
            vm_struct_pages: off("vm_struct", "pages")?,
            page_size: size("page")?,
            folio_index: off("folio", "index")?,
            mapping_i_pages: off("address_space", "i_pages")?,
            xarray_head: off("xarray", "xa_head")?,
            xa_node_shift: off("xa_node", "shift")?,
            xa_node_slots: off("xa_node", "slots")?,
            sock: SockLayout::from_btf(b),
        })
    }
}
