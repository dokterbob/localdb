//! `localdb document list` / `localdb document get` — read-only surfaces
//! over the shared document read model (`localdb_core::documents`).
//!
//! Mirrors `cmds::source`'s daemon/embedded split via `DaemonAwareCommand`,
//! but `document get` does not fit `StoreScopePolicy`'s omitted-`--store`
//! shapes (every existing policy means "every store" or "one named store");
//! it resolves its own 0/1/many `-s` semantics directly against
//! `get_document_detail_scoped`'s contract instead.

use localdb_core::{
    config::loader::ConfigLoader, get_document_detail_scoped, resolve_named_stores, DocumentInfo,
    Error, Metadata,
};
use serde_json::json;

use crate::{
    app_db::{
        load_config_scaffolded, open_app_db_or_exit, resolve_daemon_store_scope_inner,
        resolve_store_scope_inner, AppDb, StoreScopePolicy,
    },
    cmds::listing::{render_scoped_list, ScopedListItem},
    command_table::{dispatch, DaemonAwareCommand},
    daemon_client::{daemon_request_async, encode_path_segment, walk_daemon_pages, CliContext},
    normalize::{print_json, validate_store_name},
};

// ---------------------------------------------------------------------------
// document list
// ---------------------------------------------------------------------------

/// One document, as `document list` reports it — identical fields whether
/// sourced from an embedded `DocumentInfo` or a daemon's `GET
/// /v1/stores/{name}/documents` (which serializes `DocumentInfo` directly,
/// see `server/src/handlers/documents.rs::list_documents`).
struct DocumentListItem {
    id: String,
    uri: String,
    title: Option<String>,
    store_id: String,
    store_name: String,
    source_id: String,
    content_hash: String,
    fetched_at: String,
    date_original: Option<String>,
    date_parsed: Option<String>,
    index_updated_at: Option<String>,
}

fn document_info_to_list_item(d: &DocumentInfo, store_name: &str) -> DocumentListItem {
    DocumentListItem {
        id: d.id.clone(),
        uri: d.uri.clone(),
        title: d.title.clone(),
        store_id: d.store_id.clone(),
        store_name: store_name.to_string(),
        source_id: d.source_id.clone(),
        content_hash: d.content_hash.clone(),
        fetched_at: d.fetched_at.clone(),
        date_original: d.date_original.clone(),
        date_parsed: d.date_parsed.clone(),
        index_updated_at: d.index_updated_at.clone(),
    }
}

/// Convert one raw `GET /v1/stores/{name}/documents` item into a
/// `DocumentListItem`. The daemon serializes `DocumentInfo` verbatim (see
/// `server/src/handlers/documents.rs::list_documents`), so fields are read
/// directly rather than through a bespoke wire shape — still defensively,
/// the same posture `cmds::source::daemon_item_to_list_item` takes toward a
/// daemon response.
fn daemon_item_to_document_list_item(
    item: &serde_json::Value,
    store_name: &str,
) -> DocumentListItem {
    DocumentListItem {
        id: item
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string(),
        uri: item
            .get("uri")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string(),
        title: item
            .get("title")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        store_id: item
            .get("store_id")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string(),
        store_name: store_name.to_string(),
        source_id: item
            .get("source_id")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string(),
        content_hash: item
            .get("content_hash")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string(),
        fetched_at: item
            .get("fetched_at")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string(),
        date_original: item
            .get("date_original")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        date_parsed: item
            .get("date_parsed")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        index_updated_at: item
            .get("index_updated_at")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    }
}

struct DocumentListCmd {
    source: Option<String>,
}

impl DaemonAwareCommand for DocumentListCmd {
    // Same rationale as `SourceListCmd::Outcome` (`cmds/source.rs`): the
    // resolved scope's store *names* travel alongside the items, so the
    // store-name column / "no documents on store X" message key off the
    // resolved scope rather than off whichever stores happened to return an
    // item.
    type Outcome = (Vec<String>, Vec<DocumentListItem>);

    // specs/05-surfaces.md §2.2 idiom: `-s` is a *filter* — a bare
    // `document list` spans every store.
    const SCOPE_POLICY: StoreScopePolicy = StoreScopePolicy::AllStores;

