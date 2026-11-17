//! The psxrt C core (the PSP runtime's interpreter and devices) on the PC: [`ffi`] for the
//! `psx2psp-sim` tool, [`Machine`] for running a disc from Rust (the converter's code capture).

use std::ffi::{c_char, c_int, c_void};
use std::sync::{Mutex, MutexGuard};

pub mod ffi {
    use super::*;

    #[repr(C)]
    pub struct Sim {
        _opaque: [u8; 0],
    }

    pub type ReadFn = extern "C" fn(*mut c_void, u32, *mut u8) -> c_int;
    pub type TextFn = extern "C" fn(*mut c_void, *const c_char, c_int);
    pub type SelectFn = extern "C" fn(*mut c_void, u32) -> u32;

    unsafe extern "C" {
        pub fn sim_new(user: *mut c_void, rd: ReadFn, disc_end: u32, tty: TextFn, log: TextFn) -> *mut Sim;
        pub fn sim_free(s: *mut Sim);
        pub fn sim_boot(s: *mut Sim) -> c_int;
        pub fn sim_frame(s: *mut Sim) -> c_int;
        pub fn sim_vram(s: *const Sim) -> *const u16;
        pub fn sim_ram(s: *mut Sim) -> *mut u8;
        pub fn sim_pad(s: *mut Sim, buttons: u16);
        pub fn sim_display(s: *const Sim, out: *mut c_int);
        pub fn sim_status(s: *const Sim, out: *mut u32);
        pub fn sim_trace(s: *mut Sim, flags: c_int);
        pub fn sim_lid(s: *mut Sim, open: c_int, disc: c_int);
        pub fn sim_discs(s: *mut Sim, sel: SelectFn, n: u32);
        pub fn sim_trace_to(path: *const c_char, out: *mut u64) -> c_int;
        pub fn sim_poll_bound(s: *mut Sim, pc: u32, gpr: *const u32, raw: u32, room: u32) -> u32;
        pub fn sim_ran() -> *const u8;
        pub fn sim_ran_clear();
    }
}

/// Raw sector size of the disc images the core reads.
pub const SECTOR: usize = 2352;
/// Sectors before the first one of the image (the core's LBAs count them).
pub const PREGAP: u32 = 150;
/// Bytes of PS1 RAM.
pub const RAM: usize = 2 << 20;
/// PS1 pad buttons (active high here; [`Machine::frame`] inverts them for the core).
pub const START: u16 = 1 << 3;
pub const CROSS: u16 = 1 << 14;

/// The core keeps its disc callback in globals: one machine at a time per process.
static ONE: Mutex<()> = Mutex::new(());

type Reader<'a> = Box<dyn FnMut(u32, &mut [u8]) -> bool + 'a>;

extern "C" fn read_cb(user: *mut c_void, lba: u32, raw: *mut u8) -> c_int {
    // SAFETY: `user` is the `Reader` boxed in the `Machine` that owns the core (the core calls
    // this only inside its methods, while what the reader borrows lives), and `raw` is a
    // SECTOR-byte buffer of the core's.
    let read = unsafe { &mut *(user as *mut Reader<'static>) };
    let buf = unsafe { std::slice::from_raw_parts_mut(raw, SECTOR) };
    if lba >= PREGAP && read(lba - PREGAP, buf) { 0 } else { -1 }
}

extern "C" fn quiet(_user: *mut c_void, _s: *const c_char, _n: c_int) {}

/// A PS1 running a disc on the core's interpreter, with no output but its memories.
pub struct Machine<'a> {
    sim: *mut ffi::Sim,
    _read: Box<Reader<'a>>,
    _one: MutexGuard<'static, ()>,
}

impl<'a> Machine<'a> {
    /// A machine with a disc of `sectors` raw sectors, which `read(lba, buf)` fills (`lba` from
    /// the first sector of the image), booted through the BIOS; `None` when the core cannot
    /// allocate or boot. Waits for any other machine of the process to be dropped.
    pub fn boot(sectors: u32, read: impl FnMut(u32, &mut [u8]) -> bool + 'a) -> Option<Machine<'a>> {
        let one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let mut read: Box<Reader<'a>> = Box::new(Box::new(read));
        let user = &mut *read as *mut Reader<'a> as *mut c_void;
        // SAFETY: `read` lives in the machine as long as the core does (freed in drop first).
        let sim = unsafe { ffi::sim_new(user, read_cb, sectors + PREGAP, quiet, quiet) };
        if sim.is_null() {
            return None;
        }
        let m = Machine { sim, _read: read, _one: one };
        // SAFETY: no core runs while this machine holds ONE.
        unsafe { ffi::sim_ran_clear() };
        // SAFETY: `sim` is valid until drop.
        (unsafe { ffi::sim_boot(m.sim) } == 0).then_some(m)
    }

    /// Runs one frame with `buttons` held; false when the guest stopped (exit or fatal error).
    pub fn frame(&mut self, buttons: u16) -> bool {
        // SAFETY: `sim` is valid until drop.
        unsafe {
            ffi::sim_pad(self.sim, !buttons);
            ffi::sim_frame(self.sim) == 0
        }
    }

    /// The 2 MB of RAM.
    pub fn ram(&self) -> &[u8] {
        // SAFETY: the core's RAM is RAM bytes, valid until drop, written only inside `frame`.
        unsafe { std::slice::from_raw_parts(ffi::sim_ram(self.sim), RAM) }
    }

    /// The words of RAM the guest ran since boot or the last call, 1 byte per word (RAM / 4
    /// bytes, nonzero where it ran); the map is cleared for the next frames.
    pub fn take_ran(&mut self) -> Vec<u8> {
        // SAFETY: the map is RAM / 4 bytes of the core's, written only inside `frame`, and this
        // machine holds ONE.
        unsafe {
            let v = std::slice::from_raw_parts(ffi::sim_ran(), RAM / 4).to_vec();
            ffi::sim_ran_clear();
            v
        }
    }
}

impl Drop for Machine<'_> {
    fn drop(&mut self) {
        // SAFETY: `sim` came from sim_new and is freed once.
        unsafe { ffi::sim_free(self.sim) }
    }
}
