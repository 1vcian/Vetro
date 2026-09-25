//! Snapshot della macchina intera (M6, ADR 0015, `docs/specs/snapshot.md`).
//!
//! Il file è l'intestazione di `vetro_snapshot` (magia, versione del
//! formato, hash della configurazione, somma di controllo) seguita da
//! queste sezioni, sempre in quest'ordine:
//!
//! | Sezione | Contenuto |
//! |---|---|
//! | `MACH` | orologio (istruzioni), scadenze in cache, WFI in sospeso, CNTPCT e stato delle linee della scheda |
//! | `CPU ` | registri generali, SIMD/FP, PSTATE, registri di sistema, monitor esclusivo |
//! | `MMU ` | registri di traduzione e voci del TLB |
//! | `PLAT` | timer, GIC, PL011, PL031, PL061 e i 32 slot virtio (trasporto, code, dispositivo, backend) |
//! | `RAM ` | la RAM a pagine, pagine a zero omesse, le altre compresse |
//!
//! Non entrano: il JIT (i blocchi tradotti si rifanno, il risultato non
//! cambia), le cache che non cambiano nulla di osservabile (traduzioni
//! recenti della MMU, livello della linea IRQ), i backend esterni (display,
//! dischi da file o via HTTP), che l'host ricollega prima del ripristino.

use vetro_jit::Next;
use vetro_platform::map;
use vetro_snapshot::{Error, Reader, Snapshot, Writer};

use super::{Devices, Machine, MachineConfig, Pointer};

/// Il contenuto della configurazione che l'hash identifica.
fn config_bytes(m: &Machine) -> Vec<u8> {
    let mut w = Writer::new();
    w.str("vetro-machine");
    let MachineConfig { ram_size, now_secs, seed } = m.cfg;
    w.u64(ram_size);
    w.u64(now_secs);
    w.u64(seed);
    let Devices { gpu, keyboard, pointer, net, vsock_cid } = &m.devices;
    // Le configurazioni annidate (monitor dell'EDID, rete, sinkhole) nella
    // loro forma `Debug`: stabile (tabelle ordinate) e completa.
    w.opt(gpu.as_ref(), |w, g| w.str(&format!("{g:?}")));
    w.bool(*keyboard);
    w.opt(*pointer, |w, p| {
        w.u8(match p {
            Pointer::Tablet => 0,
            Pointer::Multitouch => 1,
        })
    });
    w.opt(net.as_ref(), |w, n| w.str(&format!("{n:?}")));
    w.opt(*vsock_cid, Writer::u64);
    // Ogni slot virtio: tipo, feature offerte, code. Copre anche i
    // dispositivi montati dall'host dopo la costruzione (dischi); il
    // contenuto dei dischi lo controlla il dispositivo stesso.
    let b = m.board.borrow();
    for k in 0..map::VIRTIO_SLOTS as u32 {
        let t = b.virt.virtio(k).expect("32 slot");
        match t.device() {
            None => w.u32(0),
            Some(d) => {
                w.u32(d.device_id());
                w.u64(t.offered_features());
                w.seq(d.queue_max_sizes(), |w, &n| w.u16(n));
            }
        }
    }
    w.into_bytes()
}

impl Machine {
    /// Hash della configurazione: RAM, ora iniziale, seme, dispositivi e
    /// occupazione degli slot virtio. Uno snapshot si applica solo a una
    /// macchina con lo stesso hash.
    pub fn config_hash(&self) -> u64 {
        vetro_snapshot::hash64(&config_bytes(self))
    }

    /// Salva lo stato completo della macchina (vedi il modulo). Non cambia
    /// nulla: si può chiamare fra due [`Machine::run`] qualsiasi, e due
    /// salvataggi nello stesso punto danno gli stessi byte.
    pub fn save(&self) -> Vec<u8> {
        let ram = self.board.borrow().ram.size() as usize;
        let mut w = Writer::with_capacity((1 << 20) + ram / 32);
        w.section(b"MACH", |w| {
            w.u64(self.steps);
            w.opt_u64(self.timer_deadline);
            w.opt_u64(self.net_deadline);
            w.bool(self.wfi_pending);
            let b = self.board.borrow();
            w.u64(b.cntpct);
            w.bool(b.irq_dirty);
            w.bool(b.virtio_dirty);
            w.bool(b.host_wait);
        });
        w.section(b"CPU ", |w| w.put(&self.cpu));
        w.section(b"MMU ", |w| w.put(&self.mmu));
        let b = self.board.borrow();
        w.section(b"PLAT", |w| w.put(&b.virt));
        w.section(b"RAM ", |w| w.put(&b.ram));
        drop(b);
        vetro_snapshot::encode_file(self.config_hash(), w.as_bytes())
    }

