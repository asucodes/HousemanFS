//! Whether this process can read the parts of a volume that are normally protected.
//!
//! This matters because the answer changes what the accounting *means*. Run unprivileged, a
//! scan cannot open `System Volume Information` — which is where shadow copies live, and which
//! on a volume of any size can hold tens of gigabytes. Run elevated, it can.
//!
//! So the number reported to the user is only interpretable alongside this fact. Rather than
//! leaving the user to guess, the tool states which mode produced the figures.

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Whether the current process is running with an elevated token.
///
/// Returns `false` on any failure. Failing closed is the right default here: if we cannot
/// determine that we are elevated, we should behave — and report — as though we are not, rather
/// than claiming coverage we may not have.
pub fn is_elevated() -> bool {
    // SAFETY: all handles and buffers are valid locals; `token` is closed exactly once on every
    // path below. `GetCurrentProcess` returns a pseudo-handle that must not be closed.
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();

        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }

        let mut elevation: TOKEN_ELEVATION = std::mem::zeroed();
        let mut returned: u32 = 0;

        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        );

        CloseHandle(token);

        ok != 0 && elevation.TokenIsElevated != 0
    }
}
