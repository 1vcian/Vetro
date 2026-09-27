//! Writes the op stream of the synthetic scene (`vetro_gfxstream::guest::scene`)
//! and the image it must produce, for the browser rendering test
//! (`tests/web/gl.mjs`, ADR 0037):
//!
//!   cargo run -p vetro-gfxstream --example scene -- OUT_DIR
//!
//! OUT_DIR/scene.bin is a `Recorder` log (replayed with `replayRecording` of
//! web/app/gl.mjs); OUT_DIR/scene-expected.rgba the expected pixels of the
//! window's ColorBuffer (texture row 0 first, RGBA, 64x64).

use vetro_gfxstream::guest::{Guest, scene, scene_expected};
use vetro_gfxstream::{GlExecutor, NullExecutor, Recorder};

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let mut gu = Guest::new(Box::new(Recorder::new(NullExecutor::default())));
    scene(&mut gu);
    gu.gfx.flush_ops();
    let exec = std::mem::replace(&mut gu.gfx.gl.exec, Box::new(NullExecutor::default()));
    let any: Box<dyn std::any::Any> = exec;
    let rec = any.downcast::<Recorder<NullExecutor>>().expect("the recorder");
    let _: &dyn GlExecutor = &*rec;
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(format!("{dir}/scene.bin"), &rec.log).unwrap();
    std::fs::write(format!("{dir}/scene-expected.rgba"), scene_expected()).unwrap();
    println!("{dir}/scene.bin: {} bytes", rec.log.len());
}
