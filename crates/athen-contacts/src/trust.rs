//! Trust level business logic and risk multiplier calculations.

use athen_core::contact::{
    Contact, ContactId, ContactIdentifier, IdentifierKind, TrustLevel,
};
use athen_core::error::{AthenError, Result};
use chrono::Utc;
use uuid::Uuid;

use crate::ContactStore;

/// Threshold of approved interactions before considering a trust upgrade.
const APPROVAL_UPGRADE_THRESHOLD: u32 = 5;

/// Threshold of rejected interactions before considering a trust downgrade.
const REJECTION_DOWNGRADE_THRESHOLD: u32 = 3;

/// Manages contact trust levels and implicit trust evolution.
pub struct TrustManager {
    store: Box<dyn ContactStore>,
}

impl TrustManager {
    /// Create a new `TrustManager` backed by the given store.
    pub fn new(store: Box<dyn ContactStore>) -> Self {
        Self { store }
    }

    /// Resolve a sender identifier to a contact, creating a new T0 contact
    /// if none exists.
    pub async fn resolve_contact(
        &self,
        identifier: &str,
        kind: IdentifierKind,
    ) -> Result<Contact> {
        if let Some(contact) = self.store.find_by_identifier(identifier).await? {
            return Ok(contact);
        }

        let contact = Contact {
            id: Uuid::new_v4(),
            name: identifier.to_string(),
            trust_level: TrustLevel::Unknown,
            trust_manual_override: false,
            identifiers: vec![ContactIdentifier {
                kind,
                value: identifier.to_string(),
            }],
            interaction_count: 0,
            last_interaction: None,
            notes: None,
            blocked: false,
        };
        self.store.save(&contact).await?;
        Ok(contact)
    }

    /// Get the risk multiplier (M_origen) for a contact.
    pub fn risk_multiplier(&self, contact: &Contact) -> f64 {
        if contact.blocked {
            // Blocked contacts get the highest multiplier.
            return TrustLevel::Unknown.risk_multiplier();
        }
        contact.trust_level.risk_multiplier()
    }

    /// Record a positive interaction (user approved an action from this contact).
    ///
    /// After every `APPROVAL_UPGRADE_THRESHOLD` approvals the trust level is
    /// considered for an upgrade. Auto-upgrades never go past T2 (Known) and
    /// never override manually set levels.
    pub async fn record_approval(&self, contact_id: ContactId) -> Result<()> {
        let mut contact = self
            .store
            .load(contact_id)
            .await?
            .ok_or_else(|| AthenError::Other(format!("Contact not found: {contact_id}")))?;

        contact.interaction_count += 1;
        contact.last_interaction = Some(Utc::now());

        // Consider upgrade if we've crossed a threshold boundary.
        if !contact.trust_manual_override
            && contact.interaction_count % APPROVAL_UPGRADE_THRESHOLD == 0
        {
            contact.trust_level = match contact.trust_level {
                TrustLevel::Unknown => TrustLevel::Neutral,
                TrustLevel::Neutral => TrustLevel::Known,
                // Never auto-upgrade past T2.
                other => other,
            };
        }

        self.store.save(&contact).await
    }

    /// Record a negative interaction (user rejected an action from this contact).
    ///
    /// Tracks rejections via a simple counter derived from `interaction_count`
    /// (we use the negative-interaction count stored separately). To keep
    /// the struct unchanged we track rejections by decrementing
    /// `interaction_count` — but a cleaner approach uses a dedicated field.
    ///
    /// Here we keep a lightweight approach: every call bumps a rejection
    /// counter stored in `notes` as a JSON snippet, and every
    /// `REJECTION_DOWNGRADE_THRESHOLD` rejections we consider downgrading.
    pub async fn record_rejection(&self, contact_id: ContactId) -> Result<()> {
        let mut contact = self
            .store
            .load(contact_id)
            .await?
            .ok_or_else(|| AthenError::Other(format!("Contact not found: {contact_id}")))?;

        contact.last_interaction = Some(Utc::now());

        let rejection_count = parse_rejection_count(&contact.notes) + 1;
        contact.notes = Some(format!("{{\"rejections\":{rejection_count}}}"));

        // Consider downgrade.
        if !contact.trust_manual_override
            && rejection_count.is_multiple_of(REJECTION_DOWNGRADE_THRESHOLD)
        {
            contact.trust_level = match contact.trust_level {
                TrustLevel::Known => TrustLevel::Neutral,
                TrustLevel::Neutral => TrustLevel::Unknown,
                other => other,
            };
        }

        self.store.save(&contact).await
    }

