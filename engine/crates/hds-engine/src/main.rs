//! `hdsavior-engine`: the HDSavior engine's command-line tool and JSON-lines
//! server. See the library crate `hds_engine`.

use std::process::ExitCode;
use std::sync::Arc;

use hds_engine::backend::{build_backend, Backend};

fn main() -> ExitCode {
    hds_engine::platform::suppress_error_dialogs();
    let args = std::env::args_os().skip(1).collect();
    ExitCode::from(hds_engine::cli::run(args, backend()))
}

/// The backend this build serves. Debug builds (tests) can be switched to
/// the test double with `HDS_ENGINE_TEST_BACKEND=fake` (jobs end by
/// themselves) or `fake-wait` (jobs run until cancelled); release builds
/// always serve [`build_backend`].
fn backend() -> Arc<dyn Backend> {
    #[cfg(debug_assertions)]
    {
        use hds_engine::testing::{FakeBackend, FakeConfig, FakeJobs};
        let jobs = match std::env::var("HDS_ENGINE_TEST_BACKEND").as_deref() {
            Ok("fake") => Some(FakeJobs::Finish),
            Ok("fake-wait") => Some(FakeJobs::UntilCancelled),
            _ => None,
        };
        if let Some(jobs) = jobs {
            return Arc::new(FakeBackend::new(FakeConfig {
                jobs,
                ..FakeConfig::default()
            }));
        }
    }
    build_backend()
}
