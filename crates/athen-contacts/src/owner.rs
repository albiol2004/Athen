//! Owner-contact helpers.
//!
//! The "owner" is the single contact representing Athen's user. Every
//! identifier attached to the owner contact (Telegram user id, email
//! addresses, future phone numbers, …) grants `TrustLevel::AuthUser` to
//! inbound events on its channel — collapsing the previously per-channel
//! owner concept (Telegram-only `owner_user_id`) into a unified
//! cross-channel store backed by the existing contacts infrastructure.

use std::sync::Arc;

use crate::ContactStore;

/// Snapshots the owner's attached identifiers and answers the sense
/// monitors' single question: "is this inbound (scheme, value) pair the
/// owner?"
///
/// Holds an `Arc<dyn ContactStore>` so cloning is cheap; intended to be
/// built once per app and shared with every sense monitor. Lookups are
/// async because the underlying store is async, but each call is a
/// single-row SQL hit — fast enough to run per inbound message without
/// caching.
#[derive(Clone)]
pub struct OwnerLookup {
    store: Arc<dyn ContactStore>,
}

impl OwnerLookup {
    pub fn new(store: Arc<dyn ContactStore>) -> Self {
        Self { store }
    }

    /// `true` if `(scheme, value)` matches one of the owner contact's
    /// identifiers. Email values are compared case-insensitively;
    /// callers should still pass the canonical (lowercase) form because
    /// the store stores them that way and many call sites do an exact
    /// match elsewhere.
    pub async fn is_owner_identifier(&self, scheme: &str, value: &str) -> bool {
        let owner = match self.store.find_owner().await {
            Ok(Some(c)) => c,
            _ => return false,
        };
        let scheme_lc = scheme.to_ascii_lowercase();
        let value_norm = normalize_identifier_value(&scheme_lc, value);
        owner.identifiers.iter().any(|ident| {
            let ident_scheme_lc = identifier_kind_scheme(ident.kind).to_ascii_lowercase();
            ident_scheme_lc == scheme_lc
                && normalize_identifier_value(&scheme_lc, &ident.value) == value_norm
        })
    }

    /// Return every `(scheme, value)` pair the owner contact carries,
    /// or empty when no owner is set. Used by the disjointness validator
    /// to prevent assigning an owner identifier to a different contact.
    pub async fn owner_identifiers(&self) -> Vec<(String, String)> {
        let owner = match self.store.find_owner().await {
            Ok(Some(c)) => c,
            _ => return Vec::new(),
        };
        owner
            .identifiers
            .iter()
            .map(|i| {
                let scheme = identifier_kind_scheme(i.kind).to_string();
                let value = normalize_identifier_value(&scheme, &i.value);
                (scheme, value)
            })
            .collect()
    }
}

/// Map an [`athen_core::contact::IdentifierKind`] to the lowercase
/// "scheme" string the sense layer speaks (`"email"`, `"telegram_user"`,
/// …). Kept in this crate so the sense modules don't have to know about
/// `IdentifierKind` directly.
fn identifier_kind_scheme(kind: athen_core::contact::IdentifierKind) -> &'static str {
    use athen_core::contact::IdentifierKind;
    match kind {
        IdentifierKind::Email => "email",
        IdentifierKind::Phone => "phone",
        // We use `telegram_user` (not just `telegram`) because the
        // identifier is the numeric Telegram user_id, not the @username
        // — important so a future @username scheme can coexist.
        IdentifierKind::Telegram => "telegram_user",
        IdentifierKind::WhatsApp => "whatsapp",
        IdentifierKind::IMessage => "imessage",
        IdentifierKind::Signal => "signal",
        IdentifierKind::Discord => "discord",
        IdentifierKind::Slack => "slack",
        IdentifierKind::Twitter => "twitter",
        IdentifierKind::Username => "username",
        IdentifierKind::Other => "other",
    }
}

/// Normalize an identifier value before comparison. Today: only
/// lowercase email addresses; everything else is passed through. Phone
/// normalization (E.164) is intentionally out of scope until the
/// messaging-sense work needs it.
fn normalize_identifier_value(scheme: &str, value: &str) -> String {
    match scheme {
        "email" => value.to_ascii_lowercase(),
        _ => value.to_string(),
    }
}

