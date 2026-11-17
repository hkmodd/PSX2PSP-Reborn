//! Compiles the psxrt C core (shared with the PSP runtime) and the simulator's shim.

use std::path::Path;

fn main() {
    let core = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../psp/psxrt/core");
    let mut build = cc::Build::new();
    build.include(&core).file("csrc/sim.c").warnings(true).define("_CRT_SECURE_NO_WARNINGS", None).define("PSXRT_PC_TRACE", None).opt_level(2);
    for entry in std::fs::read_dir(&core).expect("psp/psxrt/core") {
        let path = entry.expect("core entry").path();
        if path.extension().is_some_and(|e| e == "c" || e == "h") {
            println!("cargo:rerun-if-changed={}", path.display());
        }
        if path.extension().is_some_and(|e| e == "c") {
            build.file(&path);
        }
    }
    // ge.c, the PSP's GE translator, with its PC host (PSX2PSP_SIM_GE: list hashes in the log)
    let psxrt = core.join("..");
    build.define("GEL_ADDR", "geh_addr");
    for f in ["ge.c", "ge_host_pc.c", "gel_hash.c", "floating.c", "coderun.c"] {
        build.file(psxrt.join(f));
    }
    build.include(&psxrt);
    for f in ["ge.c", "ge.h", "ge_host.h", "ge_host_pc.c", "ge_dump.h", "gel.h", "gel_hash.c", "gel_hash.h", "floating.c", "floating.h", "coderun.c", "coderun.h"] {
        println!("cargo:rerun-if-changed={}", psxrt.join(f).display());
    }
    println!("cargo:rerun-if-changed=csrc/sim.c");
    build.compile("psxrt_core");
}
