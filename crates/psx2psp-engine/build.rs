//! Compresses the large built-in resources into OUT_DIR: they are mostly text and zero padding
//! (popfe.ini 1.9 MB → ~110 KB, BASE.PBP 1.4 MB → ~60 KB), so every binary embedding the engine
//! is ~3 MB smaller. The readable originals stay in `resources/`; they are inflated once at run time.

use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    for (src, dst) in [("../../resources/popfe.ini", "popfe.ini.z"), ("../../resources/BASE.PBP", "BASE.PBP.z")] {
        println!("cargo::rerun-if-changed={src}");
        let data = std::fs::read(src).unwrap_or_else(|e| panic!("{src}: {e}"));
        let z = miniz_oxide::deflate::compress_to_vec_zlib(&data, 9);
        std::fs::write(out.join(dst), z).unwrap_or_else(|e| panic!("{dst}: {e}"));
    }
}
