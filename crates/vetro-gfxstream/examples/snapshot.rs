//! The GPU snapshot test's two phases (`tests/web/gl.mjs`, ADR 0037): the
//! read-back contents of a snapshot come from a real WebGL2 context, so the
//! browser runs phase A and hands its readback bytes to phase B.
//!
//!   cargo run -p vetro-gfxstream --example snapshot -- save DIR
//!     DIR/snap-a.bin: the scene, then the snapshot's readback batch (last).
//!   cargo run -p vetro-gfxstream --example snapshot -- restore DIR OUTS
//!     the scene again, the snapshot saved with OUTS (what the browser read
//!     back in phase A's last batch), restored into a new renderer, then the
//!     guest redraws: DIR/snap-b.bin (to replay on a fresh WebGL2 context)
//!     and DIR/snap-expected.rgba (the window after the redraw).

use vetro_gfxstream::guest::{Guest, scene, scene_redraw, scene_redraw_expected};
use vetro_gfxstream::{GlExecutor, NullExecutor, Recorder};
use vetro_platform::virtio::gpu::Renderer3d;

/// Answers its first batch with the given bytes.
struct Scripted(Option<Vec<u8>>);

impl GlExecutor for Scripted {
    fn execute(&mut self, _words: &[u32], _blob: &[u8], out: &mut [u8]) {
        if let Some(b) = self.0.take() {
            let n = b.len().min(out.len());
            out[..n].copy_from_slice(&b[..n]);
        }
    }
}

fn take_recording(gu: &mut Guest) -> Vec<u8> {
    let exec = gu.gfx.gl.set_executor(Box::new(NullExecutor::default()));
    let any: Box<dyn std::any::Any> = exec;
    any.downcast::<Recorder<NullExecutor>>().expect("the recorder").log
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = args.get(2).cloned().unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).unwrap();
    match args.get(1).map(String::as_str) {
        Some("save") => {
            let mut gu = Guest::new(Box::new(Recorder::new(NullExecutor::default())));
            scene(&mut gu);
            gu.gfx.flush_ops();
            let mut w = vetro_snapshot::Writer::new();
            gu.gfx.save_state(&mut w);
            let log = take_recording(&mut gu);
            std::fs::write(format!("{dir}/snap-a.bin"), &log).unwrap();
            println!("{dir}/snap-a.bin: {} bytes", log.len());
        }
        Some("restore") => {
            let outs = std::fs::read(&args[3]).unwrap();
            let mut gu = Guest::new(Box::new(NullExecutor::default()));
            scene(&mut gu);
            gu.gfx.flush_ops();
            gu.gfx.gl.set_executor(Box::new(Scripted(Some(outs))));
            let mut w = vetro_snapshot::Writer::new();
            gu.gfx.save_state(&mut w);
            let bytes = w.into_bytes();
            let mut other = Guest::new(Box::new(Recorder::new(NullExecutor::default())));
            other.gfx.restore_state(&mut vetro_snapshot::Reader::new(&bytes)).unwrap();
            scene_redraw(&mut other);
            other.gfx.flush_ops();
            let log = take_recording(&mut other);
            std::fs::write(format!("{dir}/snap-b.bin"), &log).unwrap();
            std::fs::write(format!("{dir}/snap-expected.rgba"), scene_redraw_expected()).unwrap();
            println!("{dir}/snap-b.bin: {} bytes, snapshot {} bytes", log.len(), bytes.len());
        }
        _ => {
            eprintln!("usage: snapshot save DIR | snapshot restore DIR OUTS");
            std::process::exit(2);
        }
    }
}
