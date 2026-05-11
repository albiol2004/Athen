//! Contacts management commands for the Tauri frontend.
//!
//! Provides Tauri IPC commands for viewing and managing contacts
//! and their trust levels through the UI.

use serde::{Deserialize, Serialize};
use tauri::State;

use athen_core::contact::{Contact, ContactIdentifier, IdentifierKind, TrustLevel};

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

/// Contact information serialized for the frontend.
#[derive(Debug, Clone, Serialize)]
pub struct ContactInfo {
    pub id: String,
    pub name: String,
    pub trust_level: String,
    pub trust_manual_override: bool,
    pub identifiers: Vec<IdentifierInfo>,
    pub interaction_count: u32,
    pub last_interaction: Option<String>,
    pub blocked: bool,
}

/// A single contact identifier for frontend display.
#[derive(Debug, Clone, Serialize)]
pub struct IdentifierInfo {
    pub value: String,
    pub kind: String,
}

/// Map a `TrustLevel` enum to a frontend-friendly string.
fn trust_level_str(level: TrustLevel) -> &'static str {
    match level {
        TrustLevel::Unknown => "Unknown",
        TrustLevel::Neutral => "Neutral",
        TrustLevel::Known => "Known",
        TrustLevel::Trusted => "Trusted",
        TrustLevel::AuthUser => "AuthUser",
    }
}

/// Parse a trust level string from the frontend into the enum.
fn parse_trust_level(s: &str) -> Result<TrustLevel, String> {
    match s.to_lowercase().as_str() {
        "unknown" => Ok(TrustLevel::Unknown),
        "neutral" => Ok(TrustLevel::Neutral),
        "known" => Ok(TrustLevel::Known),
        "trusted" => Ok(TrustLevel::Trusted),
        "authuser" => Ok(TrustLevel::AuthUser),
        _ => Err(format!("Invalid trust level: '{}'", s)),
    }
}

/// Input type for contact identifiers from the frontend.
#[derive(Debug, Clone, Deserialize)]
pub struct IdentifierInput {
    pub value: String,
    pub kind: String,
}

/// Parse an identifier kind string into the enum.
///
/// Accepts both the legacy PascalCase form (used by the existing
/// generic contacts CRUD) and the snake_case "scheme" form spoken by
/// the sense layer / `OwnerLookup` (e.g. `"email"`, `"telegram_user"`).
/// The latter is what the owner-contact UI sends so it stays consistent
/// with the disjointness validator.
fn parse_identifier_kind(s: &str) -> IdentifierKind {
    match s {
        // Legacy PascalCase variants
        "Email" | "email" => IdentifierKind::Email,
        "Phone" | "phone" => IdentifierKind::Phone,
        "Telegram" | "telegram" | "telegram_user" => IdentifierKind::Telegram,
        "WhatsApp" | "whatsapp" => IdentifierKind::WhatsApp,
        "IMessage" | "imessage" => IdentifierKind::IMessage,
        "Signal" | "signal" => IdentifierKind::Signal,
        "Discord" | "discord" => IdentifierKind::Discord,
        "Slack" | "slack" => IdentifierKind::Slack,
        "Twitter" | "twitter" => IdentifierKind::Twitter,
        "Username" | "username" => IdentifierKind::Username,
        _ => IdentifierKind::Other,
    }
}

