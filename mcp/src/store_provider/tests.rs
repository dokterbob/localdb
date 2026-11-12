use super::*;

#[tokio::test]
async fn static_provider_returns_its_fixed_stores() {
    let provider = StaticStoreProvider::new(vec![]);
    let stores = provider.available_stores().await.unwrap();
    assert!(stores.is_empty());
}
