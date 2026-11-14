//! Compiles the psxrt C core (shared with the PSP runtime) and the simulator's shim.

use std::path::Path;

fn main() {
    let core = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../psp/psxrt/core");
    let mut build = cc::Build::new();
    build.include(&core).file("csrc/sim.c").warnings(true).define("_CRT_SECURE_NO_WARNINGS", None).opt_level(2);
    for entry in std::fs::read_dir(&core).expect("psp/psxrt/core") {
        let path = entry.expect("core entry").path();
        if path.extension().is_some_and(|e| e == "c" || e == "h") {
            println!("cargo:rerun-if-changed={}", path.display());
        }
        if path.extension().is_some_and(|e| e == "c") {
            build.file(&path);
        }
    }
    println!("cargo:rerun-if-changed=csrc/sim.c");
    build.compile("psxrt_core");
}
