//! `select_mcp_stores` id-vs-name resolution tests.

use localdb_core::store::FakeStore;

use crate::tools::{select_mcp_stores, AvailableStore};

use super::common::make_descriptor;

// -----------------------------------------------------------------------
// Codex review round 2, finding 3 — select_mcp_stores id-vs-name ambiguity
// -----------------------------------------------------------------------

#[test]
fn select_mcp_stores_id_match_wins_over_shadowing_name() {
    // stores[0] is *named* the same string as stores[1]'s *id*. An
    // order-dependent `name == x || id == x` predicate would return
    // stores[0] (it comes first and matches on the name arm); the fix
    // must do an id pass before falling back to a name pass, so the more
    // specific (unique, machine-generated) id match wins regardless of
    // slice order.
    let shared = "shadow-value".to_string();
    let store_0 = AvailableStore::new(
        make_descriptor("store-0-id", &shared),
        Box::new(FakeStore::new()),
    );
    let store_1 = AvailableStore::new(
        make_descriptor(&shared, "store-1-name"),
        Box::new(FakeStore::new()),
    );
    let stores = vec![store_0, store_1];

    let selected =
        select_mcp_stores(&stores, std::slice::from_ref(&shared)).expect("lookup should resolve");
    assert_eq!(selected.len(), 1);
    assert_eq!(
        selected[0].id, shared,
        "the id match (stores[1]) must win over the shadowing name match (stores[0])"
    );
    assert_eq!(selected[0].name, "store-1-name");
}

#[test]
fn select_mcp_stores_falls_back_to_name_when_no_id_matches() {
    // Ordinary name-lookup path: no store's id equals the lookup string,
    // so the name pass must find it.
    let store_0 = AvailableStore::new(make_descriptor("id-0", "alpha"), Box::new(FakeStore::new()));
    let store_1 = AvailableStore::new(make_descriptor("id-1", "beta"), Box::new(FakeStore::new()));
    let stores = vec![store_0, store_1];

    let selected = select_mcp_stores(&stores, std::slice::from_ref(&"beta".to_string()))
        .expect("lookup should resolve by name");
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].id, "id-1");
    assert_eq!(selected[0].name, "beta");
}
