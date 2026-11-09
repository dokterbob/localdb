use super::*;

#[tokio::test]
async fn static_bearer_current_returns_the_configured_secret() {
    let source = StaticBearer("ldb_static".to_string());
    assert_eq!(source.current().await, Some("ldb_static".to_string()));
}

#[tokio::test]
async fn static_bearer_never_refreshes() {
    let source = StaticBearer("ldb_static".to_string());
    assert_eq!(source.refresh("ldb_static").await, None);
}

/// A `BearerSource` whose `refresh` answer is configured up front, so
/// `RefreshingHttpClient::retry_token` can be exercised without a network
/// round trip.
struct ScriptedSource {
    on_refresh: Option<String>,
}

#[async_trait]
impl BearerSource for ScriptedSource {
    async fn current(&self) -> Option<String> {
        None
    }

    async fn refresh(&self, _rejected: &str) -> Option<String> {
        self.on_refresh.clone()
    }
}

fn client_with(on_refresh: Option<&str>) -> RefreshingHttpClient {
    RefreshingHttpClient::new(Arc::new(ScriptedSource {
        on_refresh: on_refresh.map(str::to_string),
    }))
}

#[tokio::test]
async fn retry_token_gives_up_when_no_token_was_sent() {
    // There is nothing to hand the source as `rejected`, so there is
    // nothing worth retrying — e.g. the initial handshake never had a
    // bearer to begin with.
    let client = client_with(Some("ldb_new"));
    assert_eq!(client.retry_token(None).await, None);
}

#[tokio::test]
async fn retry_token_gives_up_when_the_source_cannot_refresh() {
    let client = client_with(None);
    assert_eq!(client.retry_token(Some("ldb_old".to_string())).await, None);
}

#[tokio::test]
async fn retry_token_gives_up_when_the_refresh_echoes_the_rejected_token() {
    // A source that hands back the exact token that was just rejected has
    // made no progress — retrying with it would just fail again.
    let client = client_with(Some("ldb_old"));
    assert_eq!(client.retry_token(Some("ldb_old".to_string())).await, None);
}

#[tokio::test]
async fn retry_token_returns_the_refreshed_token() {
    let client = client_with(Some("ldb_new"));
    assert_eq!(
        client.retry_token(Some("ldb_old".to_string())).await,
        Some("ldb_new".to_string())
    );
}
