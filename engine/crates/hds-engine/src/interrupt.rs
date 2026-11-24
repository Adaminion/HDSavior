//! Ctrl-C for the command-line interface (ARCHITECTURE.md section 5.6).
//!
//! While a [`CtrlC`] is alive, the first Ctrl-C (`SIGINT` on Unix; Ctrl-C
//! or Ctrl-Break on a Windows console) does not end the process: it only
//! sets a flag, which the command-line interface turns into a clean end of
//! the running job (the scan saves its session, a recovery removes its
//! part file). A second Ctrl-C ends the process the usual way. Outside a
//! job no `CtrlC` is alive and Ctrl-C works as always.
//!
//! No dependency: on Unix the C library's `signal` (the handler only
//! stores an atomic flag and resets the disposition, both async-signal
//! safe), on Windows `SetConsoleCtrlHandler` from `windows-sys`, which the
//! crate uses already.

use std::sync::atomic::{AtomicBool, Ordering};

/// Set by the handler.
static PRESSED: AtomicBool = AtomicBool::new(false);

/// The first Ctrl-C is caught while this is alive (see the module
/// documentation).
#[derive(Debug)]
pub struct CtrlC {
    installed: bool,
}

impl CtrlC {
    /// Catches the first Ctrl-C from now on. When the handler cannot be
    /// installed (or Ctrl-C is ignored by the process, as for a program
    /// started in the background), Ctrl-C keeps its usual effect.
    pub fn catch() -> CtrlC {
        PRESSED.store(false, Ordering::SeqCst);
        CtrlC {
            installed: sys::install(),
        }
    }

    /// Whether Ctrl-C was pressed since [`catch`](Self::catch).
    pub fn pressed(&self) -> bool {
        PRESSED.load(Ordering::SeqCst)
    }
}

impl Drop for CtrlC {
    fn drop(&mut self) {
        if self.installed {
            sys::restore();
        }
    }
}

#[cfg(unix)]
mod sys {
    use std::os::raw::c_int;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// `SIGINT` (2 on every Unix this builds for).
    const SIGINT: c_int = 2;
    const SIG_DFL: usize = 0;
    const SIG_IGN: usize = 1;
    const SIG_ERR: usize = usize::MAX;

    extern "C" {
        // `sighandler_t signal(int, sighandler_t)`; a handler is a plain
        // function pointer, passed as an address.
        fn signal(signum: c_int, handler: usize) -> usize;
    }

    /// The disposition before [`install`].
    static PREVIOUS: AtomicUsize = AtomicUsize::new(SIG_DFL);

    extern "C" fn on_sigint(_: c_int) {
        super::PRESSED.store(true, Ordering::SeqCst);
        // A second Ctrl-C ends the process as usual.
        // SAFETY: `signal` is async-signal-safe and gets a valid disposition.
        unsafe {
            signal(SIGINT, SIG_DFL);
        }
    }

    pub(super) fn install() -> bool {
        let handler = on_sigint as extern "C" fn(c_int) as usize;
        // SAFETY: `on_sigint` has the signature of a signal handler and only
        // does async-signal-safe things; `signal` has no other precondition.
        let previous = unsafe { signal(SIGINT, handler) };
        if previous == SIG_ERR {
            return false;
        }
        if previous == SIG_IGN {
            // Started with Ctrl-C ignored (a background job): leave it so.
            // SAFETY: as above, restoring the disposition that was there.
            unsafe {
                signal(SIGINT, SIG_IGN);
            }
            return false;
        }
        PREVIOUS.store(previous, Ordering::SeqCst);
        true
    }

    pub(super) fn restore() {
        // SAFETY: the disposition `signal` returned before `install`.
        unsafe {
            signal(SIGINT, PREVIOUS.load(Ordering::SeqCst));
        }
    }
}

#[cfg(windows)]
mod sys {
    use std::sync::atomic::Ordering;

    use windows_sys::core::BOOL;
    use windows_sys::Win32::System::Console::{
        SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT,
    };

    unsafe extern "system" fn on_ctrl(kind: u32) -> BOOL {
        if (kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT)
            && !super::PRESSED.swap(true, Ordering::SeqCst)
        {
            return 1;
        }
        // The next handler; the system's own ends the process.
        0
    }

    pub(super) fn install() -> bool {
        // SAFETY: `on_ctrl` has the signature of a console control handler;
        // it runs on a thread of its own and only touches an atomic.
        unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 1) != 0 }
    }

    pub(super) fn restore() {
        // SAFETY: removes the handler `install` added.
        unsafe {
            SetConsoleCtrlHandler(Some(on_ctrl), 0);
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod sys {
    pub(super) fn install() -> bool {
        false
    }
    pub(super) fn restore() {}
}
