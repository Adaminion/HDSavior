//! Process-wide settings made at start.

/// Stops Windows from showing a system dialog when a drive without medium
/// (an empty card reader) is touched: `SetErrorMode(SEM_FAILCRITICALERRORS)`.
/// The error then reaches the caller as an ordinary I/O error. Does nothing
/// on other systems.
pub fn suppress_error_dialogs() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Diagnostics::Debug::{
            SetErrorMode, SEM_FAILCRITICALERRORS,
        };
        // SAFETY: SetErrorMode only changes a flag of this process; it takes
        // a plain integer and has no memory-safety preconditions.
        unsafe {
            SetErrorMode(SEM_FAILCRITICALERRORS);
        }
    }
}