    /// Manually set a contact's trust level (user override).
    ///
    /// After a manual override the trust level is pinned and will not be
    /// changed by implicit learning (approvals/rejections).
    pub async fn set_trust_level(
        &self,
        contact_id: ContactId,
        level: TrustLevel,
    ) -> Result<()> {
        let mut contact = self
            .store
            .load(contact_id)
            .await?
            .ok_or_else(|| AthenError::Other(format!("Contact not found: {contact_id}")))?;

        contact.trust_level = level;
        contact.trust_manual_override = true;
        self.store.save(&contact).await
    }

    /// Block a contact. Blocked contacts always receive the highest risk
    /// multiplier regardless of their trust level.
    pub async fn block_contact(&self, contact_id: ContactId) -> Result<()> {
        let mut contact = self
            .store
            .load(contact_id)
            .await?
            .ok_or_else(|| AthenError::Other(format!("Contact not found: {contact_id}")))?;

        contact.blocked = true;
        self.store.save(&contact).await
    }

    /// Check whether a contact is blocked.
    pub fn is_blocked(&self, contact: &Contact) -> bool {
        contact.blocked
    }

    /// Return all contacts from the store.
    pub async fn list_contacts(&self) -> Result<Vec<Contact>> {
        self.store.list_all().await
    }

    /// Find a contact by one of its identifier values.
    pub async fn find_by_identifier(&self, identifier: &str) -> Result<Option<Contact>> {
        self.store.find_by_identifier(identifier).await
    }
}

