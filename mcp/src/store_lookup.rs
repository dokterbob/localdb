//! One shared rule for resolving a caller-supplied store argument (an id or
//! a name) against a list of candidates: an id match always wins over a
//! name match, because ids are unique and machine-generated while names are
//! user-chosen and may legitimately collide with some other item's id.
//!
//! Used both when resolving directly against the live store set
//! (`tools::select_mcp_stores`) and when replaying that exact resolution
//! against an upstream daemon's advertised store set from inside the MCP
//! proxy (`proxy::ProxyScope::resolve`) — the two must agree, or a store
//! that is in scope for one path could be mis-resolved by the other.

/// Resolve `value` against `items` by id first, then by name.
pub(crate) fn resolve_by_id_then_name<'a, T>(
    items: &'a [T],
    value: &str,
    id: impl Fn(&T) -> &str,
    name: impl Fn(&T) -> &str,
) -> Option<&'a T> {
    items
        .iter()
        .find(|item| id(item) == value)
        .or_else(|| items.iter().find(|item| name(item) == value))
}

#[cfg(test)]
mod tests;
