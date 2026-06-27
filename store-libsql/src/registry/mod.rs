#[cfg(test)]
use crate::connection::LibsqlDb;

pub(crate) mod documents;
pub(crate) mod sources;
pub(crate) mod sql;
pub(crate) mod stores;

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) async fn delete_sources_for_store(
    db: &LibsqlDb,
    store_id: &str,
) -> Result<u64, localdb_core::Error> {
    sources::delete_sources_for_store(db, store_id).await
}