    /// Porta la macchina nello stato di `bytes` (da [`Machine::save`]).
    ///
    /// La macchina dev'essere configurata come quella salvata (stessa
    /// [`MachineConfig`], stessi [`Devices`], stessi dispositivi montati
    /// dopo, con i loro backend esterni già collegati): altrimenti
    /// [`Error::Config`]. Uno snapshot di un'altra versione del formato dà
    /// [`Error::Version`]. Il JIT, se attivo, resta: i blocchi tradotti
    /// dalla RAM di prima si scartano da soli. Dopo un errore che non sia
    /// d'intestazione (magia, versione, configurazione, somma di controllo)
    /// lo stato della macchina è indefinito: va scartata.
    pub fn load_state(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let (header, payload) = vetro_snapshot::decode_file(bytes)?;
        let expected = self.config_hash();
        if header.config_hash != expected {
            return Err(Error::Config { found: header.config_hash, expected });
        }
        let mut r = Reader::new(payload);
        let mut s = r.section(b"MACH")?;
        self.steps = s.u64()?;
        self.timer_deadline = s.opt_u64()?;
        self.net_deadline = s.opt_u64()?;
        self.wfi_pending = s.bool()?;
        {
            let mut b = self.board.borrow_mut();
            b.cntpct = s.u64()?;
            b.irq_dirty = s.bool()?;
            b.virtio_dirty = s.bool()?;
            b.host_wait = s.bool()?;
            b.irq_cache = None;
        }
        s.finish()?;
        let mut s = r.section(b"CPU ")?;
        self.cpu.restore(&mut s)?;
        s.finish()?;
        let mut s = r.section(b"MMU ")?;
        self.mmu.restore(&mut s)?;
        s.finish()?;
        {
            let mut b = self.board.borrow_mut();
            let mut s = r.section(b"PLAT")?;
            b.virt.restore(&mut s)?;
            s.finish()?;
            let mut s = r.section(b"RAM ")?;
            b.ram.restore(&mut s)?;
            s.finish()?;
        }
        r.finish()?;
        self.interp = Next::Jit;
        Ok(())
    }

