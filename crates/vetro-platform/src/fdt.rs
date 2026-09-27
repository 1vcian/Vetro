//! Binary device tree generator (FDT/DTB version 17, Devicetree
//! specification v0.4 chapter 5) and device tree of the virt platform.
//!
//! Layout produced: header (40 bytes), memory reservation block,
//! structure block, strings block. All numbers are big
//! endian; property names are deduplicated in the strings block.

use core::fmt;

use crate::map;

pub const FDT_MAGIC: u32 = 0xD00D_FEED;
pub const FDT_VERSION: u32 = 17;
pub const FDT_LAST_COMP_VERSION: u32 = 16;
pub const FDT_BEGIN_NODE: u32 = 1;
pub const FDT_END_NODE: u32 = 2;
pub const FDT_PROP: u32 = 3;
pub const FDT_END: u32 = 9;
/// Size of the v17 header.
pub const HEADER_SIZE: usize = 40;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FdtError {
    /// Empty node or property name where not allowed, or containing a NUL.
    InvalidName(String),
    /// Property written outside any node.
    PropertyOutsideNode(String),
    /// `end_node` without a matching `begin_node`.
    UnmatchedEnd,
    /// `finish` with nodes still open.
    UnclosedNodes(usize),
    /// No root node.
    Empty,
    /// More than one root node.
    MultipleRoots,
}

impl fmt::Display for FdtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FdtError::InvalidName(n) => write!(f, "invalid name: {n:?}"),
            FdtError::PropertyOutsideNode(n) => write!(f, "property {n:?} outside a node"),
            FdtError::UnmatchedEnd => write!(f, "end_node without begin_node"),
            FdtError::UnclosedNodes(n) => write!(f, "{n} nodes still open"),
            FdtError::Empty => write!(f, "device tree without root"),
            FdtError::MultipleRoots => write!(f, "more than one root node"),
        }
    }
}

/// Builder of a DTB. The methods chain; the first error is
/// kept and returned by [`FdtBuilder::finish`].
#[derive(Clone, Debug, Default)]
pub struct FdtBuilder {
    structs: Vec<u8>,
    strings: Vec<u8>,
    reserve: Vec<(u64, u64)>,
    boot_cpuid: u32,
    depth: usize,
    roots: usize,
    error: Option<FdtError>,
}