    async fn run_daemon(&self, ctx: &CliContext, base_url: &str) -> Result<Self::Outcome, Error> {
        let store_names =
            resolve_daemon_store_scope_inner(base_url, ctx, Self::SCOPE_POLICY).await?;

        let mut all = Vec::new();
        for store_name in &store_names {
            let mut path = format!("/v1/stores/{}/documents", encode_path_segment(store_name));
            if let Some(source) = &self.source {
                path.push_str("?source=");
                path.push_str(&encode_path_segment(source));
            }
            walk_daemon_pages(base_url, &path, |items| {
                for item in items {
                    all.push(daemon_item_to_document_list_item(item, store_name));
                }
                false
            })
            .await?;
        }
        Ok((store_names, all))
    }

    async fn run_embedded(
        &self,
        ctx: &CliContext,
        _config_loader: &ConfigLoader,
        db: &AppDb,
    ) -> Result<Self::Outcome, Error> {
        let rows = resolve_store_scope_inner(ctx, db, Self::SCOPE_POLICY).await?;
        let mut all = Vec::new();
        for row in &rows {
            let docs = db
                .backend()
                .list_documents(&row.id, self.source.as_deref(), None, 0)
                .await?;
            for d in &docs {
                all.push(document_info_to_list_item(d, &row.name));
            }
        }
        let store_names = rows.into_iter().map(|r| r.name).collect();
        Ok((store_names, all))
    }
}

impl ScopedListItem for DocumentListItem {
    const JSON_KEY: &'static str = "documents";
    const EMPTY_NOUN: &'static str = "documents";

    fn json_row(&self) -> serde_json::Value {
        json!({
            "id": self.id,
            "uri": self.uri,
            "title": self.title,
            "store": { "name": self.store_name },
            "store_id": self.store_id,
            "source_id": self.source_id,
            "content_hash": self.content_hash,
            "fetched_at": self.fetched_at,
            "date_original": self.date_original,
            "date_parsed": self.date_parsed,
            "index_updated_at": self.index_updated_at,
        })
    }

    fn human_line(&self, with_store_column: bool, col_width: usize) -> String {
        let body = match self.title.as_deref() {
            Some(t) if !t.is_empty() => format!("{} {} ({})", self.id, self.uri, t),
            _ => format!("{} {}", self.id, self.uri),
        };
        if with_store_column {
            format!("{:<width$}{}", self.store_name, body, width = col_width)
        } else {
            body
        }
    }
}

/// `localdb document list`
pub fn run_document_list(ctx: &CliContext, source: Option<&str>) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_document_list_async(ctx, source));
}

pub(crate) async fn run_document_list_async(ctx: &CliContext, source: Option<&str>) {
    let config_loader = load_config_scaffolded(ctx).await;
    let cmd = DocumentListCmd {
        source: source.map(str::to_string),
    };
    let (scope_store_names, items) = dispatch(&cmd, ctx, &config_loader, || {
        open_app_db_or_exit(ctx, &config_loader)
    })
    .await;
    render_scoped_list(&items, &scope_store_names, ctx.json);
}

// ---------------------------------------------------------------------------
// document get
// ---------------------------------------------------------------------------

/// A single document's full detail, as `document get` reports it — the
/// mode-agnostic shape both transports converge on. `text` is always
/// populated (both transports fetch it unconditionally: the daemon's `GET
/// /v1/documents/{id}` always includes it, and the embedded branch always
/// passes `include_text: true` to `get_document_detail_scoped`); whether it
/// is actually *shown* is a rendering decision (`--text` for human output,
/// always for `--json`), not a fetch decision — see `render_document_get`.
#[derive(Debug)]
struct DocumentGetResult {
    id: String,
    uri: String,
    title: Option<String>,
    store_id: String,
    source_id: String,
    content_hash: String,
    fetched_at: String,
    metadata: Metadata,
    text: String,
    date_original: Option<String>,
    date_parsed: Option<String>,
    index_updated_at: Option<String>,
}

impl DocumentGetResult {
    fn from_detail(detail: localdb_core::DocumentDetail) -> Self {
        let info = detail.info;
        Self {
            id: info.id,
            uri: info.uri,
            title: info.title,
            store_id: info.store_id,
            source_id: info.source_id,
            content_hash: info.content_hash,
            fetched_at: info.fetched_at,
            metadata: info.metadata,
            text: detail.text.unwrap_or_default(),
            date_original: info.date_original,
            date_parsed: info.date_parsed,
            index_updated_at: info.index_updated_at,
        }
    }
}

