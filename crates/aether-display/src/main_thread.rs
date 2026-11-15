//! Run AppKit work on the main thread from any thread.

use dispatch2::DispatchQueue;
use objc2::MainThreadMarker;

/// Run `f` on the main thread and return its result. Executes inline when already on the
/// main thread (dispatching synchronously to the main queue from it would deadlock).
///
/// Requires the main thread to service the main dispatch queue (an AppKit run loop, as in the
/// desktop app). CLI callers should call from the main thread directly.
pub fn run_sync<R: Send + 'static>(f: impl FnOnce(MainThreadMarker) -> R + Send + 'static) -> R {
    if let Some(mtm) = MainThreadMarker::new() {
        return f(mtm);
    }
    let slot = std::sync::Arc::new(parking_lot::Mutex::new(None));
    let out = slot.clone();
    DispatchQueue::main().exec_sync(move || {
        // SAFETY: blocks submitted to the main queue run on the main thread.
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        *out.lock() = Some(f(mtm));
    });
    let r = slot.lock().take();
    r.expect("main-thread closure completed")
}