impl FdtBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    fn fail(&mut self, e: FdtError) {
        self.error.get_or_insert(e);
    }

    fn push_u32(&mut self, v: u32) {
        self.structs.extend_from_slice(&v.to_be_bytes());
    }

    fn pad(&mut self) {
        while !self.structs.len().is_multiple_of(4) {
            self.structs.push(0);
        }
    }

    /// Adds a reserved region (`/memreserve/`).
    pub fn reserve_memory(&mut self, addr: u64, size: u64) -> &mut Self {
        self.reserve.push((addr, size));
        self
    }

    /// `boot_cpuid_phys` of the header.
    pub fn boot_cpuid(&mut self, id: u32) -> &mut Self {
        self.boot_cpuid = id;
        self
    }

    /// Opens a node. The root has an empty name; the others do not.
    pub fn begin_node(&mut self, name: &str) -> &mut Self {
        if name.contains('\0') || (name.is_empty() != (self.depth == 0)) {
            self.fail(FdtError::InvalidName(name.into()));
        }
        if self.depth == 0 {
            if self.roots > 0 {
                self.fail(FdtError::MultipleRoots);
            }
            self.roots += 1;
        }
        self.push_u32(FDT_BEGIN_NODE);
        self.structs.extend_from_slice(name.as_bytes());
        self.structs.push(0);
        self.pad();
        self.depth += 1;
        self
    }

    pub fn end_node(&mut self) -> &mut Self {
        if self.depth == 0 {
            self.fail(FdtError::UnmatchedEnd);
            return self;
        }
        self.push_u32(FDT_END_NODE);
        self.depth -= 1;
        self
    }

    fn string_offset(&mut self, name: &str) -> u32 {
        let needle = name.as_bytes();
        let mut start = 0;
        for (i, &b) in self.strings.iter().enumerate() {
            if b == 0 {
                if &self.strings[start..i] == needle {
                    return start as u32;
                }
                start = i + 1;
            }
        }
        let off = self.strings.len() as u32;
        self.strings.extend_from_slice(needle);
        self.strings.push(0);
        off
    }

    /// Property with a raw value.
    pub fn prop_bytes(&mut self, name: &str, value: &[u8]) -> &mut Self {
        if name.is_empty() || name.contains('\0') {
            self.fail(FdtError::InvalidName(name.into()));
        }
        if self.depth == 0 {
            self.fail(FdtError::PropertyOutsideNode(name.into()));
        }
        let off = self.string_offset(name);
        self.push_u32(FDT_PROP);
        self.push_u32(value.len() as u32);
        self.push_u32(off);
        self.structs.extend_from_slice(value);
        self.pad();
        self
    }

    /// Property without a value (boolean), e.g. `interrupt-controller`.
    pub fn prop_empty(&mut self, name: &str) -> &mut Self {
        self.prop_bytes(name, &[])
    }

    pub fn prop_u32(&mut self, name: &str, v: u32) -> &mut Self {
        self.prop_bytes(name, &v.to_be_bytes())
    }

    pub fn prop_u64(&mut self, name: &str, v: u64) -> &mut Self {
        self.prop_bytes(name, &v.to_be_bytes())
    }

    pub fn prop_u32_list(&mut self, name: &str, vs: &[u32]) -> &mut Self {
        let bytes: Vec<u8> = vs.iter().flat_map(|v| v.to_be_bytes()).collect();
        self.prop_bytes(name, &bytes)
    }

    /// List of u64, each as two cells (e.g. `reg` with 2+2 cells).
    pub fn prop_u64_list(&mut self, name: &str, vs: &[u64]) -> &mut Self {
        let bytes: Vec<u8> = vs.iter().flat_map(|v| v.to_be_bytes()).collect();
        self.prop_bytes(name, &bytes)
    }

    pub fn prop_str(&mut self, name: &str, s: &str) -> &mut Self {
        self.prop_strs(name, &[s])
    }

    /// List of NUL-terminated strings (e.g. `compatible`).
    pub fn prop_strs(&mut self, name: &str, ss: &[&str]) -> &mut Self {
        let mut bytes = Vec::new();
        for s in ss {
            if s.contains('\0') {
                self.fail(FdtError::InvalidName((*s).into()));
            }
            bytes.extend_from_slice(s.as_bytes());
            bytes.push(0);
        }
        self.prop_bytes(name, &bytes)
    }

    /// Closes the structure and produces the blob.
    pub fn finish(&self) -> Result<Vec<u8>, FdtError> {
        if let Some(e) = &self.error {
            return Err(e.clone());
        }
        if self.depth != 0 {
            return Err(FdtError::UnclosedNodes(self.depth));
        }
        if self.roots == 0 {
            return Err(FdtError::Empty);
        }
        let off_rsv = HEADER_SIZE;
        let rsv_size = (self.reserve.len() + 1) * 16;
        let off_struct = off_rsv + rsv_size;
        let size_struct = self.structs.len() + 4;
        let off_strings = off_struct + size_struct;
        let total = off_strings + self.strings.len();

        let mut out = Vec::with_capacity(total);
        for v in [
            FDT_MAGIC,
            total as u32,
            off_struct as u32,
            off_strings as u32,
            off_rsv as u32,
            FDT_VERSION,
            FDT_LAST_COMP_VERSION,
            self.boot_cpuid,
            self.strings.len() as u32,
            size_struct as u32,
        ] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        for &(a, s) in self.reserve.iter().chain(&[(0, 0)]) {
            out.extend_from_slice(&a.to_be_bytes());
            out.extend_from_slice(&s.to_be_bytes());
        }
        out.extend_from_slice(&self.structs);
        out.extend_from_slice(&FDT_END.to_be_bytes());
        out.extend_from_slice(&self.strings);
        debug_assert_eq!(out.len(), total);
        Ok(out)
    }
}

/// Parameters of the virt platform device tree (one CPU).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtDtbConfig {
    /// RAM size starting at `map::RAM_BASE`.
    pub ram_size: u64,
    /// Kernel command line (`/chosen/bootargs`).
    pub bootargs: String,
    /// Initramfs: physical start and end (exclusive) addresses.
    pub initrd: Option<(u64, u64)>,
    /// PSCI conduit: "hvc" (default, no emulated EL2/EL3) or "smc".
    pub psci_method: &'static str,
    /// Fixed PL011 clock in Hz.
    pub uart_clock_hz: u32,
    /// Seed of `/chosen/rng-seed` (32 bytes) and `/chosen/kaslr-seed`, which
    /// QEMU virt always sets: derived from this number, so boot stays
    /// deterministic. `None` = no seeds.
    pub seed: Option<u64>,
    /// Total DTB size (`totalsize`), with free space at the end:
    /// QEMU does not compact its 1 MiB device tree and Linux reserves all of
    /// `totalsize`. 0 = nothing added.
    pub pad_to: usize,
}

