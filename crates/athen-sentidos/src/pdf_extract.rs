//! Pure-Rust PDF text extraction, run eagerly when a PDF attachment
//! lands. The extracted text is cached as a `<file>.txt` sidecar that
//! outlives the original bytes after the TTL purger sweeps them — so
//! arc continuity ("what did that PDF say?") survives byte purge.
//!
//! Two responsibilities only:
//! - Take a PDF path, write a sidecar, return its path.
//! - Truncate cached text to fit the executor's inline budget for
//!   non-document-capable models, returning an `InlineSnippet` with
//!   metadata so the executor can dangle a "call read_attachment_full
//!   for the rest" tool hint when there's more to read.
//!
//! Heavier extraction (layout-aware tables, OCR for scans) is a
//! separate, optional path that lives behind a Python tool call. This
//! module is the always-available, zero-system-deps baseline.

use std::path::{Path, PathBuf};

use athen_core::error::{AthenError, Result};

/// A snippet ready to drop into the agent's first turn. `truncated`
/// tells the caller whether it should advertise a follow-up tool that
/// returns the full extracted text.
#[derive(Debug, Clone)]
pub struct InlineSnippet {
    pub text: String,
    pub total_chars: usize,
    pub truncated: bool,
}

/// Default character budget for inlined PDF text. ~6 KB lands at
/// roughly 1.5 K tokens — enough for most invoices, receipts, and
/// single-page docs to fit fully, while still leaving room in the
/// context for the agent to think and respond.
pub const DEFAULT_INLINE_CHAR_BUDGET: usize = 6_000;

/// Extract text from `pdf_path` and persist it next to the file as
/// `<pdf_path>.txt`. Returns the sidecar path on success.
///
/// `pdf-extract` is synchronous and CPU-bound — the caller should drive
/// this on a blocking pool (`tokio::task::spawn_blocking` from async
/// contexts) so the runtime worker thread isn't pinned.
pub fn extract_to_sidecar(pdf_path: &Path) -> Result<PathBuf> {
    let text = extract_text(pdf_path)?;
    let sidecar = sidecar_path(pdf_path);
    std::fs::write(&sidecar, &text)
        .map_err(|e| AthenError::Other(format!("write pdf sidecar {sidecar:?}: {e}")))?;
    Ok(sidecar)
}

/// Run the actual extraction. Surfaced as its own function so callers
/// who want the text in memory can skip the sidecar write.
pub fn extract_text(pdf_path: &Path) -> Result<String> {
    pdf_extract::extract_text(pdf_path)
        .map_err(|e| AthenError::Other(format!("pdf-extract {pdf_path:?}: {e}")))
}

/// Where the cached `.txt` sidecar lives for a given PDF path.
pub fn sidecar_path(pdf_path: &Path) -> PathBuf {
    let mut s = pdf_path.as_os_str().to_os_string();
    s.push(".txt");
    PathBuf::from(s)
}

/// Build the snippet to inline into the agent's first turn. If the
/// full text fits in the budget, no truncation marker is added — the
/// agent doesn't need a "call this tool for more" hint when there's
/// nothing more.
pub fn truncate_for_inline(text: &str, max_chars: usize) -> InlineSnippet {
    let total_chars = text.chars().count();
    if total_chars <= max_chars {
        return InlineSnippet {
            text: text.to_string(),
            total_chars,
            truncated: false,
        };
    }
    // Floor to char boundary so we don't slice mid-codepoint.
    let mut byte_end = 0;
    for (idx, (b, _)) in text.char_indices().enumerate() {
        if idx == max_chars {
            byte_end = b;
            break;
        }
        byte_end = b + text[b..].chars().next().unwrap().len_utf8();
    }
    InlineSnippet {
        text: text[..byte_end].to_string(),
        total_chars,
        truncated: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_path_appends_txt() {
        let p = Path::new("/tmp/foo.pdf");
        assert_eq!(sidecar_path(p), Path::new("/tmp/foo.pdf.txt"));
    }

    #[test]
    fn sidecar_path_handles_no_extension() {
        let p = Path::new("/tmp/raw");
        assert_eq!(sidecar_path(p), Path::new("/tmp/raw.txt"));
    }

    #[test]
    fn truncate_keeps_short_text_untouched() {
        let snippet = truncate_for_inline("hello", 100);
        assert_eq!(snippet.text, "hello");
        assert!(!snippet.truncated);
        assert_eq!(snippet.total_chars, 5);
    }

    #[test]
    fn truncate_cuts_long_text_and_flags_truncation() {
        let long = "a".repeat(20_000);
        let snippet = truncate_for_inline(&long, 6_000);
        assert_eq!(snippet.text.len(), 6_000);
        assert!(snippet.truncated);
        assert_eq!(snippet.total_chars, 20_000);
    }

    #[test]
    fn truncate_respects_char_boundary_for_multibyte() {
        // Each emoji is 4 bytes / 1 codepoint. 5 codepoints = 20 bytes.
        let s: String = "🦀".repeat(5);
        let snippet = truncate_for_inline(&s, 3);
        assert_eq!(snippet.text.chars().count(), 3);
        assert!(snippet.truncated);
    }

    #[test]
    fn extract_text_propagates_pdfextract_errors() {
        // Pass a path that isn't a real PDF — pdf-extract should fail
        // and we should surface the error rather than panic.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"this is not a pdf").unwrap();
        let result = extract_text(tmp.path());
        assert!(result.is_err());
    }
}