/// Map an `IdentifierKind` to the snake_case scheme string the sense
/// layer + `OwnerLookup` use. Kept in sync with
/// `athen_contacts::owner::identifier_kind_scheme` so the owner-contact
/// UI round-trips correctly through the disjointness validator.
fn identifier_kind_scheme(kind: IdentifierKind) -> &'static str {
    match kind {
        IdentifierKind::Email => "email",
        IdentifierKind::Phone => "phone",
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

/// Extract the bare email address from a value that may be wrapped in
/// "Name <addr@host>" RFC 5322 form. Returns the input unchanged when no
/// angle brackets are present. Lowercases the result.
fn normalize_email_address(raw: &str) -> String {
    let trimmed = raw.trim();
    if let (Some(start), Some(end)) = (trimmed.find('<'), trimmed.rfind('>')) {
        if end > start + 1 {
            return trimmed[start + 1..end].trim().to_ascii_lowercase();
        }
    }
    trimmed.to_ascii_lowercase()
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

/// Return all contacts for the contacts list view.
#[tauri::command]
pub async fn list_contacts(state: State<'_, AppState>) -> Result<Vec<ContactInfo>, String> {
    let tm = state
        .trust_manager
        .as_ref()
        .ok_or_else(|| "Trust manager not available".to_string())?;

    let contacts = tm
        .list_contacts()
        .await
        .map_err(|e| format!("Failed to list contacts: {e}"))?;

    Ok(contacts
        .into_iter()
        .map(|c| ContactInfo {
            id: c.id.to_string(),
            name: c.name.clone(),
            trust_level: trust_level_str(c.trust_level).to_string(),
            trust_manual_override: c.trust_manual_override,
            identifiers: c
                .identifiers
                .iter()
                .map(|i| IdentifierInfo {
                    value: i.value.clone(),
                    kind: format!("{:?}", i.kind),
                })
                .collect(),
            interaction_count: c.interaction_count,
            last_interaction: c.last_interaction.map(|t| t.to_rfc3339()),
            blocked: c.blocked,
        })
        .collect())
}

/// Return a single contact by ID.
#[tauri::command]
pub async fn get_contact(
    state: State<'_, AppState>,
    id: String,
) -> Result<Option<ContactInfo>, String> {
    let tm = state
        .trust_manager
        .as_ref()
        .ok_or_else(|| "Trust manager not available".to_string())?;

    let uuid = uuid::Uuid::parse_str(&id).map_err(|e| format!("Invalid contact ID: {e}"))?;

    let contacts = tm
        .list_contacts()
        .await
        .map_err(|e| format!("Failed to load contacts: {e}"))?;

    Ok(contacts
        .into_iter()
        .find(|c| c.id == uuid)
        .map(|c| ContactInfo {
            id: c.id.to_string(),
            name: c.name.clone(),
            trust_level: trust_level_str(c.trust_level).to_string(),
            trust_manual_override: c.trust_manual_override,
            identifiers: c
                .identifiers
                .iter()
                .map(|i| IdentifierInfo {
                    value: i.value.clone(),
                    kind: format!("{:?}", i.kind),
                })
                .collect(),
            interaction_count: c.interaction_count,
            last_interaction: c.last_interaction.map(|t| t.to_rfc3339()),
            blocked: c.blocked,
        }))
}

/// Set the trust level for a contact (manual override).
#[tauri::command]
pub async fn set_contact_trust(
    state: State<'_, AppState>,
    id: String,
    trust_level: String,
) -> Result<(), String> {
    let tm = state
        .trust_manager
        .as_ref()
        .ok_or_else(|| "Trust manager not available".to_string())?;

    let uuid = uuid::Uuid::parse_str(&id).map_err(|e| format!("Invalid contact ID: {e}"))?;
    let level = parse_trust_level(&trust_level)?;

    tm.set_trust_level(uuid, level)
        .await
        .map_err(|e| format!("Failed to set trust level: {e}"))?;

    Ok(())
}

/// Block a contact so all their actions receive maximum risk multiplier.
#[tauri::command]
pub async fn block_contact(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let tm = state
        .trust_manager
        .as_ref()
        .ok_or_else(|| "Trust manager not available".to_string())?;

    let uuid = uuid::Uuid::parse_str(&id).map_err(|e| format!("Invalid contact ID: {e}"))?;

    tm.block_contact(uuid)
        .await
        .map_err(|e| format!("Failed to block contact: {e}"))?;

    Ok(())
}

/// Unblock a contact by resetting the blocked flag.
///
/// TrustManager only exposes `block_contact` (one-way), so we use the
/// shared contact store directly to load, mutate, and save the contact.
#[tauri::command]
pub async fn unblock_contact(state: State<'_, AppState>, id: String) -> Result<(), String> {
    use athen_contacts::ContactStore as _;

    let store = state
        .contact_store
        .as_ref()
        .ok_or_else(|| "Contact store not available".to_string())?;

    let uuid = uuid::Uuid::parse_str(&id).map_err(|e| format!("Invalid contact ID: {e}"))?;

    let mut contact = store
        .load(uuid)
        .await
        .map_err(|e| format!("Failed to load contact: {e}"))?
        .ok_or_else(|| format!("Contact not found: {id}"))?;

    contact.blocked = false;

    store
        .save(&contact)
        .await
        .map_err(|e| format!("Failed to save contact: {e}"))?;

    Ok(())
}

/// Delete a contact from the store.
#[tauri::command]
pub async fn delete_contact(state: State<'_, AppState>, id: String) -> Result<(), String> {
    use athen_contacts::ContactStore as _;

    let store = state
        .contact_store
        .as_ref()
        .ok_or_else(|| "Contact store not available".to_string())?;

    let uuid = uuid::Uuid::parse_str(&id).map_err(|e| format!("Invalid contact ID: {e}"))?;

    store
        .delete(uuid)
        .await
        .map_err(|e| format!("Failed to delete contact: {e}"))?;

    Ok(())
}

/// Create a new contact with a name and optional identifiers.
#[tauri::command]
pub async fn create_contact(
    state: State<'_, AppState>,
    name: String,
    identifiers: Vec<IdentifierInput>,
) -> Result<ContactInfo, String> {
    use athen_contacts::ContactStore as _;

    let store = state
        .contact_store
        .as_ref()
        .ok_or_else(|| "Contact store not available".to_string())?;

    let id = uuid::Uuid::new_v4();
    let contact = Contact {
        id,
        name: name.clone(),
        trust_level: TrustLevel::Neutral,
        trust_manual_override: false,
        identifiers: identifiers
            .iter()
            .map(|i| ContactIdentifier {
                value: i.value.clone(),
                kind: parse_identifier_kind(&i.kind),
            })
            .collect(),
        interaction_count: 0,
        last_interaction: None,
        notes: None,
        blocked: false,
        is_owner: false,
    };

    store
        .save(&contact)
        .await
        .map_err(|e| format!("Failed to create contact: {e}"))?;

    Ok(ContactInfo {
        id: id.to_string(),
        name,
        trust_level: trust_level_str(TrustLevel::Neutral).to_string(),
        trust_manual_override: false,
        identifiers: identifiers
            .iter()
            .map(|i| IdentifierInfo {
                value: i.value.clone(),
                kind: i.kind.clone(),
            })
            .collect(),
        interaction_count: 0,
        last_interaction: None,
        blocked: false,
    })
}

/// Update an existing contact. Only provided fields are changed.
#[tauri::command]
pub async fn update_contact(
    state: State<'_, AppState>,
    id: String,
    name: Option<String>,
    identifiers: Option<Vec<IdentifierInput>>,
) -> Result<ContactInfo, String> {
    use athen_contacts::ContactStore as _;

    let store = state
        .contact_store
        .as_ref()
        .ok_or_else(|| "Contact store not available".to_string())?;

    let uuid = uuid::Uuid::parse_str(&id).map_err(|e| format!("Invalid contact ID: {e}"))?;

    let mut contact = store
        .load(uuid)
        .await
        .map_err(|e| format!("Failed to load contact: {e}"))?
        .ok_or_else(|| format!("Contact not found: {id}"))?;

    if let Some(new_name) = name {
        contact.name = new_name;
    }

    if let Some(new_identifiers) = identifiers {
        contact.identifiers = new_identifiers
            .iter()
            .map(|i| ContactIdentifier {
                value: i.value.clone(),
                kind: parse_identifier_kind(&i.kind),
            })
            .collect();
    }

    store
        .save(&contact)
        .await
        .map_err(|e| format!("Failed to update contact: {e}"))?;

    Ok(ContactInfo {
        id: contact.id.to_string(),
        name: contact.name.clone(),
        trust_level: trust_level_str(contact.trust_level).to_string(),
        trust_manual_override: contact.trust_manual_override,
        identifiers: contact
            .identifiers
            .iter()
            .map(|i| IdentifierInfo {
                value: i.value.clone(),
                kind: format!("{:?}", i.kind),
            })
            .collect(),
        interaction_count: contact.interaction_count,
        last_interaction: contact.last_interaction.map(|t| t.to_rfc3339()),
        blocked: contact.blocked,
    })
}

// ---------------------------------------------------------------------------
// Owner-contact commands ("My Contact Info")
// ---------------------------------------------------------------------------

/// Trimmed view of the owner contact for the dedicated settings panel.
///
/// Identifier `kind` values are emitted in the snake_case scheme form
/// (`"email"`, `"telegram_user"`, …) so the UI uses the same vocabulary
/// the disjointness validator speaks; round-tripping through
/// `save_owner_contact` is therefore stable.
#[derive(Debug, Clone, Serialize)]
pub struct ContactView {
    pub id: String,
    pub name: String,
    pub identifiers: Vec<IdentifierView>,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IdentifierView {
    pub kind: String,
    pub value: String,
}

fn contact_to_view(c: &Contact) -> ContactView {
    ContactView {
        id: c.id.to_string(),
        name: c.name.clone(),
        identifiers: c
            .identifiers
            .iter()
            .map(|i| IdentifierView {
                kind: identifier_kind_scheme(i.kind).to_string(),
                value: i.value.clone(),
            })
            .collect(),
        // We don't yet track a created_at column on Contact; surface
        // last_interaction as a best-effort substitute (or None on fresh
        // owners). The FE just shows or omits it.
        created_at: c.last_interaction.map(|t| t.to_rfc3339()),
    }
}

/// Return the current owner contact, or `None` when no owner is set.
#[tauri::command]
pub async fn get_owner_contact(state: State<'_, AppState>) -> Result<Option<ContactView>, String> {
    use athen_contacts::ContactStore as _;

    let store = state
        .contact_store
        .as_ref()
        .ok_or_else(|| "Contact store not available".to_string())?;

    let owner = store
        .find_owner()
        .await
        .map_err(|e| format!("Failed to load owner contact: {e}"))?;

    Ok(owner.as_ref().map(contact_to_view))
}

/// Single upsert for the owner contact.
///
/// - Validates that none of the supplied identifiers collide with
///   Athen's own configured identifiers (IMAP username, SMTP
///   `from_address`, Telegram bot id). This is the *inverse* of what
///   the settings-save commands do — same validator, sides swapped.
/// - Replaces the existing owner contact in place when one exists, or
///   creates a fresh one and flags it as owner.
/// - Always sets `trust_level = AuthUser` + `trust_manual_override =
///   true` so the auto-evolution path can't demote the user themselves.
/// - Lowercase-normalises email identifiers as belt-and-braces (the
///   persistence layer already does this on its side).
#[tauri::command]
pub async fn save_owner_contact(
    state: State<'_, AppState>,
    name: String,
    identifiers: Vec<IdentifierInput>,
) -> Result<ContactView, String> {
    use athen_contacts::ContactStore as _;

    let store = state
        .contact_store
        .as_ref()
        .ok_or_else(|| "Contact store not available".to_string())?;

    // Build the validated, normalised identifier list before any
    // persistence work.
    let parsed_idents: Vec<ContactIdentifier> = identifiers
        .iter()
        .filter(|i| !i.value.trim().is_empty())
        .map(|i| {
            let kind = parse_identifier_kind(&i.kind);
            let value = match kind {
                IdentifierKind::Email => normalize_email_address(&i.value),
                _ => i.value.trim().to_string(),
            };
            ContactIdentifier { kind, value }
        })
        .collect();

    // Disjointness check against Athen's *own* identifiers (IMAP
    // username, SMTP from_address, Telegram bot id from the token).
    // Snapshot the current config + vault state and build the "Athen
    // side" identifier set, then verify none of the user-supplied
    // owner identifiers fall inside it.
    let candidates: Vec<(String, String)> = parsed_idents
        .iter()
        .map(|i| (identifier_kind_scheme(i.kind).to_string(), i.value.clone()))
        .collect();

    let athen_idents = collect_athen_side_identifiers(&state).await;
    if !candidates.is_empty() && !athen_idents.is_empty() {
        if let Err(conflicts) =
            athen_contacts::assert_disjoint_from_owner(&athen_idents, &candidates)
        {
            let parts: Vec<String> = conflicts
                .into_iter()
                .map(|(s, v)| format!("{s}={v}"))
                .collect();
            return Err(format!(
                "These identifiers are already used by Athen itself: {} (cannot be both you and Athen's identity)",
                parts.join(", ")
            ));
        }
    }

    // Replace-in-place when an owner already exists; create otherwise.
    let existing = store
        .find_owner()
        .await
        .map_err(|e| format!("Failed to load existing owner: {e}"))?;

    let final_contact = match existing {
        Some(mut c) => {
            c.name = name.trim().to_string();
            c.identifiers = parsed_idents;
            c.trust_level = TrustLevel::AuthUser;
            c.trust_manual_override = true;
            c.is_owner = true;
            store
                .save(&c)
                .await
                .map_err(|e| format!("Failed to save owner contact: {e}"))?;
            // Defensive: ensure no other row carries the flag.
            store
                .set_owner(&c.id)
                .await
                .map_err(|e| format!("Failed to flag owner: {e}"))?;
            c
        }
        None => {
            let id = uuid::Uuid::new_v4();
            let c = Contact {
                id,
                name: name.trim().to_string(),
                trust_level: TrustLevel::AuthUser,
                trust_manual_override: true,
                identifiers: parsed_idents,
                interaction_count: 0,
                last_interaction: None,
                notes: None,
                blocked: false,
                is_owner: true,
            };
            store
                .save(&c)
                .await
                .map_err(|e| format!("Failed to create owner contact: {e}"))?;
            store
                .set_owner(&id)
                .await
                .map_err(|e| format!("Failed to flag owner: {e}"))?;
            c
        }
    };

    Ok(contact_to_view(&final_contact))
}

/// Remove the owner contact entirely.
///
/// Deletes the row outright rather than just clearing `is_owner`:
/// nothing else in the system points at the owner contact by id (sense
/// monitors look it up via `find_owner` each poll), so an outright
/// delete is the cleanest "forget me" semantics and avoids a stale
/// "ex-owner" record cluttering the contacts list.
#[tauri::command]
pub async fn clear_owner_contact(state: State<'_, AppState>) -> Result<(), String> {
    use athen_contacts::ContactStore as _;

    let store = state
        .contact_store
        .as_ref()
        .ok_or_else(|| "Contact store not available".to_string())?;

    let owner = store
        .find_owner()
        .await
        .map_err(|e| format!("Failed to load owner contact: {e}"))?;

    if let Some(c) = owner {
        store
            .delete(c.id)
            .await
            .map_err(|e| format!("Failed to delete owner contact: {e}"))?;
    }
    Ok(())
}

/// Collect Athen's "own" identifiers from the live config + vault.
///
/// Used by `save_owner_contact` to refuse owner identifiers that would
/// collide with Athen's IMAP login, SMTP `from_address`, or bot id —
/// inverse of the check on the settings-save side.
async fn collect_athen_side_identifiers(state: &AppState) -> Vec<(String, String)> {
    let config = crate::settings::load_main_config_public();
    let mut out: Vec<(String, String)> = Vec::new();

    let uname = config.email.username.trim();
    if uname.contains('@') {
        out.push(("email".to_string(), uname.to_ascii_lowercase()));
    }
    let from = config.email.from_address.trim();
    if !from.is_empty() {
        out.push(("email".to_string(), normalize_email_address(from)));
    }
    let smtp_u = config.email.smtp_username.trim();
    if smtp_u.contains('@') {
        out.push(("email".to_string(), smtp_u.to_ascii_lowercase()));
    }

    // Bot id may live in cleartext config OR (vault-routed) the vault.
    let mut token = config.telegram.bot_token.clone();
    if token.is_empty() {
        if let Some(vault) = state.vault.as_ref() {
            if let Ok(Some(t)) = vault
                .get(
                    crate::vault_creds::SCOPE_TELEGRAM,
                    crate::vault_creds::KEY_BOT_TOKEN,
                )
                .await
            {
                token = t;
            }
        }
    }
    if let Some(bot_id) = crate::settings::bot_user_id_from_token(&token) {
        out.push(("telegram_user".to_string(), bot_id));
    }

    // De-dupe: keep insertion order, drop later duplicates.
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

#[cfg(test)]
mod owner_contact_tests {
    use super::*;

    #[test]
    fn parse_identifier_kind_accepts_snake_case_aliases() {
        assert_eq!(parse_identifier_kind("email"), IdentifierKind::Email);
        assert_eq!(
            parse_identifier_kind("telegram_user"),
            IdentifierKind::Telegram
        );
        assert_eq!(parse_identifier_kind("phone"), IdentifierKind::Phone);
        assert_eq!(parse_identifier_kind("Email"), IdentifierKind::Email);
        assert_eq!(parse_identifier_kind("Telegram"), IdentifierKind::Telegram);
        assert_eq!(parse_identifier_kind("nonsense"), IdentifierKind::Other);
    }

    #[test]
    fn identifier_kind_scheme_round_trips_with_parser() {
        let kinds = [
            IdentifierKind::Email,
            IdentifierKind::Phone,
            IdentifierKind::Telegram,
            IdentifierKind::WhatsApp,
            IdentifierKind::IMessage,
            IdentifierKind::Signal,
            IdentifierKind::Discord,
            IdentifierKind::Slack,
            IdentifierKind::Twitter,
            IdentifierKind::Username,
            IdentifierKind::Other,
        ];
        for k in kinds {
            let scheme = identifier_kind_scheme(k);
            assert_eq!(
                parse_identifier_kind(scheme),
                k,
                "scheme '{scheme}' did not round-trip"
            );
        }
    }

    #[test]
    fn normalize_email_address_strips_display_name() {
        assert_eq!(
            normalize_email_address("Alex <alex@example.com>"),
            "alex@example.com"
        );
        assert_eq!(
            normalize_email_address("  Alex  <ALEX@Example.com>  "),
            "alex@example.com"
        );
        assert_eq!(
            normalize_email_address("ALEX@example.com"),
            "alex@example.com"
        );
    }

    // The full upsert paths run against an in-memory contact store and
    // exercise the create / replace / conflict branches without
    // requiring an `AppState`. We test the lower-level shape — same
    // logic that `save_owner_contact` orchestrates — to keep the unit
    // boundary clean.
    use athen_contacts::{ContactStore, InMemoryContactStore};
    use std::sync::Arc;

    async fn save_owner_via_store(
        store: &Arc<dyn ContactStore>,
        name: &str,
        identifiers: Vec<(IdentifierKind, &str)>,
    ) -> Contact {
        let parsed: Vec<ContactIdentifier> = identifiers
            .into_iter()
            .map(|(k, v)| ContactIdentifier {
                kind: k,
                value: match k {
                    IdentifierKind::Email => normalize_email_address(v),
                    _ => v.trim().to_string(),
                },
            })
            .collect();

        let existing = store.find_owner().await.unwrap();
        let contact = match existing {
            Some(mut c) => {
                c.name = name.to_string();
                c.identifiers = parsed;
                c.trust_level = TrustLevel::AuthUser;
                c.trust_manual_override = true;
                c.is_owner = true;
                store.save(&c).await.unwrap();
                store.set_owner(&c.id).await.unwrap();
                c
            }
            None => {
                let id = uuid::Uuid::new_v4();
                let c = Contact {
                    id,
                    name: name.to_string(),
                    trust_level: TrustLevel::AuthUser,
                    trust_manual_override: true,
                    identifiers: parsed,
                    interaction_count: 0,
                    last_interaction: None,
                    notes: None,
                    blocked: false,
                    is_owner: true,
                };
                store.save(&c).await.unwrap();
                store.set_owner(&id).await.unwrap();
                c
            }
        };
        contact
    }

    #[tokio::test]
    async fn save_creates_owner_when_none_exists() {
        let store: Arc<dyn ContactStore> = Arc::new(InMemoryContactStore::new());
        let saved = save_owner_via_store(
            &store,
            "Alex",
            vec![(IdentifierKind::Email, "Alex@example.com")],
        )
        .await;
        assert!(saved.is_owner);
        assert_eq!(saved.trust_level, TrustLevel::AuthUser);
        assert!(saved.trust_manual_override);
        assert_eq!(saved.identifiers.len(), 1);
        assert_eq!(saved.identifiers[0].value, "alex@example.com");
        let owner = store.find_owner().await.unwrap().unwrap();
        assert_eq!(owner.id, saved.id);
    }

    #[tokio::test]
    async fn save_replaces_existing_owner_in_place() {
        let store: Arc<dyn ContactStore> = Arc::new(InMemoryContactStore::new());
        let first = save_owner_via_store(
            &store,
            "Old Name",
            vec![(IdentifierKind::Email, "old@x.com")],
        )
        .await;
        let second = save_owner_via_store(
            &store,
            "New Name",
            vec![
                (IdentifierKind::Email, "new@x.com"),
                (IdentifierKind::Telegram, "12345"),
            ],
        )
        .await;
        // Same row, new contents.
        assert_eq!(first.id, second.id);
        assert_eq!(second.name, "New Name");
        assert_eq!(second.identifiers.len(), 2);
        let all = store.list_all().await.unwrap();
        let owners: Vec<_> = all.iter().filter(|c| c.is_owner).collect();
        assert_eq!(owners.len(), 1);
    }

    #[tokio::test]
    async fn clear_owner_deletes_row() {
        let store: Arc<dyn ContactStore> = Arc::new(InMemoryContactStore::new());
        let saved = save_owner_via_store(
            &store,
            "Alex",
            vec![(IdentifierKind::Email, "alex@example.com")],
        )
        .await;
        // Mirror clear_owner_contact's body.
        let owner = store.find_owner().await.unwrap();
        if let Some(c) = owner {
            store.delete(c.id).await.unwrap();
        }
        assert!(store.find_owner().await.unwrap().is_none());
        assert!(store.load(saved.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn disjointness_rejects_owner_clash_with_athen() {
        // Pretend Athen's IMAP login is "alex@example.com" and the user
        // tries to assign the same address as their owner identifier.
        let athen_side = vec![("email".to_string(), "alex@example.com".to_string())];
        let candidates = vec![("email".to_string(), "alex@example.com".to_string())];
        let err = athen_contacts::assert_disjoint_from_owner(&athen_side, &candidates).unwrap_err();
        assert_eq!(err, vec![("email".into(), "alex@example.com".into())]);
    }

    #[tokio::test]
    async fn disjointness_passes_when_athen_side_empty() {
        // Fresh install: no IMAP / SMTP / bot configured yet → owner
        // identifiers are unconstrained.
        let athen_side: Vec<(String, String)> = vec![];
        let candidates = vec![("email".to_string(), "alex@example.com".to_string())];
        assert!(athen_contacts::assert_disjoint_from_owner(&athen_side, &candidates).is_ok());
    }
}
