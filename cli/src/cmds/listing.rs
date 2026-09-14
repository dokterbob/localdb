//! Shared rendering for store-scoped `<noun> list` commands (`source list`,
//! `document list`).

use serde_json::json;

use crate::normalize::print_json;

#[cfg(test)]
mod tests;

/// Width of the store-name column: longest name in scope plus two spaces of
/// separation before the item line begins. Only used when `>1` store is in
/// scope; callers pass `0` (ignored) otherwise.
pub(crate) fn store_column_width<'a>(names: impl Iterator<Item = &'a str>) -> usize {
    names.map(str::len).max().unwrap_or(0) + 2
}

/// One row of a store-scoped `<noun> list` command — the per-type behavior
/// `render_scoped_list` needs: the `--json` envelope key, the empty-scope
/// noun, and the two renderings of a single row.
pub(crate) trait ScopedListItem {
    /// Key wrapping the rows in `--json` output (`{"documents": [...]}`).
    const JSON_KEY: &'static str;
    /// Noun in the empty-scope message ("No documents on store 'x'.").
    const EMPTY_NOUN: &'static str;
    /// This row's `--json` representation.
    fn json_row(&self) -> serde_json::Value;
    /// This row's human-readable line, with or without a leading store-name
    /// column. `col_width` is only consulted when `with_store_column` is
    /// true.
    fn human_line(&self, with_store_column: bool, col_width: usize) -> String;
}

/// Render a store-scoped list the way every `<noun> list` command does:
/// `--json` prints `{"<json_key>": [rows…]}`; an empty result prints
/// `No <noun> on store '<name>'.` for a single-store scope and
/// `No <noun> in scope.` otherwise; a non-empty result prints one line per
/// item, gaining a leading store-name column only when more than one store
/// is in scope (specs/05-surfaces.md §2.2).
pub(crate) fn render_scoped_list<T: ScopedListItem>(
    items: &[T],
    scope_store_names: &[String],
    json_mode: bool,
) {
    if json_mode {
        let rows: Vec<serde_json::Value> = items.iter().map(ScopedListItem::json_row).collect();
        print_json(&json!({ T::JSON_KEY: rows }));
        return;
    }

    if items.is_empty() {
        if scope_store_names.len() == 1 {
            println!("No {} on store '{}'.", T::EMPTY_NOUN, scope_store_names[0]);
        } else {
            println!("No {} in scope.", T::EMPTY_NOUN);
        }
        return;
    }

    let multi = scope_store_names.len() > 1;
    let col_width = if multi {
        store_column_width(scope_store_names.iter().map(String::as_str))
    } else {
        0
    };
    for item in items {
        println!("{}", item.human_line(multi, col_width));
    }
}
