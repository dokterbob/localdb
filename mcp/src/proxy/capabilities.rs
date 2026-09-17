//! Search capability discovery on the authenticated upstream connection.

use super::{upstream_error_to_mcp, McpError, PaginatedRequestParams, ProxyHandler};

impl ProxyHandler {
    /// Discover on the authenticated session; cache only completed discovery.
    pub(super) async fn supports_search_dedup(&self) -> Result<bool, McpError> {
        let mut cached = self.search_dedup_supported.lock().await;
        if let Some(supported) = *cached {
            return Ok(supported);
        }
        let mut cursor = None;
        let mut seen = std::collections::HashSet::new();
        loop {
            let page = self
                .upstream
                .list_tools(
                    cursor
                        .map(|cursor| PaginatedRequestParams::default().with_cursor(Some(cursor))),
                )
                .await
                .map_err(upstream_error_to_mcp)?;
            if let Some(search) = page.tools.iter().find(|tool| tool.name == "search") {
                let supported = search
                    .input_schema
                    .get("properties")
                    .and_then(|v| v.as_object())
                    .is_some_and(|properties| properties.contains_key("dedup"));
                *cached = Some(supported);
                return Ok(supported);
            }
            match page.next_cursor {
                Some(next) if seen.insert(next.clone()) => cursor = Some(next),
                Some(_) => {
                    return Err(McpError::internal_error(
                        "mcp proxy: repeated tools/list pagination cursor",
                        None,
                    ))
                }
                None => {
                    *cached = Some(false);
                    return Ok(false);
                }
            }
        }
    }
}
