//! HTTP API daemon for localdb.
//!
//! ## Entry point
//!
//! ## API surface
//!
//! All routes are mounted at `/v1`. See [`handlers`] and
//! specs/05-surfaces.md §3.
//!
//! Implemented in T11.

pub mod daemon;
pub mod error;
pub mod handlers;
pub mod job_queue;
pub mod scheduler;
pub mod socket;
pub mod state;
pub mod watcher;

pub use daemon::{build_router, start_daemon, validate_bind_address, DaemonHandle, DaemonOptions};
pub use error::{ApiError, ErrorResponse};
pub use job_queue::JobQueue;
pub use scheduler::UrlRefreshScheduler;
pub use state::AppState;
