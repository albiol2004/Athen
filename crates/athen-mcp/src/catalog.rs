//! Built-in catalog of branded MCPs available to the user.
//!
//! Adding a new entry here makes it appear in the Tools tab of the UI.
//! Whether the underlying binary actually ships (or has to be downloaded)
//! is encoded in `McpSource`.

use athen_core::risk::BaseImpact;
use athen_core::traits::mcp::{McpCatalogEntry, McpSource};
use serde_json::json;

/// Return the full hardcoded catalog of branded MCPs.
pub fn builtin_catalog() -> Vec<McpCatalogEntry> {
    vec![files_entry()]
}

/// Find an entry by id.
pub fn lookup(id: &str) -> Option<McpCatalogEntry> {
    builtin_catalog().into_iter().find(|e| e.id == id)
}

fn files_entry() -> McpCatalogEntry {
    McpCatalogEntry {
        id: "files".to_string(),
        display_name: "Files".to_string(),
        description: "Read, write, and organize files in a sandboxed folder. \
                      All operations are confined to the folder you choose."
            .to_string(),
        icon: Some("folder".to_string()),
        config_schema: json!({
            "type": "object",
            "properties": {
                "sandbox_root": {
                    "type": "string",
                    "title": "Sandbox folder",
                    "description": "Absolute path to the folder Files can access. \
                                    Defaults to ~/.athen/files."
                }
            },
            "required": ["sandbox_root"]
        }),
        source: McpSource::Bundled {
            binary_name: "mcp-filesystem".to_string(),
        },
        base_risk: BaseImpact::WritePersist,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_files() {
        let cat = builtin_catalog();
        assert!(cat.iter().any(|e| e.id == "files"));
    }

    #[test]
    fn lookup_works() {
        assert!(lookup("files").is_some());
        assert!(lookup("does-not-exist").is_none());
    }
}
