use super::*;

#[test]
fn local_trust_is_admin_with_full_access() {
    let p = Principal::local_trust();
    assert_eq!(p.role, Role::Admin);
    assert_eq!(p.access, StoreAccess::All);
}

#[test]
fn require_admin_ok_for_admin() {
    let p = Principal::local_trust();
    assert!(p.require_admin().is_ok());
}

#[test]
fn require_admin_forbidden_for_member() {
    let p = Principal {
        user_id: "u1".into(),
        name: "member".into(),
        role: Role::Member,
        access: StoreAccess::Granted(Default::default()),
    };
    assert!(matches!(p.require_admin(), Err(Error::Forbidden { .. })));
}

#[test]
fn admin_reads_private_and_shared() {
    let p = Principal::local_trust();
    assert!(p.can_read_store("s", StoreVisibility::Private));
    assert!(p.can_read_store("s", StoreVisibility::Shared));
}

#[test]
fn member_without_grant_cannot_read_shared() {
    let p = Principal {
        user_id: "u".into(),
        name: "m".into(),
        role: Role::Member,
        access: StoreAccess::Granted(Default::default()),
    };
    assert!(!p.can_read_store("docs", StoreVisibility::Shared));
}

#[test]
fn member_with_grant_reads_shared() {
    let mut set = HashSet::new();
    set.insert("docs".to_string());
    let p = Principal {
        user_id: "u".into(),
        name: "m".into(),
        role: Role::Member,
        access: StoreAccess::Granted(set),
    };
    assert!(p.can_read_store("docs", StoreVisibility::Shared));
}

#[test]
fn member_grant_on_other_store_does_not_leak_access() {
    let mut set = HashSet::new();
    set.insert("docs".to_string());
    let p = Principal {
        user_id: "u".into(),
        name: "m".into(),
        role: Role::Member,
        access: StoreAccess::Granted(set),
    };
    assert!(!p.can_read_store("other", StoreVisibility::Shared));
}

#[test]
fn member_never_reads_private_even_with_grant() {
    let mut set = HashSet::new();
    set.insert("secret".to_string());
    let p = Principal {
        user_id: "u".into(),
        name: "m".into(),
        role: Role::Member,
        access: StoreAccess::Granted(set),
    };
    assert!(!p.can_read_store("secret", StoreVisibility::Private));
}