/// The daemon's `GET /v1/documents/{id}` response shape (`DocumentRecord` in
/// `server/src/handlers/documents.rs`) — not reusable directly, since that
/// type isn't part of `server`'s public surface (only its handler functions
/// are re-exported). Parsed defensively via `serde_json::Value`, matching
/// this module's `document list` daemon parsing.
fn document_get_result_from_daemon_json(v: &serde_json::Value) -> Result<DocumentGetResult, Error> {
    let get_str = |key: &str| -> Result<String, Error> {
        v.get(key)
            .and_then(|f| f.as_str())
            .map(str::to_string)
            .ok_or_else(|| Error::Internal {
                message: format!("daemon document response missing '{key}' field"),
                correlation_id: "document_get_daemon_shape".to_string(),
            })
    };
    let metadata: Metadata = v
        .get("metadata")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| Error::Internal {
            message: format!("daemon document response has a malformed 'metadata' field: {e}"),
            correlation_id: "document_get_daemon_shape".to_string(),
        })?
        .ok_or_else(|| Error::Internal {
            message: "daemon document response missing 'metadata' field".to_string(),
            correlation_id: "document_get_daemon_shape".to_string(),
        })?;
    Ok(DocumentGetResult {
        id: get_str("id")?,
        uri: get_str("uri")?,
        title: v.get("title").and_then(|f| f.as_str()).map(str::to_string),
        store_id: get_str("store_id")?,
        source_id: get_str("source_id")?,
        content_hash: get_str("content_hash")?,
        fetched_at: get_str("fetched_at")?,
        metadata,
        text: v
            .get("normalized_text")
            .and_then(|f| f.as_str())
            .unwrap_or_default()
            .to_string(),
        date_original: v
            .get("date_original")
            .and_then(|f| f.as_str())
            .map(str::to_string),
        date_parsed: v
            .get("date_parsed")
            .and_then(|f| f.as_str())
            .map(str::to_string),
        index_updated_at: v
            .get("index_updated_at")
            .and_then(|f| f.as_str())
            .map(str::to_string),
    })
}

struct DocumentGetCmd<'a> {
    id: &'a str,
}

impl DaemonAwareCommand for DocumentGetCmd<'_> {
    type Outcome = DocumentGetResult;

    // `document get` resolves its own 0/1/many `-s` scope directly (see the
    // module doc comment) rather than through `resolve_store_scope`/
    // `resolve_daemon_store_scope` — this constant governs none of its
    // behavior, so any variant is equally inert here.
    const SCOPE_POLICY: StoreScopePolicy = StoreScopePolicy::AllStoresAllowEmpty;

    async fn run_daemon(&self, ctx: &CliContext, base_url: &str) -> Result<Self::Outcome, Error> {
        for name in &ctx.stores {
            validate_store_name(name)?;
        }
        let mut url = format!("{base_url}/v1/documents/{}", encode_path_segment(self.id));
        if !ctx.stores.is_empty() {
            let query: Vec<String> = ctx
                .stores
                .iter()
                .map(|name| format!("store={}", encode_path_segment(name)))
                .collect();
            url.push('?');
            url.push_str(&query.join("&"));
        }
        let v = daemon_request_async(reqwest::Method::GET, &url, None).await?;
        document_get_result_from_daemon_json(&v)
    }

    async fn run_embedded(
        &self,
        ctx: &CliContext,
        _config_loader: &ConfigLoader,
        db: &AppDb,
    ) -> Result<Self::Outcome, Error> {
        for name in &ctx.stores {
            validate_store_name(name)?;
        }
        let store_rows = resolve_named_stores(db.backend(), &ctx.stores).await?;
        let store_ids: Vec<String> = store_rows.into_iter().map(|r| r.id).collect();
        let detail = get_document_detail_scoped(db.backend(), self.id, &store_ids, true).await?;
        Ok(DocumentGetResult::from_detail(detail))
    }
}

