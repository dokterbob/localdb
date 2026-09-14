//! `Principal`, `Role`, and `StoreAccess`: the pure (I/O-free) identity and
//! grant-evaluation types (D5, D7).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::types::StoreVisibility;
use crate::Error;

/// A user's role. Only two roles exist (D7): `admin` sees and manages
/// everything; `member` is scoped by store grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    Member,
}

/// Which stores a principal may read, independent of role.
///
/// `All` covers admins (who see every store) and the local-trust principal
/// used when auth is not enforced. `Granted` is the set of store names a
/// `member` holds an explicit grant for (D7) — checked only against
/// `shared`-visibility stores; `private` stores are never grantable and
/// never appear here (see `Principal::can_read_store`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreAccess {
    All,
    Granted(HashSet<String>),
}

/// The authenticated identity behind a request, resolved once per request by
/// `AuthService::authenticate` (or synthesized locally when auth is off).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub user_id: String,
    pub name: String,
    pub role: Role,
    pub access: StoreAccess,
}

impl Principal {
    /// The principal used when auth is not enforced: embedded mode, or a
    /// daemon with `server.auth: off` on a loopback bind
    /// (specs/05-surfaces.md §3). Full admin trust; no persisted user backs
    /// it, matching the existing "anything that can reach this process is
    /// trusted" boundary.
    pub fn local_trust() -> Self {
        Principal {
            user_id: "local".to_string(),
            name: "local".to_string(),
            role: Role::Admin,
            access: StoreAccess::All,
        }
    }

    /// `Ok(())` if this principal is an admin, `Err(Forbidden)` otherwise.
    pub fn require_admin(&self) -> Result<(), Error> {
        if self.role == Role::Admin {
            Ok(())
        } else {
            Err(Error::Forbidden {
                message: format!("user '{}' is not an admin", self.name),
            })
        }
    }

    /// D7 grant evaluation: can this principal read a store with the given
    /// `visibility`?
    ///
    /// - Admins read every store, `private` or `shared`.
    /// - Members read `shared` stores they hold a grant for; `private`
    ///   stores are admin-only regardless of any grant — grants against a
    ///   `private` store are rejected at grant time
    ///   (`AuthService::grant_store`), so a well-formed `Granted` set never
    ///   actually contains one, but this check is the backstop.
    pub fn require_read_store(&self, name: &str, visibility: StoreVisibility) -> Result<(), Error> {
        if self.can_read_store(name, visibility) {
            Ok(())
        } else {
            Err(Error::Forbidden {
                message: format!("you do not have access to store '{name}'"),
            })
        }
    }

    pub fn can_read_store(&self, store_name: &str, visibility: StoreVisibility) -> bool {
        match self.role {
            Role::Admin => true,
            Role::Member => match visibility {
                StoreVisibility::Private => false,
                StoreVisibility::Shared => match &self.access {
                    StoreAccess::All => true,
                    StoreAccess::Granted(set) => set.contains(store_name),
                },
            },
        }
    }
}

#[cfg(test)]
mod tests;
