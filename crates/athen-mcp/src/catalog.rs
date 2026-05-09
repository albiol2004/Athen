//! Built-in catalog of branded MCPs available to the user.
//!
//! Adding a new entry here makes it appear in the Tools tab of the UI.
//! Whether the underlying binary actually ships (or has to be downloaded)
//! is encoded in `McpSource`.
//!
//! Currently empty: the original `files` entry was removed once the
//! built-in `read`/`edit`/`write`/`grep`/`list_directory` tools became
//! canonical. The catalog stays as the extension point for future
//! branded MCPs (Slack, Notion, etc.).

use athen_core::traits::mcp::McpCatalogEntry;

/// Return the full hardcoded catalog of branded MCPs.
pub fn builtin_catalog() -> Vec<McpCatalogEntry> {
    Vec::new()
}

/// Find an entry by id.
pub fn lookup(id: &str) -> Option<McpCatalogEntry> {
    builtin_catalog().into_iter().find(|e| e.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_empty_after_files_removal() {
        assert!(builtin_catalog().is_empty());
    }

    #[test]
    fn lookup_returns_none_for_removed_files_entry() {
        assert!(lookup("files").is_none());
        assert!(lookup("does-not-exist").is_none());
    }
}
