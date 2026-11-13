//! Compresses the large built-in resources into OUT_DIR: they are mostly text and zero padding
//! (popfe.ini 1.9 MB → ~110 KB, BASE.PBP 1.4 MB → ~60 KB), so every binary embedding the engine
//! is ~3 MB smaller. The readable originals stay in `resources/`; they are inflated once at run time.

use std::io::Write;
use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    for (src, dst) in [("../../resources/popfe.ini", "popfe.ini.z"), ("../../resources/BASE.PBP", "BASE.PBP.z")] {
        println!("cargo::rerun-if-changed={src}");
        let data = std::fs::read(src).unwrap_or_else(|e| panic!("{src}: {e}"));
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        z.write_all(&data).expect("in-memory write");
        std::fs::write(out.join(dst), z.finish().expect("in-memory write")).unwrap_or_else(|e| panic!("{dst}: {e}"));
    }
}
