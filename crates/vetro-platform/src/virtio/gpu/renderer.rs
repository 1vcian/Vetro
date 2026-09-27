//! The 3D side of virtio-gpu (§5.7.6.8, `VIRTIO_GPU_F_VIRGL`): contexts,
//! 3D resources, transfers and command submission are forwarded to a
//! [`Renderer3d`] (ADR 0036: gfxstream in `vetro-gfxstream`). The device keeps
//! only what the virtio protocol itself defines (ids, backing, scanouts);
//! what a 3D resource contains is the renderer's business.

use core::any::Any;

use super::super::GuestRam;
use super::Rect;

/// Arguments of RESOURCE_CREATE_3D (`struct virtio_gpu_resource_create_3d`),
/// with the virgl/Gallium meaning of target, format and bind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Create3d {
    pub target: u32,
    pub format: u32,
    pub bind: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub array_size: u32,
    pub last_level: u32,
    pub nr_samples: u32,
    pub flags: u32,
}

/// `struct virtio_gpu_box`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Box3d {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub w: u32,
    pub h: u32,
    pub d: u32,
}

/// TRANSFER_TO_HOST_3D / TRANSFER_FROM_HOST_3D
/// (`struct virtio_gpu_transfer_host_3d`): the box of the resource, and where
/// it lies in the backing (`offset`, rows `stride` bytes apart, layers
/// `layer_stride` apart; 0 = tightly packed).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Transfer3d {
    pub bx: Box3d,
    pub offset: u64,
    pub level: u32,
    pub stride: u32,
    pub layer_stride: u32,
}

/// The guest memory of a resource (its ATTACH_BACKING entries in order), seen
/// as one contiguous range. Reads and writes past the entries are cut short,
/// like QEMU's `iov_to_buf`/`iov_from_buf`.
pub struct Backing<'a> {
    ram: &'a mut dyn GuestRam,
    ents: &'a [(u64, u32)],
}

impl<'a> Backing<'a> {
    pub(super) fn new(ram: &'a mut dyn GuestRam, ents: &'a [(u64, u32)]) -> Self {
        Self { ram, ents }
    }

    /// Total bytes of the entries.
    pub fn len(&self) -> u64 {
        self.ents.iter().map(|&(_, l)| u64::from(l)).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copies `out.len()` bytes from `offset`; returns how many the backing
    /// had (the rest of `out` is left as it is).
    pub fn read(&self, offset: u64, out: &mut [u8]) -> usize {
        let mut done = 0usize;
        let mut off = offset;
        for &(addr, len) in self.ents {
            if done == out.len() {
                break;
            }
            let len = u64::from(len);
            if off >= len {
                off -= len;
                continue;
            }
            let n = ((len - off) as usize).min(out.len() - done);
            // The entries were checked at ATTACH: RAM doesn't shrink.
            let _ = self.ram.read(addr + off, &mut out[done..done + n]);
            done += n;
            off = 0;
        }
        done
    }

    /// Copies `data` to `offset`; returns how many bytes fitted.
    pub fn write(&mut self, offset: u64, data: &[u8]) -> usize {
        let mut done = 0usize;
        let mut off = offset;
        for &(addr, len) in self.ents {
            if done == data.len() {
                break;
            }
            let len = u64::from(len);
            if off >= len {
                off -= len;
                continue;
            }
            let n = ((len - off) as usize).min(data.len() - done);
            let _ = self.ram.write(addr + off, &data[done..done + n]);
            done += n;
            off = 0;
        }
        done
    }
}

/// A capability set announced to the driver (GET_CAPSET_INFO/GET_CAPSET).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capset {
    pub id: u32,
    pub max_version: u32,
    pub data: Vec<u8>,
}

/// Renderer of the 3D commands. Errors are virtio-gpu response codes
/// (`RESP_ERR_*`). Execution is synchronous: when a method returns, the
/// command is complete (fences are signalled with the response).
pub trait Renderer3d: Any {
    /// Capability sets (`num_capsets` in the configuration space).
    fn capsets(&self) -> Vec<Capset> {
        Vec::new()
    }
    /// CTX_CREATE: `name` is the debug name (at most 64 bytes).
    fn context_create(&mut self, ctx: u32, context_init: u32, name: &[u8]) -> Result<(), u32>;
    fn context_destroy(&mut self, ctx: u32);
    fn context_attach(&mut self, ctx: u32, res: u32);
    fn context_detach(&mut self, ctx: u32, res: u32);
    /// RESOURCE_CREATE_3D; returns the host memory the resource takes
    /// (counted against `max_hostmem`).
    fn resource_create(&mut self, res: u32, args: &Create3d) -> Result<u64, u32>;
    /// RESOURCE_UNREF (or device reset).
    fn resource_destroy(&mut self, res: u32);
    /// TRANSFER_TO_HOST_3D: guest memory → resource.
    fn transfer_to_host(
        &mut self,
        ctx: u32,
        res: u32,
        t: &Transfer3d,
        backing: &mut Backing<'_>,
    ) -> Result<(), u32>;
    /// TRANSFER_FROM_HOST_3D: resource → guest memory.
    fn transfer_from_host(
        &mut self,
        ctx: u32,
        res: u32,
        t: &Transfer3d,
        backing: &mut Backing<'_>,
    ) -> Result<(), u32>;
    /// SUBMIT_3D: the command buffer of context `ctx`.
    fn submit(&mut self, ctx: u32, cmd: &[u8]) -> Result<(), u32>;
    /// SET_SCANOUT with a 3D resource (`Some`) or turned off (`None`).
    fn scanout(&mut self, scanout: u32, resource: Option<(u32, Rect)>);
    /// RESOURCE_FLUSH of a 3D resource shown on `scanout`; `dirty` in scanout
    /// coordinates.
    fn flush(&mut self, scanout: u32, res: u32, dirty: Rect);
    /// Device reset: every context and resource is gone.
    fn reset(&mut self) {}
    /// Renderer state for snapshots (ADR 0015). The device saves its own part
    /// (ids, backing, scanouts) around it.
    fn save_state(&self, w: &mut vetro_snapshot::Writer);
    fn restore_state(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()>;
}
