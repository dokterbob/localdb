//! Authentication and authorization domain module (issue #98).
//!
//! ALL auth policy lives here per the layering invariant in
//! specs/01-architecture.md §1 — no domain logic in surface crates.
//! Persistence is behind the `AuthStore` trait; the concrete implementation
//! lives in `store-libsql`, mirroring how `RetrievalStore` (`core::store`) is
//! implemented there.
//!
//! See specs/02-domain-model.md (entities) and specs/05-surfaces.md §3.1
//! (route-level behavior — lands incrementally across T1-T7; this ticket is
//! foundation only: no daemon/CLI/MCP wiring).

mod principal;
mod service;
mod store;
mod token;

pub use principal::{Principal, Role, StoreAccess};
pub use service::{AuthService, IssuedInvite, IssuedToken};
#[cfg(any(test, feature = "test-support"))]
pub use store::FakeAuthStore;
pub use store::{
    AccessRequestRow, AccessRequestState, AuthStore, AuthTokenRow, InviteMode, InviteRow,
    StoreGrantRow, TokenKind, UserRow,
};
pub use token::{
    hash_secret, is_expired, mint_secret, rfc3339_from_now, verify_pkce_s256, verify_secret,
    MintedSecret, ACCESS_TOKEN_TTL_SECS, REFRESH_TOKEN_TTL_SECS, TOKEN_PREFIX,
};