/// Parse the rejection count from the notes field.
fn parse_rejection_count(notes: &Option<String>) -> u32 {
    notes
        .as_ref()
        .and_then(|n| {
            serde_json::from_str::<serde_json::Value>(n)
                .ok()
                .and_then(|v| v.get("rejections")?.as_u64())
                .map(|v| v as u32)
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemoryContactStore;

    fn make_manager() -> TrustManager {
        TrustManager::new(Box::new(InMemoryContactStore::new()))
    }

    // --- Risk multiplier values ---

    #[test]
    fn test_trust_multiplier_values() {
        assert!((TrustLevel::Unknown.risk_multiplier() - 5.0).abs() < f64::EPSILON);
        assert!((TrustLevel::Neutral.risk_multiplier() - 2.0).abs() < f64::EPSILON);
        assert!((TrustLevel::Known.risk_multiplier() - 1.5).abs() < f64::EPSILON);
        assert!((TrustLevel::Trusted.risk_multiplier() - 1.0).abs() < f64::EPSILON);
        assert!((TrustLevel::AuthUser.risk_multiplier() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_risk_multiplier_blocked_contact() {
        let manager = make_manager();
        let mut contact = Contact {
            id: Uuid::new_v4(),
            name: "blocked".into(),
            trust_level: TrustLevel::Trusted,
            trust_manual_override: false,
            identifiers: vec![],
            interaction_count: 0,
            last_interaction: None,
            notes: None,
            blocked: true,
        };
        // Blocked contacts always get the Unknown (highest) multiplier.
        assert!((manager.risk_multiplier(&contact) - 5.0).abs() < f64::EPSILON);

        contact.blocked = false;
        assert!((manager.risk_multiplier(&contact) - 1.0).abs() < f64::EPSILON);
    }

    // --- Contact resolution ---

    #[tokio::test]
    async fn test_resolve_creates_unknown_contact() {
        let manager = make_manager();
        let contact = manager
            .resolve_contact("new@example.com", IdentifierKind::Email)
            .await
            .unwrap();

        assert_eq!(contact.trust_level, TrustLevel::Unknown);
        assert_eq!(contact.identifiers[0].value, "new@example.com");
        assert!(!contact.blocked);
    }

    #[tokio::test]
    async fn test_resolve_finds_existing_contact() {
        let manager = make_manager();
        let first = manager
            .resolve_contact("alice@test.com", IdentifierKind::Email)
            .await
            .unwrap();
        let second = manager
            .resolve_contact("alice@test.com", IdentifierKind::Email)
            .await
            .unwrap();

        assert_eq!(first.id, second.id);
    }

    // --- Implicit trust evolution: approvals ---

    #[tokio::test]
    async fn test_approval_upgrades_after_threshold() {
        let manager = make_manager();
        let contact = manager
            .resolve_contact("sender@test.com", IdentifierKind::Email)
            .await
            .unwrap();
        let id = contact.id;
        assert_eq!(contact.trust_level, TrustLevel::Unknown);

        // 4 approvals: no upgrade yet.
        for _ in 0..4 {
            manager.record_approval(id).await.unwrap();
        }
        let c = manager.find_by_identifier("sender@test.com").await.unwrap().unwrap();
        assert_eq!(c.trust_level, TrustLevel::Unknown);

        // 5th approval: upgrade T0 -> T1.
        manager.record_approval(id).await.unwrap();
        let c = manager.find_by_identifier("sender@test.com").await.unwrap().unwrap();
        assert_eq!(c.trust_level, TrustLevel::Neutral);

        // 5 more approvals: upgrade T1 -> T2.
        for _ in 0..5 {
            manager.record_approval(id).await.unwrap();
        }
        let c = manager.find_by_identifier("sender@test.com").await.unwrap().unwrap();
        assert_eq!(c.trust_level, TrustLevel::Known);

        // 5 more approvals: should NOT upgrade past T2.
        for _ in 0..5 {
            manager.record_approval(id).await.unwrap();
        }
        let c = manager.find_by_identifier("sender@test.com").await.unwrap().unwrap();
        assert_eq!(c.trust_level, TrustLevel::Known);
    }

    // --- Implicit trust evolution: rejections ---

    #[tokio::test]
    async fn test_rejection_downgrades_after_threshold() {
        let manager = make_manager();
        let contact = manager
            .resolve_contact("bad@test.com", IdentifierKind::Email)
            .await
            .unwrap();
        let id = contact.id;

        // Manually set to Known so we can observe downgrades.
        manager.set_trust_level(id, TrustLevel::Known).await.unwrap();
        // Clear the manual override flag so implicit learning works.
        {
            let mut c = manager.store.load(id).await.unwrap().unwrap();
            c.trust_manual_override = false;
            manager.store.save(&c).await.unwrap();
        }

        // 2 rejections: no downgrade yet.
        for _ in 0..2 {
            manager.record_rejection(id).await.unwrap();
        }
        let c = manager.store.load(id).await.unwrap().unwrap();
        assert_eq!(c.trust_level, TrustLevel::Known);

        // 3rd rejection: downgrade T2 -> T1.
        manager.record_rejection(id).await.unwrap();
        let c = manager.store.load(id).await.unwrap().unwrap();
        assert_eq!(c.trust_level, TrustLevel::Neutral);

        // 3 more rejections: downgrade T1 -> T0.
        for _ in 0..3 {
            manager.record_rejection(id).await.unwrap();
        }
        let c = manager.store.load(id).await.unwrap().unwrap();
        assert_eq!(c.trust_level, TrustLevel::Unknown);
    }

    // --- Manual override ---

    #[tokio::test]
    async fn test_manual_override_sets_level() {
        let manager = make_manager();
        let contact = manager
            .resolve_contact("user@test.com", IdentifierKind::Email)
            .await
            .unwrap();
        let id = contact.id;

        manager
            .set_trust_level(id, TrustLevel::Trusted)
            .await
            .unwrap();

        let c = manager.store.load(id).await.unwrap().unwrap();
        assert_eq!(c.trust_level, TrustLevel::Trusted);
        assert!(c.trust_manual_override);
    }

    #[tokio::test]
    async fn test_manual_override_not_auto_downgraded() {
        let manager = make_manager();
        let contact = manager
            .resolve_contact("vip@test.com", IdentifierKind::Email)
            .await
            .unwrap();
        let id = contact.id;

        manager
            .set_trust_level(id, TrustLevel::Trusted)
            .await
            .unwrap();

        // Many rejections should NOT downgrade a manually set level.
        for _ in 0..10 {
            manager.record_rejection(id).await.unwrap();
        }
        let c = manager.store.load(id).await.unwrap().unwrap();
        assert_eq!(c.trust_level, TrustLevel::Trusted);
    }

    #[tokio::test]
    async fn test_manual_override_not_auto_upgraded() {
        let manager = make_manager();
        let contact = manager
            .resolve_contact("pinned@test.com", IdentifierKind::Email)
            .await
            .unwrap();
        let id = contact.id;

        // Manually pin at T0.
        manager
            .set_trust_level(id, TrustLevel::Unknown)
            .await
            .unwrap();

        // Many approvals should NOT upgrade a manually set level.
        for _ in 0..20 {
            manager.record_approval(id).await.unwrap();
        }
        let c = manager.store.load(id).await.unwrap().unwrap();
        assert_eq!(c.trust_level, TrustLevel::Unknown);
        assert!(c.trust_manual_override);
    }

    // --- Blocking ---

    #[tokio::test]
    async fn test_block_contact() {
        let manager = make_manager();
        let contact = manager
            .resolve_contact("spammer@test.com", IdentifierKind::Email)
            .await
            .unwrap();
        let id = contact.id;

        assert!(!manager.is_blocked(&contact));

        manager.block_contact(id).await.unwrap();

        let c = manager.store.load(id).await.unwrap().unwrap();
        assert!(manager.is_blocked(&c));
        assert!((manager.risk_multiplier(&c) - 5.0).abs() < f64::EPSILON);
    }

    // --- Listing ---

    #[tokio::test]
    async fn test_list_contacts() {
        let manager = make_manager();
        manager
            .resolve_contact("a@test.com", IdentifierKind::Email)
            .await
            .unwrap();
        manager
            .resolve_contact("b@test.com", IdentifierKind::Email)
            .await
            .unwrap();

        let all = manager.list_contacts().await.unwrap();
        assert_eq!(all.len(), 2);
    }

    // --- Find by identifier ---

    #[tokio::test]
    async fn test_find_by_identifier_not_found() {
        let manager = make_manager();
        let result = manager
            .find_by_identifier("nonexistent@test.com")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_find_by_identifier_found() {
        let manager = make_manager();
        let contact = manager
            .resolve_contact("found@test.com", IdentifierKind::Email)
            .await
            .unwrap();

        let found = manager
            .find_by_identifier("found@test.com")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, contact.id);
    }

    // --- Five approvals upgrade logic step by step ---

    #[tokio::test]
    async fn test_five_approvals_upgrade_step_by_step() {
        let manager = make_manager();
        let contact = manager
            .resolve_contact("step@test.com", IdentifierKind::Email)
            .await
            .unwrap();
        let id = contact.id;

        for i in 1..=5 {
            manager.record_approval(id).await.unwrap();
            let c = manager.store.load(id).await.unwrap().unwrap();
            if i < 5 {
                assert_eq!(c.trust_level, TrustLevel::Unknown);
            } else {
                assert_eq!(c.trust_level, TrustLevel::Neutral);
            }
        }
    }

    // --- Three rejections downgrade logic step by step ---

    #[tokio::test]
    async fn test_three_rejections_downgrade_step_by_step() {
        let manager = make_manager();
        let contact = manager
            .resolve_contact("stepdown@test.com", IdentifierKind::Email)
            .await
            .unwrap();
        let id = contact.id;

        // Set to Neutral without manual override.
        {
            let mut c = manager.store.load(id).await.unwrap().unwrap();
            c.trust_level = TrustLevel::Neutral;
            manager.store.save(&c).await.unwrap();
        }

        for i in 1..=3 {
            manager.record_rejection(id).await.unwrap();
            let c = manager.store.load(id).await.unwrap().unwrap();
            if i < 3 {
                assert_eq!(c.trust_level, TrustLevel::Neutral);
            } else {
                assert_eq!(c.trust_level, TrustLevel::Unknown);
            }
        }
    }
}
