//! Contacts management commands for the Tauri frontend.
//!
//! Provides Tauri IPC commands for viewing and managing contacts
//! and their trust levels through the UI.

use serde::Serialize;
use tauri::State;

use athen_core::contact::TrustLevel;

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

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

/// Return all contacts for the contacts list view.
#[tauri::command]
pub async fn list_contacts(
    state: State<'_, AppState>,
) -> Result<Vec<ContactInfo>, String> {
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

    let uuid = uuid::Uuid::parse_str(&id)
        .map_err(|e| format!("Invalid contact ID: {e}"))?;

    let contacts = tm
        .list_contacts()
        .await
        .map_err(|e| format!("Failed to load contacts: {e}"))?;

    Ok(contacts.into_iter().find(|c| c.id == uuid).map(|c| ContactInfo {
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

    let uuid = uuid::Uuid::parse_str(&id)
        .map_err(|e| format!("Invalid contact ID: {e}"))?;
    let level = parse_trust_level(&trust_level)?;

    tm.set_trust_level(uuid, level)
        .await
        .map_err(|e| format!("Failed to set trust level: {e}"))?;

    Ok(())
}

/// Block a contact so all their actions receive maximum risk multiplier.
#[tauri::command]
pub async fn block_contact(
    state: State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    let tm = state
        .trust_manager
        .as_ref()
        .ok_or_else(|| "Trust manager not available".to_string())?;

    let uuid = uuid::Uuid::parse_str(&id)
        .map_err(|e| format!("Invalid contact ID: {e}"))?;

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
pub async fn unblock_contact(
    state: State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    use athen_contacts::ContactStore as _;

    let store = state
        .contact_store
        .as_ref()
        .ok_or_else(|| "Contact store not available".to_string())?;

    let uuid = uuid::Uuid::parse_str(&id)
        .map_err(|e| format!("Invalid contact ID: {e}"))?;

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
pub async fn delete_contact(
    state: State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    use athen_contacts::ContactStore as _;

    let store = state
        .contact_store
        .as_ref()
        .ok_or_else(|| "Contact store not available".to_string())?;

    let uuid = uuid::Uuid::parse_str(&id)
        .map_err(|e| format!("Invalid contact ID: {e}"))?;

    store
        .delete(uuid)
        .await
        .map_err(|e| format!("Failed to delete contact: {e}"))?;

    Ok(())
}
