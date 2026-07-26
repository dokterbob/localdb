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

pub mod auth;
pub mod daemon;
pub mod error;
pub mod handlers;
pub mod job_queue;
pub mod mcp_bridge;
pub mod scheduler;
pub mod search_service;
pub mod socket;
pub mod state;
pub mod watcher;

pub use auth::AuthMode;
pub use daemon::{build_router, resolve_auth_mode, start_daemon, DaemonHandle, DaemonOptions};
pub use error::{ApiError, ErrorResponse};
pub use job_queue::JobQueue;
pub use scheduler::UrlRefreshScheduler;
pub use state::AppState;
