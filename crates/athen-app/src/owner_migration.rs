//! One-shot migration that promotes the legacy
//! `TelegramConfig::owner_user_id` into a proper owner contact in the
//! unified contact store.
//!
//! Runs once on startup (idempotent): if a contact is already flagged
//! `is_owner`, nothing happens. If not, and the user has configured a
//! Telegram owner user_id at any point, we create a single "Owner"
//! contact with `TrustLevel::AuthUser`, attach the `telegram_user`
//! identifier, and mark it as the owner.
//!
//! Phase 1 of the cross-channel owner identity unification. The
//! Telegram fallback path inside the sense monitor keeps reading
//! `owner_user_id` directly for one release so first-boot before this
//! migration fires still works; the field will be retired afterwards.

use athen_contacts::ContactStore;
use athen_core::config::TelegramConfig;
use athen_core::contact::{Contact, ContactIdentifier, IdentifierKind, TrustLevel};
use uuid::Uuid;

/// Idempotently mirror the legacy Telegram owner id into the contact
/// store. Returns `true` when a new owner contact was created, `false`
/// when nothing needed to change.
pub async fn migrate_telegram_owner_to_contacts(
    store: &dyn ContactStore,
    telegram_config: &TelegramConfig,
) -> bool {
    // Already migrated, or already a manually-set owner — do nothing.
    if let Ok(Some(_)) = store.find_owner().await {
        return false;
    }

    let owner_user_id = match telegram_config.owner_user_id {
        Some(id) => id,
        None => return false,
    };

    let owner_id = Uuid::new_v4();
    let contact = Contact {
        id: owner_id,
        name: "Owner".to_string(),
        // AuthUser trust so the existing risk fast-pass fires once the
        // coordinator's `resolve_sender_trust` finds this contact by
        // its Telegram identifier.
        trust_level: TrustLevel::AuthUser,
        // Manual override: this is the user themselves; auto-evolution
        // (record_approval/record_rejection) must not touch it.
        trust_manual_override: true,
        identifiers: vec![ContactIdentifier {
            kind: IdentifierKind::Telegram,
            value: owner_user_id.to_string(),
        }],
        interaction_count: 0,
        last_interaction: None,
        notes: Some(
            "Auto-migrated from TelegramConfig::owner_user_id. Add email/phone identifiers in Settings to extend AuthUser trust to other channels.".into(),
        ),
        blocked: false,
        is_owner: true,
    };

    if let Err(e) = store.save(&contact).await {
        tracing::warn!("Owner migration: failed to save owner contact: {e}");
        return false;
    }
    if let Err(e) = store.set_owner(&owner_id).await {
        tracing::warn!("Owner migration: failed to flag owner: {e}");
        return false;
    }

    tracing::info!(
        owner_id = %owner_id,
        telegram_user_id = owner_user_id,
        "Migrated legacy Telegram owner_user_id into the unified contact store"
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_contacts::InMemoryContactStore;

    fn config_with_owner(id: Option<i64>) -> TelegramConfig {
        TelegramConfig {
            enabled: true,
            bot_token: "x".into(),
            owner_user_id: id,
            allowed_chat_ids: vec![],
            poll_interval_secs: 5,
        }
    }

    #[tokio::test]
    async fn migration_creates_owner_when_id_set_and_no_owner_exists() {
        let store = InMemoryContactStore::new();
        let cfg = config_with_owner(Some(12345));
        assert!(migrate_telegram_owner_to_contacts(&store, &cfg).await);
        let owner = store.find_owner().await.unwrap().expect("owner");
        assert!(owner.is_owner);
        assert_eq!(owner.trust_level, TrustLevel::AuthUser);
        assert_eq!(owner.identifiers.len(), 1);
        assert_eq!(owner.identifiers[0].kind, IdentifierKind::Telegram);
        assert_eq!(owner.identifiers[0].value, "12345");
    }

    #[tokio::test]
    async fn migration_is_idempotent() {
        let store = InMemoryContactStore::new();
        let cfg = config_with_owner(Some(12345));
        assert!(migrate_telegram_owner_to_contacts(&store, &cfg).await);
        // Second run is a no-op.
        assert!(!migrate_telegram_owner_to_contacts(&store, &cfg).await);
        // Still exactly one owner.
        let all = store.list_all().await.unwrap();
        let owners: Vec<_> = all.iter().filter(|c| c.is_owner).collect();
        assert_eq!(owners.len(), 1);
    }

    #[tokio::test]
    async fn migration_skips_when_no_legacy_id_configured() {
        let store = InMemoryContactStore::new();
        let cfg = config_with_owner(None);
        assert!(!migrate_telegram_owner_to_contacts(&store, &cfg).await);
        assert!(store.find_owner().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn migration_skips_when_owner_already_set_manually() {
        let store = InMemoryContactStore::new();
        // Pre-existing owner with a different identifier.
        let mut existing = Contact {
            id: Uuid::new_v4(),
            name: "Manual".into(),
            trust_level: TrustLevel::AuthUser,
            trust_manual_override: true,
            identifiers: vec![ContactIdentifier {
                kind: IdentifierKind::Email,
                value: "me@x.com".into(),
            }],
            interaction_count: 0,
            last_interaction: None,
            notes: None,
            blocked: false,
            is_owner: true,
        };
        let id = existing.id;
        existing.is_owner = true;
        store.save(&existing).await.unwrap();
        store.set_owner(&id).await.unwrap();

        let cfg = config_with_owner(Some(99999));
        assert!(!migrate_telegram_owner_to_contacts(&store, &cfg).await);

        let owner = store.find_owner().await.unwrap().unwrap();
        // Still the manual one; we didn't overwrite.
        assert_eq!(owner.id, id);
    }
}
