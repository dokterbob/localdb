use super::super::*;
use axum::http::header;
use localdb_core::Error;
#[test]
fn unauthorized_response_carries_www_authenticate_bearer() {
    let response = ApiError(Error::Unauthorized {
        message: "m".into(),
    })
    .into_response();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer"),
        "D6: every 401 must carry WWW-Authenticate: Bearer"
    );
}

#[test]
fn forbidden_response_has_no_www_authenticate() {
    let response = ApiError(Error::Forbidden {
        message: "m".into(),
    })
    .into_response();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(response.headers().get(header::WWW_AUTHENTICATE).is_none());
}

#[test]
fn unauthorized_maps_to_401() {
    assert_eq!(
        http_status_for(&Error::Unauthorized {
            message: "m".into()
        }),
        StatusCode::UNAUTHORIZED
    );
}

#[test]
fn forbidden_maps_to_403() {
    assert_eq!(
        http_status_for(&Error::Forbidden {
            message: "m".into()
        }),
        StatusCode::FORBIDDEN
    );
}