/// Human-readable `document get` output lines: key-value metadata lines,
/// then (only when `include_text`) a blank line followed by the
/// reconstructed document text. Factored out from `render_document_get` so
/// the exact line set is unit-testable without capturing stdout.
fn document_get_human_lines(doc: &DocumentGetResult, include_text: bool) -> Vec<String> {
    let mut lines = vec![format!("id: {}", doc.id), format!("uri: {}", doc.uri)];
    if let Some(title) = &doc.title {
        lines.push(format!("title: {title}"));
    }
    lines.push(format!("store_id: {}", doc.store_id));
    lines.push(format!("source_id: {}", doc.source_id));
    lines.push(format!("content_hash: {}", doc.content_hash));
    lines.push(format!("fetched_at: {}", doc.fetched_at));
    if let Some(date) = &doc.date_original {
        lines.push(format!("date: {date}"));
    }

    // Dublin Core fields beyond `title` (already shown above from the
    // document registry's own `title` column) — printed only when present,
    // per specs/02-domain-model.md §7's optional-everywhere shape.
    let dc = doc.metadata.dublin_core();
    if !dc.creator.is_empty() {
        lines.push(format!("dc.creator: {}", dc.creator.join(", ")));
    }
    if !dc.subject.is_empty() {
        lines.push(format!("dc.subject: {}", dc.subject.join(", ")));
    }
    if let Some(v) = &dc.description {
        lines.push(format!("dc.description: {v}"));
    }
    if let Some(v) = &dc.publisher {
        lines.push(format!("dc.publisher: {v}"));
    }
    if !dc.contributor.is_empty() {
        lines.push(format!("dc.contributor: {}", dc.contributor.join(", ")));
    }
    if let Some(v) = &dc.date {
        lines.push(format!("dc.date: {v}"));
    }
    if let Some(v) = &dc.r#type {
        lines.push(format!("dc.type: {v}"));
    }
    if let Some(v) = &dc.format {
        lines.push(format!("dc.format: {v}"));
    }
    if let Some(v) = &dc.identifier {
        lines.push(format!("dc.identifier: {v}"));
    }
    if let Some(v) = &dc.source {
        lines.push(format!("dc.source: {v}"));
    }
    if let Some(v) = &dc.language {
        lines.push(format!("dc.language: {v}"));
    }
    if !dc.relation.is_empty() {
        lines.push(format!("dc.relation: {}", dc.relation.join(", ")));
    }
    if let Some(v) = &dc.coverage {
        lines.push(format!("dc.coverage: {v}"));
    }
    if let Some(v) = &dc.rights {
        lines.push(format!("dc.rights: {v}"));
    }

    if include_text {
        lines.push(String::new());
        lines.push(doc.text.clone());
    }
    lines
}

/// Build the `document get --json` object. Always includes `text` (see
/// `DocumentGetResult`'s doc comment) — `--text` governs only the
/// human-readable renderer, `document_get_human_lines`, not this shape.
fn document_get_result_json(doc: &DocumentGetResult) -> serde_json::Value {
    json!({
        "id": doc.id,
        "uri": doc.uri,
        "title": doc.title,
        "store_id": doc.store_id,
        "source_id": doc.source_id,
        "content_hash": doc.content_hash,
        "fetched_at": doc.fetched_at,
        "metadata": doc.metadata,
        "text": doc.text,
        "date_original": doc.date_original,
        "date_parsed": doc.date_parsed,
        "index_updated_at": doc.index_updated_at,
    })
}

fn render_document_get(doc: &DocumentGetResult, include_text: bool, json_mode: bool) {
    if json_mode {
        print_json(&document_get_result_json(doc));
        return;
    }
    for line in document_get_human_lines(doc, include_text) {
        println!("{line}");
    }
}

/// `localdb document get <id>`
pub fn run_document_get(ctx: &CliContext, id: &str, include_text: bool) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_document_get_async(ctx, id, include_text));
}

pub(crate) async fn run_document_get_async(ctx: &CliContext, id: &str, include_text: bool) {
    let config_loader = load_config_scaffolded(ctx).await;
    let doc = dispatch(&DocumentGetCmd { id }, ctx, &config_loader, || {
        open_app_db_or_exit(ctx, &config_loader)
    })
    .await;
    render_document_get(&doc, include_text, ctx.json);
}

#[cfg(test)]
mod tests;
