//! Caricamento di un ELF statico e stack iniziale (argv, envp, auxv).

use super::mm::{Mm, STACK_SIZE, STACK_TOP};
use crate::elf::{self, LoadError, PAGE};
use vetro_cpu::{Perm, UserMemory};

/// Pagina del trampolino di ritorno dai gestori di segnale (il `sigtramp`
/// del vDSO di Linux arm64): usato quando sa_restorer non è impostato.
pub const SIGTRAMP: u64 = 0x0000_7fff_fff0_0000;

pub struct Image {
    pub mm: Mm,
    pub entry: u64,
    pub sp: u64,
}

/// HWCAP della Cortex-A53, come li riporta QEMU `-cpu cortex-a53`:
/// FP, ASIMD, AES, PMULL, SHA1, SHA2, CRC32, CPUID.
pub const HWCAP: u64 = (1 << 0) | (1 << 1) | (1 << 3) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 7) | (1 << 11);

pub fn load(
    image: &[u8],
    argv: &[Vec<u8>],
    envp: &[Vec<u8>],
    execfn: &str,
    random: &[u8],
) -> Result<Image, LoadError> {
    let mut mem = UserMemory::new();
    let loaded = elf::load(image, &mut mem)?;
    mem.map(STACK_TOP - STACK_SIZE, vec![0; STACK_SIZE as usize], Perm::RW)
        .map_err(|e| LoadError::Overlap(e.base))?;
    let brk = loaded.end.next_multiple_of(PAGE);
    let mut mm = Mm::new(mem, brk);
    let sp = setup_stack(&mut mm.mem, argv, envp, execfn, random, &loaded);
    Ok(Image { mm, entry: loaded.entry, sp })
}

fn setup_stack(
    mem: &mut UserMemory,
    argv: &[Vec<u8>],
    envp: &[Vec<u8>],
    execfn: &str,
    random: &[u8],
    loaded: &elf::Loaded,
) -> u64 {
    const AT_NULL: u64 = 0;
    const AT_PHDR: u64 = 3;
    const AT_PHENT: u64 = 4;
    const AT_PHNUM: u64 = 5;
    const AT_PAGESZ: u64 = 6;
    const AT_BASE: u64 = 7;
    const AT_FLAGS: u64 = 8;
    const AT_ENTRY: u64 = 9;
    const AT_UID: u64 = 11;
    const AT_EUID: u64 = 12;
    const AT_GID: u64 = 13;
    const AT_EGID: u64 = 14;
    const AT_PLATFORM: u64 = 15;
    const AT_HWCAP: u64 = 16;
    const AT_CLKTCK: u64 = 17;
    const AT_SECURE: u64 = 23;
    const AT_RANDOM: u64 = 25;
    const AT_HWCAP2: u64 = 26;
    const AT_EXECFN: u64 = 31;

    let mut top = STACK_TOP;
    let mut push = |mem: &mut UserMemory, b: &[u8]| {
        top -= b.len() as u64;
        mem.poke(top, b).expect("stack mappato");
        top
    };
    let cstr = |s: &[u8]| {
        let mut v = s.to_vec();
        v.push(0);
        v
    };
    let execfn_addr = push(mem, &cstr(execfn.as_bytes()));
    let env_addrs: Vec<u64> =
        envp.iter().rev().map(|e| push(mem, &cstr(e))).collect::<Vec<_>>().into_iter().rev().collect();
    let arg_addrs: Vec<u64> =
        argv.iter().rev().map(|a| push(mem, &cstr(a))).collect::<Vec<_>>().into_iter().rev().collect();
    let platform = push(mem, b"aarch64\0");
    let random_addr = push(mem, random);

    let mut words: Vec<u64> = vec![argv.len() as u64];
    words.extend(&arg_addrs);
    words.push(0);
    words.extend(&env_addrs);
    words.push(0);
    for (k, v) in [
        (AT_HWCAP, HWCAP),
        (AT_PAGESZ, PAGE),
        (AT_CLKTCK, 100),
        (AT_PHDR, loaded.phdr_addr),
        (AT_PHENT, 56),
        (AT_PHNUM, loaded.phnum as u64),
        (AT_BASE, 0),
        (AT_FLAGS, 0),
        (AT_ENTRY, loaded.entry),
        (AT_UID, 0),
        (AT_EUID, 0),
        (AT_GID, 0),
        (AT_EGID, 0),
        (AT_SECURE, 0),
        (AT_RANDOM, random_addr),
        (AT_HWCAP2, 0),
        (AT_EXECFN, execfn_addr),
        (AT_PLATFORM, platform),
        (AT_NULL, 0),
    ] {
        words.push(k);
        words.push(v);
    }
    let sp = (top - words.len() as u64 * 8) & !15;
    for (i, w) in words.iter().enumerate() {
        mem.poke(sp + i as u64 * 8, &w.to_le_bytes()).expect("stack mappato");
    }
    sp
}
