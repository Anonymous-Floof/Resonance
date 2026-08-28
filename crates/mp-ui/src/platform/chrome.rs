//! Platform-specific window chrome.
//!
//! Windows paints the title bar from the *system* accent colour and light/dark
//! setting, neither of which knows about our theme. Left alone, a user with a
//! bright accent gets a vivid title bar sitting on top of a near-black shell.
//! These calls bring the non-client area into line.
//!
//! Everything here is best-effort: an unsupported Windows build simply returns
//! an error code we ignore, and non-Windows targets get no-ops. The full
//! platform integration layer (media keys, taskbar controls) arrives in M6;
//! this is only the chrome the shell needs to not look broken.

/// Applies dark title bar and, where supported, the Mica backdrop.
///
/// `dark` should match the palette actually in use, not the OS preference.
#[allow(unused_variables)]
pub fn apply_window_chrome(
    handle: &impl raw_window_handle::HasWindowHandle,
    dark: bool,
    mica: bool,
) {
    #[cfg(target_os = "windows")]
    windows_impl::apply(handle, dark, mica);
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    type Hwnd = isize;

    // Undocumented before Windows 10 20H1, where it became attribute 20.
    // Build 18985 and earlier used 19, so we try both.
    const DWMWA_USE_IMMERSIVE_DARK_MODE_OLD: u32 = 19;
    const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
    /// Windows 11 22H2+. Ignored (returns an error) on older builds.
    const DWMWA_SYSTEMBACKDROP_TYPE: u32 = 38;

    /// `DWMSBT_MAINWINDOW` - the Mica backdrop.
    const DWMSBT_MAINWINDOW: i32 = 2;
    /// `DWMSBT_NONE` - plain solid backdrop.
    const DWMSBT_NONE: i32 = 1;

    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmSetWindowAttribute(
            hwnd: Hwnd,
            attribute: u32,
            value: *const core::ffi::c_void,
            size: u32,
        ) -> i32;
    }

    pub fn apply(handle: &impl HasWindowHandle, dark: bool, mica: bool) {
        let Some(hwnd) = hwnd_of(handle) else {
            tracing::debug!("no Win32 window handle; skipping window chrome");
            return;
        };

        set_dark_titlebar(hwnd, dark);
        set_backdrop(hwnd, mica);
    }

    fn hwnd_of(handle: &impl HasWindowHandle) -> Option<Hwnd> {
        match handle.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(win32) => Some(win32.hwnd.get()),
            _ => None,
        }
    }

    fn set_dark_titlebar(hwnd: Hwnd, dark: bool) {
        // Win32 BOOL is a 32-bit int, not a Rust bool.
        let value: i32 = i32::from(dark);

        // Try the current attribute first, then the pre-20H1 one. A failure
        // here just means the title bar keeps its default colour.
        for attribute in [
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            DWMWA_USE_IMMERSIVE_DARK_MODE_OLD,
        ] {
            let result = unsafe {
                DwmSetWindowAttribute(
                    hwnd,
                    attribute,
                    (&raw const value).cast(),
                    size_of::<i32>() as u32,
                )
            };

            if result >= 0 {
                return;
            }
        }

        tracing::debug!("dark title bar not supported on this Windows build");
    }

    fn set_backdrop(hwnd: Hwnd, mica: bool) {
        let value: i32 = if mica { DWMSBT_MAINWINDOW } else { DWMSBT_NONE };

        let result = unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_SYSTEMBACKDROP_TYPE,
                (&raw const value).cast(),
                size_of::<i32>() as u32,
            )
        };

        if result < 0 {
            // Expected on Windows 10 - Mica is 11-only.
            tracing::debug!("system backdrop not supported on this Windows build");
        }
    }
}
