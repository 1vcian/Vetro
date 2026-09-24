// Sonda bare-metal della modalità sistema (EL0/EL1, MMU, eccezioni).
//
// Gira su `qemu-system-aarch64 -M virt,gic-version=3 -cpu cortex-a53`
// (build.sh registra l'uscita in probe.expected) e su Vetro
// (tests/system_probe.rs), che deve stampare esattamente le stesse righe.
// Stampa sulla PL011 a 0x09000000; finisce con PSCI SYSTEM_OFF via HVC.
//
// Esclusi di proposito (differenze documentate in docs/specs/cpu.md):
// ID_AA64PFR0_EL1 (Vetro dichiara solo AArch64), trap di MSR DAIFSet da EL0
// (QEMU scambia Op1 e Op2 nella sindrome), STXR disallineata senza monitor
// (QEMU non la controlla), SP disallineato con SCTLR_EL1.SA/SA0 (QEMU non
// lo controlla), accessi disallineati a memoria Device con MMU accesa e DC
// ZVA su Device (QEMU 10 non dà il fault di allineamento; a MMU spenta sì),
// registri della piattaforma (CNT*, ICC_*), PMU e registri di debug che
// Vetro non modella.

    .equ UART, 0x09000000
    .equ PSCI_SYSTEM_OFF, 0x84000008
    // Descrittori (VMSAv8-64, granulo 4 KiB). MAIR: 0 Device-nGnRnE,
    // 1 Normal Write-Back, 2 Normal Non-cacheable.
    .equ KPAGE,    0x0040000000000707   // AF, SH inner, Normal, RW solo EL1, UXN
    .equ UCODE,    0x00200000000007c7   // RO per tutti, PXN
    .equ UDATA,    0x0060000000000747   // RW per tutti, UXN, PXN
    .equ KRO,      0x0040000000000787   // RO solo EL1, UXN
    .equ KNOAF,    0x0040000000000307   // come KPAGE ma senza AF
    .equ KDEV,     0x0060000000000403   // Device-nGnRnE, RW solo EL1
    .equ DEVBLOCK, 0x0060000000000401   // blocco da 1 GiB Device-nGnRnE
    .equ TCRVAL,   0x0000000200800019   // T0SZ 25, TG0 4 KiB, EPD1, IPS 40 bit

    .text
    .global _start
_start:
    adr x1, stack_top
    mov sp, x1
    adr x1, vectors
    msr vbar_el1, x1
    isb

// Stampa "nome=valore" di un registro di sistema.
.macro PR name, reg
    adr x0, 91f
    bl puts
    mrs x0, \reg
    bl puthex
    b 92f
91: .asciz "\name="
    .balign 4
92:
.endm

// Scrive tutti uno, rilegge e rimette il valore di prima.
.macro WR name, reg
    mrs x20, \reg
    mov x1, #-1
    msr \reg, x1
    isb
    adr x0, 91f
    bl puts
    mrs x0, \reg
    bl puthex
    msr \reg, x20
    isb
    b 92f
91: .asciz "W:\name="
    .balign 4
92:
.endm

// Titolo di una prova.
.macro T name
    adr x0, 91f
    bl puts
    b 92f
91: .asciz "T:\name\n"
    .balign 4
92:
.endm

// Stampa un registro generale.
.macro PX reg
    mov x0, \reg
    bl puthex
.endm

// AT su un'etichetta e stampa di PAR_EL1.
.macro ATL name, op, label
    T \name
    adr x1, \label
    at \op, x1
    isb
    mrs x0, par_el1
    bl puthex
.endm

// AT su un indirizzo in x1 e stampa di PAR_EL1.
.macro ATX name, op
    T \name
    at \op, x1
    isb
    mrs x0, par_el1
    bl puthex
.endm