/// Size of QEMU virt's device tree (`create_device_tree`).
pub const QEMU_FDT_SIZE: usize = 1 << 20;

/// splitmix64: deterministic bytes for the device tree seeds.
fn seed_bytes(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed;
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        out.extend_from_slice(&(z ^ (z >> 31)).to_le_bytes());
    }
    out.truncate(n);
    out
}

impl Default for VirtDtbConfig {
    fn default() -> Self {
        Self {
            ram_size: 1 << 30,
            bootargs: "console=ttyAMA0 earlycon".into(),
            initrd: None,
            psci_method: "hvc",
            uart_clock_hz: map::UART_CLOCK_HZ,
            seed: None,
            pad_to: 0,
        }
    }
}

/// Phandle of the GIC in the platform device tree.
pub const PHANDLE_GIC: u32 = 1;
/// Phandle of the fixed clock of the PL011/PL031/PL061.
pub const PHANDLE_CLK: u32 = 2;
/// Phandle of the PL061 GPIO (used by `gpio-keys`).
pub const PHANDLE_GPIO: u32 = 3;
/// Linux KEY_POWER (`linux,code` of the power key).
const KEY_POWER: u32 = 116;

const GIC_SPI: u32 = 0;
const GIC_PPI: u32 = 1;
const IRQ_EDGE_RISING: u32 = 1;
const IRQ_LEVEL_HIGH: u32 = 4;

/// Name of the UART node (also in `stdout-path`).
pub const UART_NODE: &str = "pl011@9000000";

fn reg(b: &mut FdtBuilder, base: u64, size: u64) {
    b.prop_u64_list("reg", &[base, size]);
}

