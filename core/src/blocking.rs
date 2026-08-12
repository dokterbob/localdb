//! Helper for running CPU-bound synchronous work from async code without
//! stalling a shared multi-thread tokio runtime's worker pool.
//!
//! `Parser::parse` ([`crate::parser::Parser`]) is documented as sync and
//! CPU-bound; callers on a runtime that also serves other concurrent work
//! (e.g. the HTTP daemon's job engine, which runs ingestion on the same
//! runtime as `/v1` routes and SSE streams) must not call it inline, or a
//! large document's parse can starve HTTP handling for the duration.
//!
//! This mirrors the pattern already used at every local-inference call site
//! in the `embed` crate (`embed/src/pplx_onnx.rs`, `pplx_context_onnx.rs`,
//! `pplx_context_coreml.rs`): `tokio::task::block_in_place` moves the
//! blocking work off the async worker thread's cooperative scheduling, so
//! sibling tasks on other worker threads keep making progress. But
//! `block_in_place` PANICS when called from a *current-thread* runtime (the
//! kind `#[tokio::test]` defaults to, and the kind a single-threaded
//! embedded CLI invocation may use) — a current-thread runtime has no other
//! worker thread to hand off to. `run_blocking` checks the runtime flavor
//! first: multi-thread runtime -> `block_in_place`; anything else (including
//! no runtime at all) -> call `f` inline. `ingest` depends on `core` but not
//! on `embed`, so this lives here rather than being reused directly from
//! `embed`; `embed`'s own call sites are free to adopt this shared version
//! later instead of their private per-file copies.

/// Run a CPU-bound synchronous closure without blocking a shared
/// multi-thread tokio runtime's worker pool.
///
/// See the module doc comment for why the current-thread branch calls `f`
/// inline rather than `block_in_place`ing (which would panic there).
pub fn run_blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No runtime at all: must call `f` inline, not panic.
    #[test]
    fn run_blocking_outside_any_runtime_calls_f_inline() {
        assert_eq!(run_blocking(|| 42), 42);
    }

    /// Current-thread runtime: `block_in_place` would panic here; the
    /// flavor check must route around it and just call `f` inline.
    #[tokio::test]
    async fn run_blocking_on_current_thread_runtime_does_not_panic() {
        assert_eq!(run_blocking(|| 7), 7);
    }

    /// Multi-thread runtime: takes the `block_in_place` branch.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_blocking_on_multi_thread_runtime_uses_block_in_place() {
        assert_eq!(run_blocking(|| 99), 99);
    }
}
