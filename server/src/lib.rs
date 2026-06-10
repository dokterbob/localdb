//! HTTP API daemon for localdb.
//!
//! Provides the axum-based REST API (`/v1`), daemon lifecycle management
//! (unix socket for discovery, write lock), file watching, URL refresh
//! scheduling, and the background job queue.
//!
//! ## Entry point
//!
//! Call [`daemon::start_daemon`] with resolved paths and the loaded YAML
//! config to start the server. It validates the bind address, acquires the
//! write lock, and returns a [`daemon::DaemonHandle`] + a server future to
//! await.
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
pub mod lock;
pub mod scheduler;
pub mod socket;
pub mod state;
pub mod watcher;

pub use daemon::{build_router, start_daemon, validate_bind_address, DaemonHandle, DaemonOptions};
pub use error::{ApiError, ErrorResponse};
pub use job_queue::JobQueue;
pub use lock::WriteLock;
pub use scheduler::UrlRefreshScheduler;
pub use state::AppState;