/// DTB of the virt platform: same shape as QEMU virt's, reduced
/// to Vetro's devices.
pub fn virt_dtb(cfg: &VirtDtbConfig) -> Vec<u8> {
    let mut b = FdtBuilder::new();
    b.begin_node("")
        .prop_str("compatible", "linux,dummy-virt")
        .prop_u32("#address-cells", 2)
        .prop_u32("#size-cells", 2)
        .prop_u32("interrupt-parent", PHANDLE_GIC);

    b.begin_node("chosen");
    if let Some(seed) = cfg.seed {
        let bytes = seed_bytes(seed, 40);
        b.prop_bytes("rng-seed", &bytes[..32]).prop_bytes("kaslr-seed", &bytes[32..]);
    }
    b.prop_str("bootargs", &cfg.bootargs).prop_str("stdout-path", &format!("/{UART_NODE}"));
    if let Some((start, end)) = cfg.initrd {
        b.prop_u64("linux,initrd-start", start).prop_u64("linux,initrd-end", end);
    }
    b.end_node();

    b.begin_node(&format!("memory@{:x}", map::RAM_BASE)).prop_str("device_type", "memory");
    reg(&mut b, map::RAM_BASE, cfg.ram_size);
    b.end_node();

    b.begin_node("cpus").prop_u32("#address-cells", 1).prop_u32("#size-cells", 0);
    b.begin_node("cpu@0")
        .prop_str("device_type", "cpu")
        .prop_str("compatible", "arm,cortex-a53")
        .prop_u32("reg", 0)
        .prop_str("enable-method", "psci")
        .end_node();
    b.end_node();

    b.begin_node("psci")
        .prop_strs("compatible", &["arm,psci-1.0", "arm,psci-0.2"])
        .prop_str("method", cfg.psci_method)
        .end_node();

    // PPIs in the device tree: number = INTID - 16.
    let ppi = |intid: u32| [GIC_PPI, intid - 16, IRQ_LEVEL_HIGH];
    let timer_irqs: Vec<u32> = [map::PPI_SEC_PTIMER, map::PPI_PTIMER, map::PPI_VTIMER, map::PPI_HYP_TIMER]
        .into_iter()
        .flat_map(ppi)
        .collect();
    b.begin_node("timer")
        .prop_str("compatible", "arm,armv8-timer")
        .prop_u32_list("interrupts", &timer_irqs)
        .prop_empty("always-on")
        .end_node();

    b.begin_node(&format!("intc@{:x}", map::GICD_BASE))
        .prop_str("compatible", "arm,gic-v3")
        .prop_u32("#interrupt-cells", 3)
        .prop_empty("interrupt-controller")
        .prop_u64_list("reg", &[map::GICD_BASE, map::GICD_SIZE, map::GICR_BASE, map::GICR_SIZE_PER_CPU])
        .prop_u32("#redistributor-regions", 1)
        .prop_u32("#address-cells", 2)
        .prop_u32("#size-cells", 2)
        .prop_empty("ranges")
        .prop_u32("phandle", PHANDLE_GIC)
        .end_node();

    b.begin_node("apb-pclk")
        .prop_str("compatible", "fixed-clock")
        .prop_u32("#clock-cells", 0)
        .prop_u32("clock-frequency", cfg.uart_clock_hz)
        .prop_str("clock-output-names", "clk24mhz")
        .prop_u32("phandle", PHANDLE_CLK)
        .end_node();

    b.begin_node(UART_NODE).prop_strs("compatible", &["arm,pl011", "arm,primecell"]);
    reg(&mut b, map::UART_BASE, map::UART_SIZE);
    b.prop_u32_list("interrupts", &[GIC_SPI, map::UART_SPI, IRQ_LEVEL_HIGH])
        .prop_u32_list("clocks", &[PHANDLE_CLK, PHANDLE_CLK])
        .prop_strs("clock-names", &["uartclk", "apb_pclk"])
        .end_node();

    b.begin_node(&format!("pl031@{:x}", map::RTC_BASE))
        .prop_strs("compatible", &["arm,pl031", "arm,primecell"]);
    reg(&mut b, map::RTC_BASE, map::RTC_SIZE);
    b.prop_u32_list("interrupts", &[GIC_SPI, map::RTC_SPI, IRQ_LEVEL_HIGH])
        .prop_u32("clocks", PHANDLE_CLK)
        .prop_str("clock-names", "apb_pclk")
        .end_node();

    // GPIO and power key like QEMU virt (create_gpio_devices).
    b.begin_node(&format!("pl061@{:x}", map::GPIO_BASE))
        .prop_strs("compatible", &["arm,pl061", "arm,primecell"]);
    reg(&mut b, map::GPIO_BASE, map::GPIO_SIZE);
    b.prop_u32_list("interrupts", &[GIC_SPI, map::GPIO_SPI, IRQ_LEVEL_HIGH])
        .prop_u32("#gpio-cells", 2)
        .prop_empty("gpio-controller")
        .prop_u32("clocks", PHANDLE_CLK)
        .prop_str("clock-names", "apb_pclk")
        .prop_u32("phandle", PHANDLE_GPIO)
        .end_node();
    b.begin_node("gpio-keys").prop_str("compatible", "gpio-keys");
    b.begin_node("poweroff")
        .prop_str("label", "GPIO Key Poweroff")
        .prop_u32("linux,code", KEY_POWER)
        .prop_u32_list("gpios", &[PHANDLE_GPIO, crate::pl061::POWER_KEY_LINE, 0])
        .end_node();
    b.end_node();

    for k in 0..map::VIRTIO_SLOTS {
        let base = map::VIRTIO_BASE + k * map::VIRTIO_SLOT_SIZE;
        b.begin_node(&format!("virtio_mmio@{base:x}")).prop_str("compatible", "virtio,mmio");
        reg(&mut b, base, map::VIRTIO_SLOT_SIZE);
        b.prop_u32_list("interrupts", &[GIC_SPI, map::VIRTIO_SPI_BASE + k as u32, IRQ_EDGE_RISING])
            .prop_empty("dma-coherent")
            .end_node();
    }

    b.end_node();
    // The structure is fixed and well-formed: an error here is our bug.
    let mut dtb = b.finish().expect("well-formed virt device tree");
    if dtb.len() < cfg.pad_to {
        dtb.resize(cfg.pad_to, 0);
        let total = (cfg.pad_to as u32).to_be_bytes();
        dtb[4..8].copy_from_slice(&total);
    }
    dtb
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn be32(b: &[u8], off: usize) -> u32 {
        u32::from_be_bytes(b[off..off + 4].try_into().unwrap())
    }

    /// Minimal parser: node path -> (property name -> value).
    fn parse(dtb: &[u8]) -> BTreeMap<String, BTreeMap<String, Vec<u8>>> {
        assert_eq!(be32(dtb, 0), FDT_MAGIC);
        let off_struct = be32(dtb, 8) as usize;
        let off_strings = be32(dtb, 12) as usize;
        let size_struct = be32(dtb, 36) as usize;
        let mut nodes = BTreeMap::new();
        let mut path: Vec<String> = Vec::new();
        let mut p = off_struct;
        loop {
            assert!(p < off_struct + size_struct, "structure without FDT_END");
            let tok = be32(dtb, p);
            p += 4;
            match tok {
                FDT_BEGIN_NODE => {
                    let end = p + dtb[p..].iter().position(|&c| c == 0).unwrap();
                    path.push(String::from_utf8(dtb[p..end].to_vec()).unwrap());
                    p = (end + 1).next_multiple_of(4);
                    nodes.insert(full(&path), BTreeMap::new());
                }
                FDT_END_NODE => {
                    path.pop().expect("END_NODE without a node");
                }
                FDT_PROP => {
                    let len = be32(dtb, p) as usize;
                    let nameoff = be32(dtb, p + 4) as usize;
                    let s = off_strings + nameoff;
                    let e = s + dtb[s..].iter().position(|&c| c == 0).unwrap();
                    let name = String::from_utf8(dtb[s..e].to_vec()).unwrap();
                    let value = dtb[p + 8..p + 8 + len].to_vec();
                    nodes.get_mut(&full(&path)).unwrap().insert(name, value);
                    p = (p + 8 + len).next_multiple_of(4);
                }
                4 => {} // FDT_NOP
                FDT_END => break,
                t => panic!("unknown token {t}"),
            }
        }
        assert!(path.is_empty(), "unclosed nodes");
        assert_eq!(p, off_struct + size_struct);
        nodes
    }

    fn full(path: &[String]) -> String {
        if path.len() <= 1 { "/".into() } else { path[1..].iter().map(|n| format!("/{n}")).collect() }
    }

    fn cells(v: &[u8]) -> Vec<u32> {
        v.chunks(4).map(|c| u32::from_be_bytes(c.try_into().unwrap())).collect()
    }

    fn strs(v: &[u8]) -> Vec<&str> {
        v.strip_suffix(&[0]).unwrap().split(|&c| c == 0).map(|s| core::str::from_utf8(s).unwrap()).collect()
    }

    #[test]
    fn semi_e_spazio_libero_come_qemu() {
        let cfg = VirtDtbConfig { seed: Some(7), pad_to: QEMU_FDT_SIZE, ..VirtDtbConfig::default() };
        let dtb = virt_dtb(&cfg);
        assert_eq!(dtb.len(), QEMU_FDT_SIZE);
        assert_eq!(be32(&dtb, 4) as usize, QEMU_FDT_SIZE, "totalsize with the free space");
        assert_eq!(virt_dtb(&cfg), dtb, "deterministic");
        let other = virt_dtb(&VirtDtbConfig { seed: Some(8), ..cfg.clone() });
        assert_ne!(other, dtb);
        let find = |d: &[u8], name: &[u8]| d.windows(name.len()).any(|w| w == name);
        assert!(find(&dtb, b"rng-seed\0") && find(&dtb, b"kaslr-seed\0"));
        assert!(!find(&virt_dtb(&VirtDtbConfig::default()), b"rng-seed\0"));
    }

    #[test]
    fn header_coerente() {
        let dtb = virt_dtb(&VirtDtbConfig::default());
        assert_eq!(be32(&dtb, 0), 0xD00D_FEED);
        assert_eq!(be32(&dtb, 4) as usize, dtb.len(), "totalsize");
        let (off_struct, off_strings, off_rsv) = (be32(&dtb, 8), be32(&dtb, 12), be32(&dtb, 16));
        assert_eq!(be32(&dtb, 20), 17);
        assert_eq!(be32(&dtb, 24), 16);
        assert_eq!(be32(&dtb, 28), 0);
        let (size_strings, size_struct) = (be32(&dtb, 32), be32(&dtb, 36));
        assert_eq!(off_rsv, 40);
        assert_eq!(off_rsv % 8, 0);
        assert_eq!(off_struct % 4, 0);
        assert_eq!(off_struct, off_rsv + 16, "empty reservations: only the terminator");
        assert_eq!(off_strings, off_struct + size_struct);
        assert_eq!(off_strings + size_strings, dtb.len() as u32);
        assert_eq!(be32(&dtb, off_struct as usize), FDT_BEGIN_NODE);
        assert_eq!(be32(&dtb, (off_strings - 4) as usize), FDT_END);
    }

    #[test]
    fn proprieta_della_piattaforma() {
        let cfg = VirtDtbConfig {
            ram_size: 0x8000_0000,
            bootargs: "console=ttyAMA0 rdinit=/init".into(),
            initrd: Some((0x4800_0000, 0x4810_0000)),
            ..VirtDtbConfig::default()
        };
        let t = parse(&virt_dtb(&cfg));
        let root = &t["/"];
        assert_eq!(strs(&root["compatible"]), ["linux,dummy-virt"]);
        assert_eq!(cells(&root["#address-cells"]), [2]);
        assert_eq!(cells(&root["interrupt-parent"]), [PHANDLE_GIC]);

        let chosen = &t["/chosen"];
        assert_eq!(strs(&chosen["bootargs"]), ["console=ttyAMA0 rdinit=/init"]);
        assert_eq!(strs(&chosen["stdout-path"]), ["/pl011@9000000"]);
        assert!(t.contains_key("/pl011@9000000"), "stdout-path points to an existing node");
        assert_eq!(cells(&chosen["linux,initrd-start"]), [0, 0x4800_0000]);
        assert_eq!(cells(&chosen["linux,initrd-end"]), [0, 0x4810_0000]);

        let mem = &t["/memory@40000000"];
        assert_eq!(strs(&mem["device_type"]), ["memory"]);
        assert_eq!(cells(&mem["reg"]), [0, 0x4000_0000, 0, 0x8000_0000]);

        let cpu = &t["/cpus/cpu@0"];
        assert_eq!(strs(&cpu["enable-method"]), ["psci"]);
        assert_eq!(strs(&cpu["device_type"]), ["cpu"]);
        assert_eq!(cells(&t["/cpus"]["#size-cells"]), [0]);
        assert_eq!(strs(&t["/psci"]["method"]), ["hvc"]);
        assert_eq!(strs(&t["/psci"]["compatible"]), ["arm,psci-1.0", "arm,psci-0.2"]);

        let gic = &t["/intc@8000000"];
        assert_eq!(strs(&gic["compatible"]), ["arm,gic-v3"]);
        assert!(gic["interrupt-controller"].is_empty());
        assert_eq!(cells(&gic["reg"]), [0, 0x0800_0000, 0, 0x1_0000, 0, 0x080A_0000, 0, 0x2_0000]);
        assert_eq!(cells(&gic["phandle"]), [PHANDLE_GIC]);

        let timer = &t["/timer"];
        assert_eq!(strs(&timer["compatible"]), ["arm,armv8-timer"]);
        assert_eq!(cells(&timer["interrupts"]), [1, 13, 4, 1, 14, 4, 1, 11, 4, 1, 10, 4]);

        let clk = &t["/apb-pclk"];
        assert_eq!(cells(&clk["clock-frequency"]), [24_000_000]);
        assert_eq!(cells(&clk["phandle"]), [PHANDLE_CLK]);

        let uart = &t["/pl011@9000000"];
        assert_eq!(strs(&uart["compatible"]), ["arm,pl011", "arm,primecell"]);
        assert_eq!(cells(&uart["reg"]), [0, 0x0900_0000, 0, 0x1000]);
        assert_eq!(cells(&uart["interrupts"]), [0, 1, 4]);
        assert_eq!(cells(&uart["clocks"]), [PHANDLE_CLK, PHANDLE_CLK]);
        assert_eq!(strs(&uart["clock-names"]), ["uartclk", "apb_pclk"]);

        let rtc = &t["/pl031@9010000"];
        assert_eq!(strs(&rtc["compatible"]), ["arm,pl031", "arm,primecell"]);
        assert_eq!(cells(&rtc["interrupts"]), [0, 2, 4]);

        // GPIO and power key: the same values as QEMU 10.0's DTB
        // (`-M virt,dumpdtb=`), apart from the phandle number.
        let gpio = &t["/pl061@9030000"];
        assert_eq!(strs(&gpio["compatible"]), ["arm,pl061", "arm,primecell"]);
        assert_eq!(cells(&gpio["reg"]), [0, 0x0903_0000, 0, 0x1000]);
        assert_eq!(cells(&gpio["interrupts"]), [0, 7, 4]);
        assert_eq!(cells(&gpio["#gpio-cells"]), [2]);
        assert!(gpio["gpio-controller"].is_empty());
        assert_eq!(cells(&gpio["clocks"]), [PHANDLE_CLK]);
        assert_eq!(strs(&gpio["clock-names"]), ["apb_pclk"]);
        assert_eq!(cells(&gpio["phandle"]), [PHANDLE_GPIO]);
        assert_eq!(strs(&t["/gpio-keys"]["compatible"]), ["gpio-keys"]);
        let key = &t["/gpio-keys/poweroff"];
        assert_eq!(cells(&key["gpios"]), [PHANDLE_GPIO, 3, 0]);
        assert_eq!(cells(&key["linux,code"]), [0x74]);
        assert_eq!(strs(&key["label"]), ["GPIO Key Poweroff"]);

        let v0 = &t["/virtio_mmio@a000000"];
        assert_eq!(cells(&v0["interrupts"]), [0, 16, 1]);
        let v31 = &t["/virtio_mmio@a003e00"];
        assert_eq!(cells(&v31["reg"]), [0, 0x0A00_3E00, 0, 0x200]);
        assert_eq!(cells(&v31["interrupts"]), [0, 47, 1]);
    }

    #[test]
    fn stringhe_deduplicate() {
        let dtb = virt_dtb(&VirtDtbConfig::default());
        let off = be32(&dtb, 12) as usize;
        let strings = &dtb[off..];
        let n = strings.split(|&c| c == 0).filter(|s| *s == b"compatible").count();
        assert_eq!(n, 1);
    }

    #[test]
    fn builder_con_riserve_e_tipi() {
        let mut b = FdtBuilder::new();
        b.reserve_memory(0x1000, 0x2000)
            .boot_cpuid(3)
            .begin_node("")
            .prop_u32("a", 7)
            .prop_u64("b", 0x1_0000_0002)
            .prop_str("c", "xy")
            .prop_u32_list("d", &[1, 2])
            .prop_strs("e", &["p", "q"])
            .begin_node("figlio@1")
            .prop_bytes("f", &[9, 8, 7])
            .end_node()
            .end_node();
        let dtb = b.finish().unwrap();
        assert_eq!(be32(&dtb, 28), 3);
        assert_eq!(be32(&dtb, 8), 40 + 32, "one reservation plus the terminator");
        assert_eq!(&dtb[40..56], &[0, 0, 0, 0, 0, 0, 0x10, 0, 0, 0, 0, 0, 0, 0, 0x20, 0]);
        let t = parse(&dtb);
        assert_eq!(cells(&t["/"]["a"]), [7]);
        assert_eq!(cells(&t["/"]["b"]), [1, 2]);
        assert_eq!(strs(&t["/"]["c"]), ["xy"]);
        assert_eq!(cells(&t["/"]["d"]), [1, 2]);
        assert_eq!(strs(&t["/"]["e"]), ["p", "q"]);
        assert_eq!(t["/figlio@1"]["f"], [9, 8, 7]);
    }

    #[test]
    fn errori_del_builder() {
        let mut b = FdtBuilder::new();
        b.begin_node("");
        assert_eq!(b.finish(), Err(FdtError::UnclosedNodes(1)));
        assert_eq!(FdtBuilder::new().finish(), Err(FdtError::Empty));
        let mut b = FdtBuilder::new();
        b.end_node();
        assert_eq!(b.finish(), Err(FdtError::UnmatchedEnd));
        let mut b = FdtBuilder::new();
        b.prop_u32("x", 1);
        assert_eq!(b.finish(), Err(FdtError::PropertyOutsideNode("x".into())));
        let mut b = FdtBuilder::new();
        b.begin_node("").begin_node("").end_node().end_node();
        assert_eq!(b.finish(), Err(FdtError::InvalidName(String::new())));
        let mut b = FdtBuilder::new();
        b.begin_node("").end_node().begin_node("").end_node();
        assert_eq!(b.finish(), Err(FdtError::MultipleRoots));
        let mut b = FdtBuilder::new();
        b.begin_node("radice");
        assert!(matches!(b.finish(), Err(FdtError::InvalidName(_))));
    }
}
