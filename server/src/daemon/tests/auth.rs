use crate::auth::AuthMode;
use crate::daemon::resolve_auth_mode;
use localdb_core::{config::schema::ServerAuthMode, Error};
use std::net::SocketAddr;
#[test]
fn resolve_auth_mode_auto_loopback_is_open() {
    assert_eq!(
        resolve_auth_mode(loopback(), ServerAuthMode::Auto).unwrap(),
        AuthMode::Open
    );
}

#[test]
fn resolve_auth_mode_auto_non_loopback_is_enforced() {
    assert_eq!(
        resolve_auth_mode(non_loopback(), ServerAuthMode::Auto).unwrap(),
        AuthMode::Enforced
    );
}

#[test]
fn resolve_auth_mode_auto_ipv6_loopback_is_open() {
    assert_eq!(
        resolve_auth_mode("[::1]:7700".parse().unwrap(), ServerAuthMode::Auto).unwrap(),
        AuthMode::Open
    );
}

#[test]
fn resolve_auth_mode_auto_wildcard_is_enforced() {
    // An unspecified bind is reachable from any network — non-loopback
    // for enforcement purposes.
    assert_eq!(
        resolve_auth_mode("0.0.0.0:7700".parse().unwrap(), ServerAuthMode::Auto).unwrap(),
        AuthMode::Enforced
    );
}

#[test]
fn resolve_auth_mode_required_loopback_is_enforced() {
    assert_eq!(
        resolve_auth_mode(loopback(), ServerAuthMode::Required).unwrap(),
        AuthMode::Enforced
    );
}

#[test]
fn resolve_auth_mode_required_non_loopback_is_enforced() {
    assert_eq!(
        resolve_auth_mode(non_loopback(), ServerAuthMode::Required).unwrap(),
        AuthMode::Enforced
    );
}

#[test]
fn resolve_auth_mode_off_loopback_is_open() {
    assert_eq!(
        resolve_auth_mode(loopback(), ServerAuthMode::Off).unwrap(),
        AuthMode::Open
    );
}

#[test]
fn resolve_auth_mode_off_non_loopback_is_invalid_config() {
    let err = resolve_auth_mode(non_loopback(), ServerAuthMode::Off).unwrap_err();
    assert!(
        matches!(err, Error::InvalidConfig { ref message } if message.contains("192.0.2.1")),
        "expected InvalidConfig naming the bind address, got: {err:?}"
    );
}

#[test]
fn resolve_auth_mode_off_wildcard_is_invalid_config() {
    let err = resolve_auth_mode("0.0.0.0:7700".parse().unwrap(), ServerAuthMode::Off).unwrap_err();
    assert!(matches!(err, Error::InvalidConfig { .. }));
}

fn loopback() -> SocketAddr {
    "127.0.0.1:7700".parse().unwrap()
}
fn non_loopback() -> SocketAddr {
    "192.0.2.1:7700".parse().unwrap()
}