// Voce della tabella di livello 3 per la pagina di `label` (identità).
.macro PTE label, attrs
    adr x0, \label
    ldr x4, =\attrs
    orr x4, x4, x0
    lsr x1, x0, #12
    and x1, x1, #0x1ff
    adr x2, l3
    str x4, [x2, x1, lsl #3]
.endm

    // --- Identificazione e reset (MMU spenta, EL1h) ---
    PR MIDR, midr_el1
    PR MPIDR, mpidr_el1
    PR REVIDR, revidr_el1
    PR CURRENTEL, currentel
    PR DAIF, daif
    PR SPSEL, spsel
    PR SCTLR, sctlr_el1
    PR ACTLR, actlr_el1
    PR CPACR, cpacr_el1
    PR TCR, tcr_el1
    PR MAIR, mair_el1
    PR AMAIR, amair_el1
    PR CONTEXTIDR, contextidr_el1
    PR CNTKCTL, cntkctl_el1
    PR MDSCR, mdscr_el1
    PR OSLSR, oslsr_el1
    PR MDRAR, mdrar_el1
    PR CLIDR, clidr_el1
    PR CSSELR, csselr_el1
    PR CTR, ctr_el0
    PR DCZID, dczid_el0
    PR AIDR, aidr_el1
    PR RVBAR, rvbar_el1
    PR ISR, isr_el1
    PR PAR, par_el1
    PR PMUSERENR, pmuserenr_el0
    PR ID_PFR0, S3_0_C0_C1_0
    PR ID_PFR1, S3_0_C0_C1_1
    PR ID_DFR0, S3_0_C0_C1_2
    PR ID_AFR0, S3_0_C0_C1_3
    PR ID_MMFR0, S3_0_C0_C1_4
    PR ID_MMFR1, S3_0_C0_C1_5
    PR ID_MMFR2, S3_0_C0_C1_6
    PR ID_MMFR3, S3_0_C0_C1_7
    PR ID_ISAR0, S3_0_C0_C2_0
    PR ID_ISAR1, S3_0_C0_C2_1
    PR ID_ISAR2, S3_0_C0_C2_2
    PR ID_ISAR3, S3_0_C0_C2_3
    PR ID_ISAR4, S3_0_C0_C2_4
    PR ID_ISAR5, S3_0_C0_C2_5
    PR ID_2_6, S3_0_C0_C2_6
    PR ID_2_7, S3_0_C0_C2_7
    PR MVFR0, S3_0_C0_C3_0
    PR MVFR1, S3_0_C0_C3_1
    PR MVFR2, S3_0_C0_C3_2
    PR ID_3_3, S3_0_C0_C3_3
    PR ID_3_7, S3_0_C0_C3_7
    PR ID_AA64PFR1, S3_0_C0_C4_1
    PR ID_AA64ZFR0, S3_0_C0_C4_4
    PR ID_AA64DFR0, S3_0_C0_C5_0
    PR ID_AA64DFR1, S3_0_C0_C5_1
    PR ID_AA64AFR0, S3_0_C0_C5_4
    PR ID_AA64ISAR0, S3_0_C0_C6_0
    PR ID_AA64ISAR1, S3_0_C0_C6_1
    PR ID_AA64ISAR2, S3_0_C0_C6_2
    PR ID_AA64MMFR0, S3_0_C0_C7_0
    PR ID_AA64MMFR1, S3_0_C0_C7_1
    PR ID_AA64MMFR2, S3_0_C0_C7_2
    PR ID_7_7, S3_0_C0_C7_7

    mov x1, #0
    msr csselr_el1, x1
    isb
    PR CCSIDR0, ccsidr_el1
    mov x1, #1
    msr csselr_el1, x1
    isb
    PR CCSIDR1, ccsidr_el1
    mov x1, #2
    msr csselr_el1, x1
    isb
    PR CCSIDR2, ccsidr_el1
    mov x1, #3
    msr csselr_el1, x1
    isb
    PR CCSIDR3, ccsidr_el1
    msr csselr_el1, xzr

    // --- Maschere di scrittura ---
    WR ACTLR, actlr_el1
    WR CPACR, cpacr_el1
    WR TCR, tcr_el1
    WR TTBR0, ttbr0_el1
    WR TTBR1, ttbr1_el1
    WR MAIR, mair_el1
    WR AMAIR, amair_el1
    WR CONTEXTIDR, contextidr_el1
    WR TPIDR1, tpidr_el1
    WR TPIDR0, tpidr_el0
    WR TPIDRRO, tpidrro_el0
    WR PAR, par_el1
    WR CNTKCTL, cntkctl_el1
    WR MDSCR, mdscr_el1
    WR ESR, esr_el1
    WR FAR, far_el1
    WR ELR, elr_el1
    WR SPSR, spsr_el1
    WR AFSR0, afsr0_el1
    WR AFSR1, afsr1_el1
    WR CSSELR, csselr_el1
    WR OSDLR, osdlr_el1
    WR MDCCINT, mdccint_el1
    WR DBGBVR0, dbgbvr0_el1
    WR DBGBCR0, dbgbcr0_el1
    WR DBGWVR0, dbgwvr0_el1
    WR DBGWCR0, dbgwcr0_el1
    WR DBGBCR5, dbgbcr5_el1
    WR DBGWCR3, dbgwcr3_el1
    WR PMUSERENR, pmuserenr_el0
    WR NZCV, nzcv
    WR SP_EL0, sp_el0
    // FPCR e FPSR con CPACR_EL1 = 0: trap FP anche a EL1.
    WR FPCR, fpcr

    mrs x20, vbar_el1
    mov x1, #-1
    msr vbar_el1, x1
    mrs x22, vbar_el1
    msr vbar_el1, x20
    isb
    T vbar
    PX x22

    msr oslar_el1, xzr
    PR OSLSR_AFTER, oslsr_el1

    // --- Eccezioni sincrone a EL1 (il gestore stampa e salta) ---
    T udf
    udf #0
    T hvc_psci_not_supported
    mov x0, #0
    hvc #0x12
    PX x0
    T smc
    smc #0x34
    T brk
    brk #0x56
    T svc
    svc #0x78
    T align_device
    mov x1, #0x0010
    lsl x1, x1, #16
    orr x1, x1, #1
    ldr x2, [x1]
    T ldxr_unaligned
    ldxr x2, [x1]
    T extabt_read
    mov x1, #0x1000
    lsl x1, x1, #20
    ldr x2, [x1]
    T extabt_write
    str x2, [x1]
    T addr_size
    mov x1, #1
    lsl x1, x1, #41
    ldr x2, [x1]
    T spsel_imm
    msr spsel, #0
    mrs x0, spsel
    bl puthex
    msr spsel, #1
    T spel0_undef
    msr spsel, #0
    mrs x2, sp_el0
    msr spsel, #1
    T eret_illegal
    mov x1, #0x9
    orr x1, x1, #0x3c0
    msr spsr_el1, x1
    adr x1, 1f
    msr elr_el1, x1
    eret
1:  nop
    T after_il
    mrs x0, currentel
    bl puthex
    T fptrap
    mrs x21, cpacr_el1
    msr cpacr_el1, xzr
    isb
    fmov d0, x1
    mrs x0, fpcr
    msr cpacr_el1, x21
    isb
    ATL at_mmu_off, s1e1r, _start
    T at_mmu_off_bigpa
    mov x1, #1
    lsl x1, x1, #41
    at s1e1w, x1
    isb
    mrs x0, par_el1
    bl puthex
    T pcalign
    adr x1, 2f
    add x1, x1, #2
    blr x1
    b 3f
2:  nop
    nop
3:

    // --- Tabelle delle pagine e MMU ---
    // L3: 0x4000_0000..0x401f_ffff a identità, kernel RW solo EL1, UXN.
    adr x0, l3
    mov x1, #0x40000000
    ldr x2, =KPAGE
    mov x3, #512
4:  orr x4, x1, x2
    str x4, [x0], #8
    add x1, x1, #0x1000
    subs x3, x3, #1
    b.ne 4b
    PTE el0_code, UCODE
    PTE el0_data, UDATA
    PTE ro_page, KRO
    PTE noaf_page, KNOAF
    PTE dev_page, KDEV
    adr x0, bad_page
    lsr x1, x0, #12
    and x1, x1, #0x1ff
    adr x2, l3
    str xzr, [x2, x1, lsl #3]
    // L2[0] -> L3; L1[0] = blocco Device da 1 GiB (UART, GIC); L1[1] -> L2.
    adr x0, l2
    adr x1, l3
    orr x1, x1, #3
    str x1, [x0]
    adr x0, l1
    ldr x1, =DEVBLOCK
    str x1, [x0]
    adr x1, l2
    orr x1, x1, #3
    str x1, [x0, #8]
    ldr x0, =0x44ff00
    msr mair_el1, x0
    ldr x0, =TCRVAL
    msr tcr_el1, x0
    adr x0, l1
    msr ttbr0_el1, x0
    isb
    tlbi vmalle1
    dsb sy
    isb
    mrs x0, sctlr_el1
    orr x0, x0, #1
    msr sctlr_el1, x0
    isb
    T mmu_on

    ATL at_k_e1r, s1e1r, _start
    ATL at_k_e0r, s1e0r, _start
    ATL at_ro_e1w, s1e1w, ro_page
    ATL at_ro_e1r, s1e1r, ro_page
    ATL at_udata_e0w, s1e0w, el0_data
    ATL at_ucode_e0r, s1e0r, el0_code
    ATL at_ucode_e0w, s1e0w, el0_code
    ATL at_noaf, s1e1r, noaf_page
    ATL at_bad, s1e1r, bad_page
    ATL at_dev, s1e1r, dev_page
    mov x1, #0x09000000
    ATX at_uart_block, s1e1r
    mov x1, #0x80000000
    ATX at_l1_invalid, s1e1r
    mov x1, #1
    lsl x1, x1, #40
    ATX at_out_of_range, s1e1r

    T ld_bad
    adr x1, bad_page
    ldr x2, [x1, #8]
    T ld_noaf
    adr x1, noaf_page
    ldr x2, [x1]
    T st_ro
    adr x1, ro_page
    str x2, [x1]
    T ldtr_kernel
    adr x1, _start
    ldtr x2, [x1]
    T sttr_ldtr_user
    adr x1, el0_data
    mov x2, #0x1234
    sttr x2, [x1]
    ldtr x3, [x1]
    PX x3
    T unaligned_normal
    adr x1, el0_data
    ldr x3, [x1, #1]
    PX x3
    T dczva_normal
    adr x1, el0_data
    dc zva, x1
    ldr x3, [x1]
    PX x3
    T ld_l1_invalid
    mov x1, #0x80000000
    ldr x2, [x1]
    // TLBI: el0_data ora punta a tlbi_page, poi torna com'era.
    T tlbi_remap
    adr x0, el0_data
    lsr x1, x0, #12
    and x1, x1, #0x1ff
    adr x2, l3
    add x2, x2, x1, lsl #3
    ldr x5, [x2]
    adr x3, tlbi_page
    ldr x4, =UDATA
    orr x4, x4, x3
    mov x6, #0x77
    str x6, [x3]
    dsb sy
    str x4, [x2]
    dsb sy
    lsr x1, x0, #12
    tlbi vae1, x1
    dsb sy
    isb
    ldr x3, [x0]
    PX x3
    str x5, [x2]
    dsb sy
    tlbi vmalle1
    dsb sy
    isb
    adr x0, el0_data
    ldr x3, [x0]
    PX x3

    // --- EL0 ---
    T el0
    mrs x1, sctlr_el1
    bic x1, x1, #(1 << 16)
    msr sctlr_el1, x1
    isb
    adr x19, _start
    adr x20, el0_data
    adr x21, el0_code
    adr x23, bad_page
    adr x1, el0_code
    msr elr_el1, x1
    msr spsr_el1, xzr
    add x1, x20, #0x800
    msr sp_el0, x1
    eret

// ---------------------------------------------------------------------------
finish:
    T fine
    ldr x0, =PSCI_SYSTEM_OFF
    hvc #0
    b .

puts:
    mov x9, #UART
1:  ldrb w10, [x0], #1
    cbz w10, 2f
    strb w10, [x9]
    b 1b
2:  ret

puthex:
    mov x9, #UART
    mov x11, #60
1:  lsr x10, x0, x11
    and x10, x10, #0xf
    cmp x10, #10
    b.lt 2f
    add x10, x10, #('a' - 10)
    b 3f
2:  add x10, x10, #'0'
3:  strb w10, [x9]
    subs x11, x11, #4
    b.ge 1b
    mov x10, #'\n'
    strb w10, [x9]
    ret

// Gestore comune: x0 = indice del vettore; x0/x1 originali in [sp].
common:
    stp x2, x3, [sp, #16]
    stp x4, x5, [sp, #32]
    stp x6, x7, [sp, #48]
    stp x8, x9, [sp, #64]
    stp x10, x11, [sp, #80]
    stp x12, x13, [sp, #96]
    stp x19, x20, [sp, #112]
    stp x29, x30, [sp, #128]
    mov x29, x0
    adr x0, s_v
    bl puts
    mov x0, x29
    bl puthex
    adr x0, s_e
    bl puts
    mrs x0, esr_el1
    bl puthex
    adr x0, s_far
    bl puts
    mrs x0, far_el1
    bl puthex
    adr x0, s_elr
    bl puts
    mrs x0, elr_el1
    adr x1, _start
    sub x0, x0, x1
    bl puthex
    adr x0, s_spsr
    bl puts
    mrs x0, spsr_el1
    bl puthex
    mrs x0, spsr_el1
    bic x0, x0, #(1 << 20)
    msr spsr_el1, x0
    mrs x0, esr_el1
    lsr x1, x0, #26
    cmp x1, #0x15
    b.ne 5f
    and x2, x0, #0xffff
    cmp x2, #2
    b.eq finish
    b 7f
5:  cmp x1, #0x16
    b.eq 7f
    cmp x1, #0x17
    b.eq 7f
    // Instruction Abort e PC disallineato: si torna all'indirizzo di
    // ritorno del BLR che li ha causati.
    cmp x1, #0x20
    b.eq 6f
    cmp x1, #0x21
    b.eq 6f
    cmp x1, #0x22
    b.eq 6f
    mrs x0, elr_el1
    add x0, x0, #4
    msr elr_el1, x0
    b 7f
6:  ldr x0, [sp, #136]
    msr elr_el1, x0
7:  ldp x2, x3, [sp, #16]
    ldp x4, x5, [sp, #32]
    ldp x6, x7, [sp, #48]
    ldp x8, x9, [sp, #64]
    ldp x10, x11, [sp, #80]
    ldp x12, x13, [sp, #96]
    ldp x19, x20, [sp, #112]
    ldp x29, x30, [sp, #128]
    ldp x0, x1, [sp]
    add sp, sp, #256
    eret

s_v:    .asciz "V:"
s_e:    .asciz "E:"
s_far:  .asciz " FAR:"
s_elr:  .asciz " ELR-start:"
s_spsr: .asciz " SPSR:"
    .balign 4
    .ltorg

    .balign 2048
vectors:
    .irp i, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
    .balign 128
    sub sp, sp, #256
    stp x0, x1, [sp]
    mov x0, #\i
    b common
    .endr

    // Codice di EL0: una pagina a sé (RO per tutti, eseguibile solo a EL0).
    .balign 4096
el0_code:
    str x0, [x20]
    ldr x2, [x20]
    ldr x2, [x19]
    str x2, [x21]
    ldr x2, [x23]
    ldr x2, [x20, #1]
    blr x19
    mrs x2, ctr_el0
    dc zva, x20
    mrs x2, daif
    mrs x2, cntvct_el0
    mrs x2, sctlr_el1
    wfi
    wfe
    dc cvau, x20
    ic ivau, x20
    msr tpidrro_el0, x0
    mrs x2, tpidrro_el0
    hvc #0
    eret
    ldtr x2, [x20]
    svc #2

    .balign 4096
el0_data:
    .space 4096
ro_page:
    .space 4096
noaf_page:
    .space 4096
bad_page:
    .space 4096
dev_page:
    .space 4096
tlbi_page:
    .space 4096
scratch:
    .space 4096
l1: .space 4096
l2: .space 4096
l3: .space 4096
    .space 8192
stack_top:
