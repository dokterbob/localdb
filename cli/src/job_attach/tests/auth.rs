use super::*;

#[tokio::test]
async fn sse_and_polling_use_the_same_credentials() {
    for sse_available in [true, false] {
        let dir = TempDir::new().unwrap();
        let mut ctx = test_ctx();
        ctx.config = Some(dir.path().join("config.yaml"));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        crate::credentials::write_entry(
            &crate::credentials::credentials_path(ctx.config.as_ref().unwrap()),
            &base,
            crate::credentials::CredentialEntry {
                access_token: Some("cached-token".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let app = Router::new()
            .route(
                "/v1/jobs/{id}/events",
                get(move |headers: axum::http::HeaderMap| async move {
                    assert_eq!(headers["authorization"], "Bearer cached-token");
                    if sse_available {
                        (
                            StatusCode::OK,
                            format!(
                                "event: job\ndata: {}\n\n",
                                serde_json::to_string(&sample_job("job-auth", IndexJobState::Done))
                                    .unwrap()
                            ),
                        )
                    } else {
                        (StatusCode::NOT_FOUND, String::new())
                    }
                }),
            )
            .route(
                "/v1/jobs/{id}",
                get(|headers: axum::http::HeaderMap| async move {
                    assert_eq!(headers["authorization"], "Bearer cached-token");
                    axum::Json(sample_job("job-auth", IndexJobState::Done))
                }),
            );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let job = attach_daemon_job(&ctx, &base, "job-auth", false, None)
            .await
            .unwrap();
        assert_eq!(job.state, IndexJobState::Done);
        task.abort();
    }
}

/// A daemon served behind a path prefix: the stored credential's key may be
/// a shorter path-segment prefix of the base URL `localdb index` was given
/// (the same rule `daemon_client::fresh_bearer_for_url` applies to every
/// other request), not the exact string. The SSE attach path must find it
/// too, not just the ones routed through `daemon_request_async`.
#[tokio::test]
async fn sse_path_picks_up_a_prefix_matched_credentials_key() {
    let dir = TempDir::new().unwrap();
    let mut ctx = test_ctx();
    ctx.config = Some(dir.path().join("config.yaml"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // Stored under the bare origin; the daemon is reached at a path-prefixed
    // base URL built from that same origin.
    let stored_key = format!("http://{addr}");
    let base = format!("http://{addr}/team");
    crate::credentials::write_entry(
        &crate::credentials::credentials_path(ctx.config.as_ref().unwrap()),
        &stored_key,
        crate::credentials::CredentialEntry {
            access_token: Some("prefix-matched-token".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let app = Router::new().route(
        "/team/v1/jobs/{id}/events",
        get(move |headers: axum::http::HeaderMap| async move {
            assert_eq!(headers["authorization"], "Bearer prefix-matched-token");
            (
                StatusCode::OK,
                format!(
                    "event: job\ndata: {}\n\n",
                    serde_json::to_string(&sample_job("job-prefix", IndexJobState::Done)).unwrap()
                ),
            )
        }),
    );
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let job = attach_daemon_job(&ctx, &base, "job-prefix", false, None)
        .await
        .unwrap();
    assert_eq!(job.state, IndexJobState::Done);
    task.abort();
}
