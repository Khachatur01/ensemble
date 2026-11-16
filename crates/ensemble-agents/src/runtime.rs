//! The async runtime agents run on, and what code that is not async yet uses
//! to wait for them.

use std::future::Future;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How often a cancel flag is looked at.
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// Resolves once `flag` is set.
pub async fn until_set(flag: &AtomicBool) {
    while !flag.load(Ordering::Relaxed) {
        tokio::time::sleep(CANCEL_POLL).await;
    }
}

/// Run `future` to its end from code that is not async: on the runtime this
/// thread is in, else on one shared by the process.
pub fn block_on<F: Future>(future: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(future)),
        Err(_) => shared().block_on(future),
    }
}

/// The runtime of the process, made on first use.
pub fn shared() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("the async runtime starts")
    })
}

/// Run `f`, which blocks, from async code without holding up the runtime's
/// other tasks: they move to other threads meanwhile. What the calling task
/// runs alongside `f` waits for it, so `f` is something short, such as a
/// file, a git command or one HTTP request.
pub fn blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}
