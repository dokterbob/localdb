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
