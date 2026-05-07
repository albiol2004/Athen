//! Attachment-handling policy: decides per-attachment whether the bytes
//! get downloaded, whether they get auto-inlined into the agent's first
//! turn, or whether the attachment is dropped entirely.
//!
//! Three independent gates:
//! - **MIME**: only known-safe types are auto-fetched. Executables and
//!   archives are blocked by default (the user can broaden via Settings).
//! - **Size**: per-attachment and per-event caps so a 50 MB ZIP from
//!   spam can't fill user storage or fork a vision-token bill.
//! - **Sender trust**: high-trust contacts auto-inline; low-trust senders
//!   either save bytes for the agent to opt into, or are skipped entirely.
//!
//! The policy is pure data — no I/O, no state. The caller passes mime +
//! size + trust and gets back a `Decision`. Sense crates apply it before
//! downloading; the executor applies it again before inlining (defence
//! in depth — a low-trust attachment that slipped past the sense gate
//! still won't auto-inline).

use serde::{Deserialize, Serialize};

use crate::contact::TrustLevel;

/// What to do with a single incoming attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentDecision {
    /// Skip entirely — no download, no metadata record. For senders we
    /// won't fetch from at all (blocked MIME, blocked sender, oversize).
    Skip,
    /// Download bytes + record metadata, but do NOT auto-inline into the
    /// first agent turn. Agent has to opt in by calling a tool. Reserved
    /// for low-trust senders where we don't want to spend vision tokens
    /// or expose the model to potentially-poisoned content unprompted.
    SaveOnly,
    /// Download bytes + record metadata + auto-inline into the first
    /// agent turn (multimodal block for images, document block or
    /// extracted text for PDFs). Reserved for trusted senders.
    SaveAndInline,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentPolicy {
    /// Allowed MIME prefixes. A MIME is allowed if it starts with any
    /// of these. Defaults cover images, PDF, plain text, and common
    /// office docs.
    pub mime_allowlist: Vec<String>,
    /// Per-attachment hard cap in bytes. Anything above is `Skip`.
    pub max_attachment_bytes: u64,
    /// Per-event hard cap on total attachment bytes. Once a SenseEvent's
    /// running total crosses this, remaining attachments are `Skip`.
    pub max_event_bytes: u64,
    /// Minimum trust level required to auto-inline. Below this, decision
    /// is downgraded from `SaveAndInline` to `SaveOnly`.
    pub min_inline_trust: TrustLevel,
    /// Minimum trust level required to even download bytes. Below this,
    /// decision is `Skip`.
    pub min_download_trust: TrustLevel,
    /// TTL in days before the bytes are purged. Extracted text sidecars
    /// outlive this and stay forever (small, useful for arc continuity).
    pub byte_ttl_days: u32,
}

impl Default for AttachmentPolicy {
    fn default() -> Self {
        Self {
            mime_allowlist: vec![
                "image/".into(),
                "application/pdf".into(),
                "text/".into(),
                "application/vnd.openxmlformats-officedocument".into(),
                "application/msword".into(),
                "application/vnd.ms-excel".into(),
                "application/vnd.ms-powerpoint".into(),
                "application/json".into(),
                "application/xml".into(),
            ],
            max_attachment_bytes: 10 * 1024 * 1024,
            max_event_bytes: 25 * 1024 * 1024,
            min_inline_trust: TrustLevel::Known,
            min_download_trust: TrustLevel::Unknown,
            byte_ttl_days: 30,
        }
    }
}

impl AttachmentPolicy {
    pub fn mime_allowed(&self, mime: &str) -> bool {
        let lower = mime.to_ascii_lowercase();
        self.mime_allowlist.iter().any(|p| lower.starts_with(p))
    }

    /// Decide what to do with a single attachment.
    ///
    /// `event_bytes_so_far` is the cumulative size of attachments
    /// already accepted for the same SenseEvent. Lets us enforce the
    /// per-event cap without the caller tracking it inline.
    pub fn decide(
        &self,
        mime: &str,
        size_bytes: u64,
        trust: TrustLevel,
        event_bytes_so_far: u64,
    ) -> AttachmentDecision {
        if !self.mime_allowed(mime) {
            return AttachmentDecision::Skip;
        }
        if size_bytes > self.max_attachment_bytes {
            return AttachmentDecision::Skip;
        }
        if event_bytes_so_far.saturating_add(size_bytes) > self.max_event_bytes {
            return AttachmentDecision::Skip;
        }
        if trust < self.min_download_trust {
            return AttachmentDecision::Skip;
        }
        if trust < self.min_inline_trust {
            return AttachmentDecision::SaveOnly;
        }
        AttachmentDecision::SaveAndInline
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_inline_for_trusted_image() {
        let p = AttachmentPolicy::default();
        let d = p.decide("image/png", 100_000, TrustLevel::Trusted, 0);
        assert_eq!(d, AttachmentDecision::SaveAndInline);
    }

    #[test]
    fn unknown_sender_only_saves_bytes() {
        let p = AttachmentPolicy::default();
        let d = p.decide("image/png", 100_000, TrustLevel::Unknown, 0);
        assert_eq!(d, AttachmentDecision::SaveOnly);
    }

    #[test]
    fn oversize_skipped() {
        let p = AttachmentPolicy::default();
        let big = p.max_attachment_bytes + 1;
        let d = p.decide("image/png", big, TrustLevel::AuthUser, 0);
        assert_eq!(d, AttachmentDecision::Skip);
    }

    #[test]
    fn per_event_budget_skips_remaining() {
        let p = AttachmentPolicy::default();
        let almost_full = p.max_event_bytes - 1024;
        // First fits in remaining slack:
        let d1 = p.decide("image/png", 512, TrustLevel::AuthUser, almost_full);
        assert_eq!(d1, AttachmentDecision::SaveAndInline);
        // Second would push over:
        let d2 = p.decide("image/png", 4096, TrustLevel::AuthUser, almost_full);
        assert_eq!(d2, AttachmentDecision::Skip);
    }

    #[test]
    fn blocked_mime_always_skipped_even_for_owner() {
        let p = AttachmentPolicy::default();
        let d = p.decide(
            "application/x-msdownload",
            1024,
            TrustLevel::AuthUser,
            0,
        );
        assert_eq!(d, AttachmentDecision::Skip);
    }

    #[test]
    fn case_insensitive_mime_match() {
        let p = AttachmentPolicy::default();
        let d = p.decide("IMAGE/PNG", 1024, TrustLevel::Trusted, 0);
        assert_eq!(d, AttachmentDecision::SaveAndInline);
    }
}
