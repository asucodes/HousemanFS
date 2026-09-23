//! Shared helpers for the wide-character Win32 API.

use std::io;

use windows_sys::Win32::Foundation::GetLastError;

/// Convert a Rust string into a NUL-terminated UTF-16 buffer.
pub(crate) fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Convert a NUL-terminated UTF-16 buffer back into a Rust string.
///
/// Uses a lossy conversion on purpose. Windows filenames are not guaranteed to be valid UTF-16
/// — lone surrogates are legal on NTFS — and a scan must not abort or silently skip a file
/// because its name is unusual. The replacement character makes such names visible instead.
pub(crate) fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Turn the calling thread's last-error code into an [`io::Error`].
pub(crate) fn last_error() -> io::Error {
    // SAFETY: no preconditions; returns the calling thread's last-error code.
    let code = unsafe { GetLastError() };
    io::Error::from_raw_os_error(code as i32)
}
