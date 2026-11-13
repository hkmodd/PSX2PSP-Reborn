//! Windows-only cap on the window size.
//!
//! iced configures the wgpu surface with the client size, and wgpu panics when either side
//! exceeds the device's max texture extent (8 192 with the limits iced requests). Windows lets
//! any process `SetWindowPos` our window to any size — up to 65 535 px — and does not apply the
//! max tracking size there. A subclass caps every size change in `WM_WINDOWPOSCHANGING`, so the
//! window, and therefore its client area, never outgrows what the surface accepts.

/// Largest window side, in physical pixels. The client area is never larger than the window.
#[cfg_attr(not(windows), allow(dead_code))]
pub const MAX_EXTENT: i32 = 8192;

/// A requested window size capped to `MAX_EXTENT` on each side.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn cap(cx: i32, cy: i32) -> (i32, i32) {
    (cx.min(MAX_EXTENT), cy.min(MAX_EXTENT))
}

/// Installs the size cap on `window`. Must run on the thread that owns it (`iced::window::run`
/// does). No-op off Windows or without a Win32 handle.
pub fn guard_resize(window: &dyn iced::window::Window) {
    #[cfg(windows)]
    {
        use iced::window::raw_window_handle::RawWindowHandle;
        if let Ok(handle) = window.window_handle()
            && let RawWindowHandle::Win32(h) = handle.as_raw()
        {
            // SAFETY: the handle comes from the live window on its own thread; the subclass proc
            // keeps no reference data, and Windows drops it with the window.
            unsafe { imp::install(h.hwnd.get() as _) };
        }
    }
    #[cfg(not(windows))]
    let _ = window;
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
    use windows_sys::Win32::UI::WindowsAndMessaging::{SWP_NOSIZE, WINDOWPOS, WM_WINDOWPOSCHANGING};

    const ID: usize = 0x5053_5850; // "PSXP"

    unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _: usize, _: usize) -> LRESULT {
        if msg == WM_WINDOWPOSCHANGING && lp != 0 {
            // SAFETY: for this message lParam points to the WINDOWPOS Windows is about to apply.
            let pos = unsafe { &mut *(lp as *mut WINDOWPOS) };
            if pos.flags & SWP_NOSIZE == 0 {
                (pos.cx, pos.cy) = super::cap(pos.cx, pos.cy);
            }
        }
        unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
    }

    pub unsafe fn install(hwnd: HWND) {
        unsafe { SetWindowSubclass(hwnd, Some(proc), ID, 0) };
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_EXTENT, cap};

    #[test]
    fn oversized_requests_are_capped() {
        // The observed crash: a window pushed to 1500 × 65 535 (client 1484 × 65 496).
        assert_eq!(cap(1500, 65_535), (1500, MAX_EXTENT));
        assert_eq!(cap(20_000, 9000), (MAX_EXTENT, MAX_EXTENT));
    }

    #[test]
    fn real_sizes_pass_through() {
        for (w, h) in [(0, 0), (1, 1), (1500, 960), (7680, 4320), (MAX_EXTENT, MAX_EXTENT)] {
            assert_eq!(cap(w, h), (w, h));
        }
    }
}