    /// Una macchina nuova nello stato di `bytes`: come
    /// [`Machine::with_devices`] seguito da [`Machine::load_state`]. Per una
    /// macchina con dispositivi montati dall'host (dischi) si costruisce la
    /// macchina, si montano, poi si chiama `load_state`.
    pub fn restore(cfg: &MachineConfig, devices: &Devices, bytes: &[u8]) -> Result<Machine, Error> {
        let mut m = Machine::with_devices(cfg, devices);
        m.load_state(bytes)?;
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::super::Stop;
    use super::super::tests::{DATA, Gate, USED, blk_machine};
    use super::*;
    use vetro_platform::virtio::VirtioBlk;

    const R: u64 = map::RAM_BASE;

    /// Sonda bare-metal (codifiche da `tools/a64asm.sh`): vettori, GICv3,
    /// timer virtuale ogni 2000 tick con un interrupt che scrive `.` sulla
    /// UART e accumula in memoria una somma dei punti interrotti (ELR, x2:
    /// ogni istruzione in più o in meno la cambia per sempre), un ciclo che
    /// incrementa una parola con LDXR/STXR (il monitor esclusivo è spesso armato) e ogni 4096 giri una
    /// SVC (che scrive `s`) e una WFI (che salta alla scadenza del timer).
    const MAIN: [u32; 35] = [
        0xd2a80000, // mov x0, #0x40000000
        0x91200001, // add x1, x0, #0x800
        0xd518c001, // msr VBAR_EL1, x1
        0xd2a10143, // mov x3, #0x80a0000
        0xb900147f, // str wzr, [x3, #0x14]
        0xd2a10004, // mov x4, #0x8000000
        0x52800045, // mov w5, #0x2
        0xb9000085, // str w5, [x4]
        0x91404066, // add x6, x3, #0x10, lsl #12
        0x12800005, // mov w5, #-0x1
        0xb90080c5, // str w5, [x6, #0x80]
        0x52a10005, // mov w5, #0x8000000
        0xb90100c5, // str w5, [x6, #0x100]
        0xd2801e05, // mov x5, #0xf0
        0xd5184605, // msr ICC_PMR_EL1, x5
        0xd2800025, // mov x5, #0x1
        0xd518cce5, // msr ICC_IGRPEN1_EL1, x5
        0xd280fa05, // mov x5, #0x7d0
        0xd51be305, // msr CNTV_TVAL_EL0, x5
        0xd2800025, // mov x5, #0x1
        0xd51be325, // msr CNTV_CTL_EL0, x5
        0xd50342ff, // msr DAIFClr, #0x2
        0xd2a12009, // mov x9, #0x9000000
        0x9140100a, // add x10, x0, #0x4, lsl #12
        0xd2800002, // mov x2, #0x0
        0x91000442, // loop: add x2, x2, #0x1
        0xc85f7d4b, // retry: ldxr x11, [x10]
        0x9100056b, // add x11, x11, #0x1
        0xc80c7d4b, // stxr w12, x11, [x10]
        0x35ffffac, // cbnz w12, retry
        0xf2402c5f, // tst x2, #0xfff
        0x54ffff41, // b.ne loop
        0xd4000001, // svc #0
        0xd503207f, // wfi
        0x17fffff7, // b loop
    ];
    /// Eccezione sincrona a EL1 con SP_EL1 (VBAR + 0x200): la SVC.
    const SVC: [u32; 4] = [
        0x52800e6d, // mov w13, #0x73
        0xb900012d, // str w13, [x9]
        0x91000694, // add x20, x20, #0x1
        0xd69f03e0, // eret
    ];
    /// IRQ a EL1 con SP_EL1 (VBAR + 0x280): il timer.
    const IRQ: [u32; 12] = [
        0xd538cc0e, // mrs x14, ICC_IAR1_EL1
        0xd280fa0f, // mov x15, #0x7d0
        0xd51be30f, // msr CNTV_TVAL_EL0, x15
        0x528005cd, // mov w13, #0x2e
        0xb900012d, // str w13, [x9]
        0xf9400550, // ldr x16, [x10, #0x8]
        0xd5384031, // mrs x17, ELR_EL1
        0xcad01e30, // eor x16, x17, x16, ror #7
        0x8b020210, // add x16, x16, x2
        0xf9000550, // str x16, [x10, #0x8]
        0xd518cc2e, // msr ICC_EOIR1_EL1, x14
        0xd69f03e0, // eret
    ];
    const IRQ_AT: u64 = R + 0x800 + 0x280;

    fn cfg() -> MachineConfig {
        MachineConfig { ram_size: 1 << 20, ..MachineConfig::default() }
    }

    fn probe() -> Machine {
        let mut m = Machine::with_devices(&cfg(), &Devices::none());
        {
            let mut b = m.board.borrow_mut();
            for (base, code) in [(R, &MAIN[..]), (R + 0xa00, &SVC[..]), (IRQ_AT, &IRQ[..])] {
                for (i, w) in code.iter().enumerate() {
                    assert!(b.ram.write(base + 4 * i as u64, &w.to_le_bytes()));
                }
            }
        }
        m.cpu.pc = R;
        m
    }

    /// Esegue fino ad almeno `end` istruzioni a quanti di `q`, accumulando
    /// la console.
    fn run_to(m: &mut Machine, end: u64, q: u64, out: &mut Vec<u8>) {
        while m.steps < end {
            let s = m.run(q.min(end - m.steps));
            out.extend(m.console_output());
            assert_eq!(s, Stop::Budget);
        }
    }

    /// Avanza un'istruzione alla volta finché `pred` vale (al più `limit`).
    fn step_until(m: &mut Machine, out: &mut Vec<u8>, limit: u64, pred: impl Fn(&Machine) -> bool) {
        for _ in 0..limit {
            if pred(m) {
                return;
            }
            m.run(1);
            out.extend(m.console_output());
        }
        panic!("condizione non raggiunta");
    }

    const END: u64 = 300_000;

    /// L'esecuzione senza interruzioni: console e stato finale.
    fn reference() -> (Vec<u8>, Vec<u8>, Machine) {
        let mut m = probe();
        let mut out = Vec::new();
        run_to(&mut m, END, 7_919, &mut out);
        let state = m.save();
        (out, state, m)
    }

    /// Salva in `m` (già avanzata, con la console `out`), ripristina in una
    /// macchina nuova e continua fino a `END`: stessa console, stesse
    /// istruzioni, stessa RAM e stesso stato di tutto (snapshot finale
    /// identico) dell'esecuzione senza interruzioni.
    fn check_cut(m: &Machine, mut out: Vec<u8>, reference: &(Vec<u8>, Vec<u8>, Machine), what: &str) {
        let snap = m.save();
        assert_eq!(snap, m.save(), "{what}: due salvataggi nello stesso punto danno byte diversi");
        let mut n = Machine::restore(&cfg(), &Devices::none(), &snap).unwrap();
        assert_eq!(n.save(), snap, "{what}: il ripristino non riproduce lo snapshot");
        out.extend(n.console_output());
        run_to(&mut n, END, 3_001, &mut out);
        let (ref_out, ref_state, ref_m) = reference;
        assert_eq!(n.steps, ref_m.steps, "{what}: istruzioni");
        assert_eq!(n.cpu, ref_m.cpu, "{what}: CPU");
        assert!(n.board.borrow().ram.bytes() == ref_m.board.borrow().ram.bytes(), "{what}: RAM");
        assert!(out == *ref_out, "{what}: console");
        assert!(n.save() == *ref_state, "{what}: stato finale");
    }

    /// Il criterio di M6 sulla sonda bare-metal: interruzioni in molti punti
    /// (anche a metà di un gestore d'interrupt, con il monitor esclusivo
    /// armato, con un interrupt attivo e IRQ mascherati, subito dopo una
    /// SVC o una WFI) danno la stessa esecuzione.
    #[test]
    fn salva_e_ripristina_in_molti_punti() {
        let r = reference();
        let out = String::from_utf8_lossy(&r.0).into_owned();
        assert!(out.matches('.').count() > 50 && out.contains('s'), "la sonda gira: {out:?}");

        let mut k = 0x9e37_79b9_7f4a_7c15u64;
        let mut cuts = vec![0, 1, 2, 25, 26, 27, 100];
        for _ in 0..12 {
            k = k.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            cuts.push(k % END);
        }
        cuts.sort();
        for &c in &cuts {
            let mut m = probe();
            let mut out = Vec::new();
            run_to(&mut m, c, 5_000, &mut out);
            check_cut(&m, out, &r, &format!("taglio a {c}"));
        }

        // Casi limite trovati un'istruzione alla volta.
        type Pred = fn(&Machine) -> bool;
        let cases: [(&str, u64, Pred); 5] = [
            ("monitor esclusivo armato", 10_000, |m| m.cpu.monitor.is_some()),
            ("dentro il gestore d'interrupt", 20_000, |m| (IRQ_AT..IRQ_AT + 40).contains(&m.cpu.pc)),
            ("interrupt attivo con IRQ mascherati", 30_000, |m| {
                m.cpu.pc == IRQ_AT + 8 && m.board.borrow().virt.gic().irq_state(27).is_some_and(|s| s.2)
            }),
            ("subito dopo la SVC", 40_000, |m| m.cpu.pc == R + 0xa00),
            ("dopo una WFI", 50_000, |m| m.cpu.pc == R + 4 * 34),
        ];
        for (what, from, pred) in cases {
            let mut m = probe();
            let mut out = Vec::new();
            run_to(&mut m, from, 5_000, &mut out);
            step_until(&mut m, &mut out, 20_000, pred);
            check_cut(&m, out, &r, what);
        }
    }

    /// Il ripristino in una macchina che ha già girato (stato diverso
    /// dappertutto, anche nella RAM e nella console) dà la stessa
    /// esecuzione.
    #[test]
    fn ripristino_sopra_una_macchina_usata() {
        let r = reference();
        let mut m = probe();
        let mut out = Vec::new();
        run_to(&mut m, 123_457, 10_000, &mut out);
        let snap = m.save();
        let mut used = probe();
        run_to(&mut used, 250_000, 10_000, &mut Vec::new());
        used.console_input(b"xyz");
        used.load_state(&snap).unwrap();
        assert_eq!(used.save(), snap);
        run_to(&mut used, END, 10_000, &mut out);
        assert!(out == r.0);
        assert!(used.save() == r.1);
    }

    /// Uno snapshot di un'altra versione del formato, di un'altra
    /// configurazione o rovinato si rifiuta con un errore chiaro, e la
    /// macchina non cambia.
    #[test]
    fn snapshot_incompatibili_rifiutati() {
        let mut m = probe();
        run_to(&mut m, 1_000, 1_000, &mut Vec::new());
        let snap = m.save();

        let mut old = snap.clone();
        old[8..12].copy_from_slice(&(vetro_snapshot::FORMAT_VERSION + 1).to_le_bytes());
        let mut fresh = probe();
        let before = fresh.save();
        let e = fresh.load_state(&old).unwrap_err();
        assert_eq!(
            e,
            Error::Version {
                found: vetro_snapshot::FORMAT_VERSION + 1,
                expected: vetro_snapshot::FORMAT_VERSION
            }
        );
        assert!(e.to_string().contains("formato versione"), "{e}");
        assert_eq!(fresh.save(), before, "rifiutato senza toccare la macchina");

        let other = MachineConfig { ram_size: 2 << 20, ..cfg() };
        let e = Machine::restore(&other, &Devices::none(), &snap).err().unwrap();
        assert!(matches!(e, Error::Config { .. }), "{e:?}");
        assert!(e.to_string().contains("configurata diversamente"), "{e}");
        let e = Machine::restore(&cfg(), &Devices::default(), &snap).err().unwrap();
        assert!(matches!(e, Error::Config { .. }), "dispositivi diversi: {e:?}");

        let mut bad = snap.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0x40;
        assert_eq!(Machine::restore(&cfg(), &Devices::none(), &bad).err(), Some(Error::Checksum));
        assert_eq!(
            Machine::restore(&cfg(), &Devices::none(), b"not a snapshot").err(),
            Some(Error::BadMagic)
        );
    }

    /// Snapshot con una richiesta di virtio-blk in sospeso (disco non
    /// pronto, `Stop::Blocked`): la richiesta in volo, il tempo fermo e il
    /// servizio da rifare entrano nello snapshot. Ripristinata su un disco
    /// ricollegato, la richiesta si completa come senza interruzione.
    #[test]
    fn richiesta_virtio_blk_in_volo() {
        let (mut ready, _) = blk_machine(true);
        assert_eq!(ready.run(1000), Stop::Budget);

        let (mut m, slot) = blk_machine(false);
        assert_eq!(m.run(1000), Stop::Blocked);
        let snap = m.save();
        let (mut n, slot2) = blk_machine(false);
        assert_eq!(slot, slot2);
        n.load_state(&snap).unwrap();
        assert!(n.blocked(), "ripristinata ferma sulla richiesta");
        assert_eq!(n.steps, 1);
        assert_eq!(n.run(1000), Stop::Blocked, "senza dati resta ferma");
        let b = n.board.borrow();
        assert!(b.virt.virtio(slot).unwrap().device_as::<VirtioBlk>().unwrap().has_pending());
        drop(b);
        n.device::<VirtioBlk, _>(Some(slot), |b| b.backend_as_mut::<Gate>().unwrap().open = true).unwrap();
        assert_eq!(n.run(999), Stop::Budget);
        assert_eq!(n.steps, ready.steps);
        assert_eq!(n.cpu, ready.cpu);
        let ram = |m: &Machine, pa: u64, len: usize| {
            let mut v = vec![0; len];
            assert!(m.board.borrow().ram.read(pa, &mut v));
            v
        };
        assert_eq!(ram(&n, USED, 16), ram(&ready, USED, 16));
        assert_eq!(ram(&n, DATA, 512), ram(&ready, DATA, 512));
    }
}
