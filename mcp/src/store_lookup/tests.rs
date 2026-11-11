use super::*;

struct Item {
    id: &'static str,
    name: &'static str,
}

fn items() -> Vec<Item> {
    vec![
        Item {
            id: "id-1",
            name: "alpha",
        },
        Item {
            id: "id-2",
            name: "beta",
        },
    ]
}

#[test]
fn resolves_by_id() {
    let items = items();
    let found = resolve_by_id_then_name(&items, "id-2", |i| i.id, |i| i.name);
    assert_eq!(found.unwrap().name, "beta");
}

#[test]
fn resolves_by_name_when_no_id_matches() {
    let items = items();
    let found = resolve_by_id_then_name(&items, "beta", |i| i.id, |i| i.name);
    assert_eq!(found.unwrap().id, "id-2");
}

#[test]
fn id_match_wins_over_a_colliding_name() {
    // A value that is simultaneously one item's id and another item's name
    // must resolve to the id match.
    let items = vec![
        Item {
            id: "id-1",
            name: "shadow",
        },
        Item {
            id: "shadow",
            name: "id-2-name",
        },
    ];
    let found = resolve_by_id_then_name(&items, "shadow", |i| i.id, |i| i.name);
    assert_eq!(
        found.unwrap().name,
        "id-2-name",
        "an id match must win over a different item's colliding name"
    );
}

#[test]
fn returns_none_when_nothing_matches() {
    let items = items();
    assert!(resolve_by_id_then_name(&items, "nope", |i| i.id, |i| i.name).is_none());
}
