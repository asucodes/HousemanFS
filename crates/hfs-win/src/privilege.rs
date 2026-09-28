//! Enabling the backup privilege.
//!
//! Some files and directories refuse to open no matter how elevated the process is: they are
//! owned by SYSTEM, held open by a service, or protected by an ACL that administrators do not
//! appear in. For those, elevation is not the missing ingredient — a *privilege* is.
//!
//! `SeBackupPrivilege` exists precisely for this. It is what backup software uses to read a
//! machine faithfully, and with it `CreateFileW` bypasses the access check rather than failing
//! it. It is documented, it is disabled by default even for administrators, and it must be
//! explicitly requested — which is the right shape for this project: nothing here happens
//! silently, and nothing escalates on its own.
//!
//! Enabling it is not a guarantee. The privilege must be *held* — a standard user does not have
//! it in their token at all, so the call fails and the caller is told so plainly rather than
//! left to wonder why the numbers moved.

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, GetLastError, LUID};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::util::to_wide;

/// `SE_BACKUP_NAME`. Spelled out because the constant is a string, not a number.
const SE_BACKUP_NAME: &str = "SeBackupPrivilege";

/// What happened when the privilege was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupPrivilege {
    /// Held and now enabled. Protected files should open.
    Enabled,
    /// Already enabled before this call.
    AlreadyEnabled,
    /// The token does not contain the privilege — the process is not running as an
    /// administrator. Expected, and not an error.
    NotHeld,
    /// The privilege is present but could not be switched on.
    Failed,
}

impl BackupPrivilege {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Enabled | Self::AlreadyEnabled)
    }
}

/// Request `SeBackupPrivilege` for the current process.
///
/// Never fails hard: a caller that cannot obtain the privilege should still scan, just with
/// less visibility, and report which of the two it got.
pub fn enable_backup_privilege() -> BackupPrivilege {
    // SAFETY: every handle and buffer is a valid local; `token` is closed exactly once below.
    // `GetCurrentProcess` returns a pseudo-handle that must not be closed.
    unsafe {
        let mut token = std::ptr::null_mut();

        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        ) == 0
        {
            return BackupPrivilege::NotHeld;
        }

        let mut luid = LUID {
            LowPart: 0,
            HighPart: 0,
        };
        let name = to_wide(SE_BACKUP_NAME);

        if LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), &mut luid) == 0 {
            CloseHandle(token);
            return BackupPrivilege::Failed;
        }

        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };

        let previous = TOKEN_PRIVILEGES {
            PrivilegeCount: 0,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: LUID {
                    LowPart: 0,
                    HighPart: 0,
                },
                Attributes: 0,
            }],
        };

        // `AdjustTokenPrivileges` reports success even when it changed nothing, so the only
        // reliable signal is the last-error code. Checking the return value alone would report
        // success to a standard user who holds nothing.
        AdjustTokenPrivileges(
            token,
            0,
            &privileges,
            std::mem::size_of::<TOKEN_PRIVILEGES>() as u32,
            &previous as *const _ as *mut _,
            std::ptr::null_mut(),
        );

        let outcome = if GetLastError() != ERROR_SUCCESS {
            BackupPrivilege::NotHeld
        } else if previous.PrivilegeCount > 0
            && previous.Privileges[0].Attributes & SE_PRIVILEGE_ENABLED != 0
        {
            BackupPrivilege::AlreadyEnabled
        } else {
            BackupPrivilege::Enabled
        };

        CloseHandle(token);
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requesting_the_privilege_never_panics_and_reports_an_outcome() {
        // The result depends on how the test process was started, so only the shape is
        // asserted. What matters is that an unprivileged process gets `NotHeld` rather than a
        // panic or a false success.
        let outcome = enable_backup_privilege();
        let _ = outcome.is_active();
    }
}
