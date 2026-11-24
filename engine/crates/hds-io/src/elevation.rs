//! Whether the process has administrative rights.

/// True when the process runs elevated (Windows: the `TokenElevation` of the
/// process token; Unix: effective user 0).
///
/// Advisory, for messages only (`hello.elevated` of the protocol): the
/// authoritative sign that the engine lacks the rights to read a disk is
/// `PermissionDenied` from [`open_source`](crate::open_source). `false` when
/// the question cannot be answered.
pub fn is_elevated() -> bool {
    imp::is_elevated()
}

#[cfg(windows)]
mod imp {
    use std::mem::size_of;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    pub(super) fn is_elevated() -> bool {
        let mut raw: HANDLE = null_mut();
        // SAFETY: the pseudo handle of the current process is always valid; `raw` is a
        // valid place for the token handle.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
            return false;
        }
        // SAFETY: the call succeeded, so `raw` is an open handle that nobody else owns;
        // the owner closes it.
        let token = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut returned = 0u32;
        // SAFETY: the token handle is open; the buffer is a TOKEN_ELEVATION of the size passed.
        let ok = unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenElevation,
                (&raw mut elevation).cast(),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut returned,
            )
        };
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

#[cfg(unix)]
mod imp {
    pub(super) fn is_elevated() -> bool {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }
}

#[cfg(not(any(windows, unix)))]
mod imp {
    pub(super) fn is_elevated() -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn answers_without_failing() {
        let elevated = super::is_elevated();
        // Whatever the answer, it is the same when asked again.
        assert_eq!(elevated, super::is_elevated());
        #[cfg(unix)]
        // SAFETY: geteuid has no preconditions.
        assert_eq!(elevated, unsafe { libc::geteuid() } == 0);
    }
}