/// Validate that none of the `candidates` (scheme, value) pairs clash
/// with the owner's attached identifiers. Returns the conflicting pairs
/// when one or more overlap, `Ok(())` otherwise.
///
/// Intended for the config-save / "add identifier" path so we never end
/// up with the owner's email accidentally tagged to a different contact
/// — which would let an unauthenticated sender masquerade as the owner.
///
/// Phase 1 ships this helper without wiring it; the Settings UI hook
/// lands in a follow-up. Tests in this crate cover the helper itself.
pub fn assert_disjoint_from_owner(
    owner_identifiers: &[(String, String)],
    candidates: &[(String, String)],
) -> std::result::Result<(), Vec<(String, String)>> {
    let owner_set: std::collections::HashSet<(String, String)> = owner_identifiers
        .iter()
        .map(|(s, v)| (s.to_ascii_lowercase(), normalize_identifier_value(s, v)))
        .collect();
    let conflicts: Vec<(String, String)> = candidates
        .iter()
        .map(|(s, v)| {
            let s = s.to_ascii_lowercase();
            let v = normalize_identifier_value(&s, v);
            (s, v)
        })
        .filter(|pair| owner_set.contains(pair))
        .collect();
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(conflicts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemoryContactStore;
    use athen_core::contact::{Contact, ContactIdentifier, IdentifierKind, TrustLevel};
    use uuid::Uuid;

    fn make_contact(name: &str, identifiers: Vec<(&str, IdentifierKind)>) -> Contact {
        Contact {
            id: Uuid::new_v4(),
            name: name.to_string(),
            trust_level: TrustLevel::AuthUser,
            trust_manual_override: true,
            identifiers: identifiers
                .into_iter()
                .map(|(value, kind)| ContactIdentifier {
                    value: value.to_string(),
                    kind,
                })
                .collect(),
            interaction_count: 0,
            last_interaction: None,
            notes: None,
            blocked: false,
            is_owner: false,
        }
    }

    #[tokio::test]
    async fn find_owner_returns_none_when_unset() {
        let store = Arc::new(InMemoryContactStore::new());
        let lookup = OwnerLookup::new(store);
        assert!(
            !lookup
                .is_owner_identifier("email", "anyone@nowhere.test")
                .await
        );
        assert!(lookup.owner_identifiers().await.is_empty());
    }

    #[tokio::test]
    async fn is_owner_identifier_positive_email_and_telegram() {
        let store: Arc<dyn ContactStore> = Arc::new(InMemoryContactStore::new());
        let mut owner = make_contact(
            "Alex",
            vec![
                ("alex@example.com", IdentifierKind::Email),
                ("987654321", IdentifierKind::Telegram),
            ],
        );
        owner.is_owner = true;
        let id = owner.id;
        store.save(&owner).await.unwrap();
        store.set_owner(&id).await.unwrap();

        let lookup = OwnerLookup::new(store);
        // Case-insensitive email match.
        assert!(
            lookup
                .is_owner_identifier("email", "ALEX@example.com")
                .await
        );
        // Telegram user id match.
        assert!(
            lookup
                .is_owner_identifier("telegram_user", "987654321")
                .await
        );
        // Wrong scheme: same value but treated as username doesn't match.
        assert!(!lookup.is_owner_identifier("username", "987654321").await);
        // Negative.
        assert!(!lookup.is_owner_identifier("email", "stranger@x.com").await);
    }

    #[tokio::test]
    async fn set_owner_clears_previous_owner() {
        let store: Arc<dyn ContactStore> = Arc::new(InMemoryContactStore::new());
        let mut a = make_contact("Alice", vec![("a@x.com", IdentifierKind::Email)]);
        let mut b = make_contact("Bob", vec![("b@x.com", IdentifierKind::Email)]);
        a.is_owner = true;
        b.is_owner = false;
        let a_id = a.id;
        let b_id = b.id;
        store.save(&a).await.unwrap();
        store.save(&b).await.unwrap();
        store.set_owner(&a_id).await.unwrap();
        assert_eq!(store.find_owner().await.unwrap().unwrap().id, a_id);

        store.set_owner(&b_id).await.unwrap();
        let owner = store.find_owner().await.unwrap().unwrap();
        assert_eq!(owner.id, b_id);
        // a is no longer flagged.
        let reloaded_a = store.load(a_id).await.unwrap().unwrap();
        assert!(!reloaded_a.is_owner);
    }

    #[test]
    fn assert_disjoint_returns_conflicts() {
        let owner = vec![
            ("email".into(), "alex@example.com".into()),
            ("telegram_user".into(), "42".into()),
        ];
        let candidates = vec![
            ("email".into(), "stranger@x.com".into()),
            ("email".into(), "ALEX@example.com".into()), // case-insensitive clash
        ];
        let err = assert_disjoint_from_owner(&owner, &candidates).unwrap_err();
        assert_eq!(err, vec![("email".into(), "alex@example.com".into())]);
    }

    #[test]
    fn assert_disjoint_passes_when_no_overlap() {
        let owner = vec![("email".into(), "alex@example.com".into())];
        let candidates = vec![("email".into(), "other@x.com".into())];
        assert!(assert_disjoint_from_owner(&owner, &candidates).is_ok());
    }

    #[test]
    fn assert_disjoint_empty_inputs() {
        assert!(assert_disjoint_from_owner(&[], &[]).is_ok());
        assert!(assert_disjoint_from_owner(&[("email".into(), "x@y.com".into())], &[]).is_ok());
        assert!(assert_disjoint_from_owner(&[], &[("email".into(), "x@y.com".into())]).is_ok());
    }
}
