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

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_SUCCESS, GetLastError, LUID, SetLastError,
};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, GetTokenInformation, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
    SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY, TokenPrivileges,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::util::to_wide;

/// `SE_BACKUP_NAME`. Spelled out because the constant is a string, not a number.
const SE_BACKUP_NAME: &str = "SeBackupPrivilege";

/// `ERROR_NOT_ALL_ASSIGNED`: the call succeeded but the token does not hold the privilege.
const ERROR_NOT_ALL_ASSIGNED: u32 = 1300;

/// What happened when the privilege was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupPrivilege {
    /// Held and now enabled. Protected files should open.
    Enabled,
    /// The token does not contain the privilege — the process is not running as an
    /// administrator. Expected, and not an error.
    NotHeld,
    /// The privilege could not be switched on for some other reason, carrying the code the
    /// operating system gave. The code is kept rather than flattened to "failed" because the
    /// difference between "access denied" and "bad length" decides what to do next.
    Failed(u32),
}

impl BackupPrivilege {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Enabled)
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
            let code = GetLastError();
            CloseHandle(token);
            return BackupPrivilege::Failed(code);
        }

        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };

        // `PreviousState` and `ReturnLength` are both null, and the length is zero to match.
        // Supplying a buffer for the previous state is documented as optional, and passing one
        // this API rejects fails the *entire* call with ERROR_NOACCESS — losing the privilege
        // for the sake of reporting whether it was already on, which is not a trade worth
        // making. That is exactly what happened on the first attempt.
        //
        // `AdjustTokenPrivileges` reports success even when it changed nothing, so the only
        // reliable signal is the last-error code. That code is only *written* when the call
        // fails, so it must be cleared first — otherwise a stale value left by any earlier call
        // in this thread is read as a failure, and a call that in fact succeeded is reported as
        // one. That was the second bug here.
        SetLastError(ERROR_SUCCESS);

        AdjustTokenPrivileges(
            token,
            0,
            &privileges,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );

        let code = GetLastError();
        let outcome = match code {
            ERROR_SUCCESS => BackupPrivilege::Enabled,
            // The call went through but the token does not contain the privilege, which means
            // the process is not running as an administrator.
            ERROR_NOT_ALL_ASSIGNED => BackupPrivilege::NotHeld,
            other => BackupPrivilege::Failed(other),
        };

        CloseHandle(token);
        outcome
    }
}

/// Whether the process token contains `SeBackupPrivilege` at all, enabled or not.
///
/// This is the question that actually matters when the enable call does not work: a token
/// either carries the privilege or it does not, and "not an administrator" and "the enable call
/// failed for another reason" need different responses. Enabling cannot be tested for by
/// enabling — that is circular — so the token is enumerated directly.
pub fn holds_backup_privilege() -> bool {
    // SAFETY: buffers are sized by the API itself before being filled, and `token` is closed
    // exactly once on every path.
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }

        // The first call is expected to fail with ERROR_INSUFFICIENT_BUFFER; its purpose is to
        // report how large the buffer needs to be.
        let mut needed: u32 = 0;
        GetTokenInformation(
            token,
            TokenPrivileges,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );

        if needed == 0 {
            CloseHandle(token);
            return false;
        }

        let mut buffer = vec![0u8; needed as usize];
        let ok = GetTokenInformation(
            token,
            TokenPrivileges,
            buffer.as_mut_ptr() as *mut core::ffi::c_void,
            needed,
            &mut needed,
        );
        CloseHandle(token);

        if ok == 0 {
            return false;
        }

        let mut wanted = LUID {
            LowPart: 0,
            HighPart: 0,
        };
        let name = to_wide(SE_BACKUP_NAME);
        if LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), &mut wanted) == 0 {
            return false;
        }

        // TOKEN_PRIVILEGES is a count followed by that many LUID_AND_ATTRIBUTES. The count is a
        // 4-byte field, so the array begins 4 bytes in.
        let count = *(buffer.as_ptr() as *const u32) as usize;
        let entries = buffer.as_ptr().add(4) as *const LUID_AND_ATTRIBUTES;

        (0..count).any(|i| {
            let entry = &*entries.add(i);
            entry.Luid.LowPart == wanted.LowPart && entry.Luid.HighPart == wanted.HighPart
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_privilege_name_resolves_to_a_luid() {
        // Resolving a privilege name to its LUID does not require holding it, so this must
        // succeed on any Windows system. If it fails, the name is wrong or the call is being
        // made incorrectly — and every later decision is built on a bad LUID.
        let name = to_wide(SE_BACKUP_NAME);
        let mut luid = LUID {
            LowPart: 0,
            HighPart: 0,
        };
        let ok = unsafe { LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), &mut luid) };
        assert_ne!(ok, 0, "SeBackupPrivilege should resolve to a LUID");
    }

    #[test]
    fn requesting_the_privilege_never_panics_and_reports_an_outcome() {
        // The result depends on how the test process was started, so only the shape is
        // asserted. What matters is that an unprivileged process gets `NotHeld` rather than a
        // panic or a false success.
        let outcome = enable_backup_privilege();
        let _ = outcome.is_active();
    }
}
