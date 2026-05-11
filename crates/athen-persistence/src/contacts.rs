//! SQLite-backed contact storage for Athen's trust management system.
//!
//! Contacts and their identifiers are stored in two related tables.
//! Implements the `ContactStore` trait from `athen-contacts`.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use rusqlite::{params, Connection};
use tokio::sync::Mutex;
use uuid::Uuid;

use athen_contacts::ContactStore;
use athen_core::contact::{Contact, ContactId, ContactIdentifier, IdentifierKind, TrustLevel};
use athen_core::error::{AthenError, Result};

const CONTACTS_SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS contacts (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    trust_level TEXT NOT NULL DEFAULT 'Unknown',
    trust_manual_override INTEGER NOT NULL DEFAULT 0,
    interaction_count INTEGER NOT NULL DEFAULT 0,
    last_interaction TEXT,
    notes TEXT,
    blocked INTEGER NOT NULL DEFAULT 0,
    is_owner INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS contact_identifiers (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    contact_id TEXT NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
    identifier TEXT NOT NULL,
    kind TEXT NOT NULL,
    UNIQUE(identifier, kind)
);
";

/// Add `is_owner` to legacy DBs created before the owner-identity
/// unification (#???). SQLite has no `ADD COLUMN IF NOT EXISTS`; we
/// probe `PRAGMA table_info` first and only run the ALTER when the
/// column is missing. Idempotent.
fn migrate_add_is_owner(conn: &Connection) -> std::result::Result<(), rusqlite::Error> {
    let mut stmt = conn.prepare("PRAGMA table_info(contacts)")?;
    let has_col = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .any(|name| name == "is_owner");
    drop(stmt);
    if !has_col {
        conn.execute(
            "ALTER TABLE contacts ADD COLUMN is_owner INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    Ok(())
}

/// SQLite-backed contact storage.
#[derive(Clone)]
pub struct SqliteContactStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteContactStore {
    /// Create a new `SqliteContactStore` wrapping the given connection.
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Create the contacts and contact_identifiers tables if they do not exist.
    pub async fn init_schema(&self) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            // Enable foreign key enforcement (required for ON DELETE CASCADE).
            conn.execute_batch("PRAGMA foreign_keys = ON;")
                .map_err(|e| AthenError::Other(format!("Enable foreign keys: {e}")))?;
            conn.execute_batch(CONTACTS_SCHEMA_SQL)
                .map_err(|e| AthenError::Other(format!("Failed to init contacts schema: {e}")))?;
            // Legacy DB compatibility: add `is_owner` to pre-existing
            // contacts tables that lack it. No-op on fresh installs
            // because CONTACTS_SCHEMA_SQL already declares the column.
            migrate_add_is_owner(&conn)
                .map_err(|e| AthenError::Other(format!("Migrate is_owner column: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }
}

fn trust_level_to_str(level: TrustLevel) -> &'static str {
    match level {
        TrustLevel::Unknown => "Unknown",
        TrustLevel::Neutral => "Neutral",
        TrustLevel::Known => "Known",
        TrustLevel::Trusted => "Trusted",
        TrustLevel::AuthUser => "AuthUser",
    }
}

fn trust_level_from_str(s: &str) -> TrustLevel {
    match s {
        "Neutral" => TrustLevel::Neutral,
        "Known" => TrustLevel::Known,
        "Trusted" => TrustLevel::Trusted,
        "AuthUser" => TrustLevel::AuthUser,
        _ => TrustLevel::Unknown,
    }
}

fn identifier_kind_to_str(kind: IdentifierKind) -> &'static str {
    match kind {
        IdentifierKind::Email => "Email",
        IdentifierKind::Phone => "Phone",
        IdentifierKind::Telegram => "Telegram",
        IdentifierKind::WhatsApp => "WhatsApp",
        IdentifierKind::IMessage => "IMessage",
        IdentifierKind::Signal => "Signal",
        IdentifierKind::Discord => "Discord",
        IdentifierKind::Slack => "Slack",
        IdentifierKind::Twitter => "Twitter",
        IdentifierKind::Username => "Username",
        IdentifierKind::Other => "Other",
    }
}

fn identifier_kind_from_str(s: &str) -> IdentifierKind {
    match s {
        "Email" => IdentifierKind::Email,
        "Phone" => IdentifierKind::Phone,
        "Telegram" => IdentifierKind::Telegram,
        "WhatsApp" => IdentifierKind::WhatsApp,
        "IMessage" => IdentifierKind::IMessage,
        "Signal" => IdentifierKind::Signal,
        "Discord" => IdentifierKind::Discord,
        "Slack" => IdentifierKind::Slack,
        "Twitter" => IdentifierKind::Twitter,
        "Username" => IdentifierKind::Username,
        _ => IdentifierKind::Other,
    }
}

/// Load all identifiers for a contact from the database.
fn load_identifiers(
    conn: &Connection,
    contact_id: &str,
) -> std::result::Result<Vec<ContactIdentifier>, rusqlite::Error> {
    let mut stmt =
        conn.prepare("SELECT identifier, kind FROM contact_identifiers WHERE contact_id = ?1")?;
    let rows = stmt.query_map(params![contact_id], |row| {
        let value: String = row.get(0)?;
        let kind_str: String = row.get(1)?;
        Ok(ContactIdentifier {
            value,
            kind: identifier_kind_from_str(&kind_str),
        })
    })?;
    let mut identifiers = Vec::new();
    for row in rows {
        identifiers.push(row?);
    }
    Ok(identifiers)
}

/// Map a rusqlite row to a Contact (without identifiers — those are loaded separately).
fn row_to_contact(row: &rusqlite::Row<'_>) -> rusqlite::Result<Contact> {
    let id_str: String = row.get(0)?;
    let id = Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let trust_str: String = row.get(2)?;
    let override_int: i32 = row.get(3)?;
    let interaction_count: u32 = row.get::<_, i64>(4)? as u32;
    let last_interaction_str: Option<String> = row.get(5)?;
    let blocked_int: i32 = row.get(7)?;
    let is_owner_int: i32 = row.get(8)?;

    let last_interaction = last_interaction_str.and_then(|s| {
        chrono::DateTime::parse_from_rfc3339(&s)
            .ok()
            .map(|dt| dt.with_timezone(&chrono::Utc))
    });

    Ok(Contact {
        id,
        name: row.get(1)?,
        trust_level: trust_level_from_str(&trust_str),
        trust_manual_override: override_int != 0,
        identifiers: Vec::new(), // filled in after query
        interaction_count,
        last_interaction,
        notes: row.get(6)?,
        blocked: blocked_int != 0,
        is_owner: is_owner_int != 0,
    })
}

const SELECT_CONTACTS: &str = "\
    SELECT id, name, trust_level, trust_manual_override, interaction_count, \
           last_interaction, notes, blocked, is_owner, created_at, updated_at \
    FROM contacts";

/// Normalize an identifier value before persisting it. Email
/// identifiers are lowercased so case-only differences don't fork a
/// contact across rows (e.g. `Alex@x.com` vs `alex@x.com`); every other
/// kind is stored verbatim. Mirrors the normalization in
/// `athen_contacts::owner` so cross-channel owner matching works.
fn normalize_identifier_value(kind: IdentifierKind, value: &str) -> String {
    match kind {
        IdentifierKind::Email => value.to_ascii_lowercase(),
        _ => value.to_string(),
    }
}

#[async_trait]
impl ContactStore for SqliteContactStore {
    async fn save(&self, contact: &Contact) -> Result<()> {
        let conn = self.conn.clone();
        let contact = contact.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute_batch("PRAGMA foreign_keys = ON;")
                .map_err(|e| AthenError::Other(format!("Enable foreign keys: {e}")))?;

            let now = Utc::now().to_rfc3339();
            let id_str = contact.id.to_string();
            let last_interaction_str = contact.last_interaction.map(|dt| dt.to_rfc3339());

            let tx = conn
                .unchecked_transaction()
                .map_err(|e| AthenError::Other(format!("Begin transaction: {e}")))?;

            // Upsert contact row.
            tx.execute(
                "INSERT INTO contacts \
                 (id, name, trust_level, trust_manual_override, interaction_count, \
                  last_interaction, notes, blocked, is_owner, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
                 ON CONFLICT(id) DO UPDATE SET \
                  name = excluded.name, \
                  trust_level = excluded.trust_level, \
                  trust_manual_override = excluded.trust_manual_override, \
                  interaction_count = excluded.interaction_count, \
                  last_interaction = excluded.last_interaction, \
                  notes = excluded.notes, \
                  blocked = excluded.blocked, \
                  is_owner = excluded.is_owner, \
                  updated_at = excluded.updated_at",
                params![
                    id_str,
                    contact.name,
                    trust_level_to_str(contact.trust_level),
                    contact.trust_manual_override as i32,
                    contact.interaction_count as i64,
                    last_interaction_str,
                    contact.notes,
                    contact.blocked as i32,
                    contact.is_owner as i32,
                    now,
                    now,
                ],
            )
            .map_err(|e| AthenError::Other(format!("Upsert contact: {e}")))?;

            // Replace identifiers: delete old, insert new. Email values
            // are lowercased on the way in so the unique index can't
            // fork on case alone.
            tx.execute(
                "DELETE FROM contact_identifiers WHERE contact_id = ?1",
                params![id_str],
            )
            .map_err(|e| AthenError::Other(format!("Delete old identifiers: {e}")))?;

            for ident in &contact.identifiers {
                let value = normalize_identifier_value(ident.kind, &ident.value);
                tx.execute(
                    "INSERT INTO contact_identifiers (contact_id, identifier, kind) \
                     VALUES (?1, ?2, ?3)",
                    params![id_str, value, identifier_kind_to_str(ident.kind),],
                )
                .map_err(|e| AthenError::Other(format!("Insert identifier: {e}")))?;
            }

            tx.commit()
                .map_err(|e| AthenError::Other(format!("Commit transaction: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    async fn load(&self, id: ContactId) -> Result<Option<Contact>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let id_str = id.to_string();
            let sql = format!("{SELECT_CONTACTS} WHERE id = ?1");
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AthenError::Other(format!("Prepare load contact: {e}")))?;

            let mut rows = stmt
                .query_map(params![id_str], row_to_contact)
                .map_err(|e| AthenError::Other(format!("Query load contact: {e}")))?;

            match rows.next() {
                Some(Ok(mut contact)) => {
                    contact.identifiers = load_identifiers(&conn, &id_str)
                        .map_err(|e| AthenError::Other(format!("Load identifiers: {e}")))?;
                    Ok(Some(contact))
                }
                Some(Err(e)) => Err(AthenError::Other(format!("Read contact row: {e}"))),
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    async fn find_by_identifier(&self, identifier: &str) -> Result<Option<Contact>> {
        let conn = self.conn.clone();
        let identifier = identifier.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            // Mirror the insert-side normalization: emails contain `@`
            // and are stored lowercased, so the lookup must too. Other
            // schemes pass through unchanged.
            let needle = if identifier.contains('@') {
                identifier.to_ascii_lowercase()
            } else {
                identifier.clone()
            };
            let sql = format!(
                "{SELECT_CONTACTS} WHERE id IN \
                 (SELECT contact_id FROM contact_identifiers WHERE identifier = ?1) \
                 LIMIT 1"
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AthenError::Other(format!("Prepare find by identifier: {e}")))?;

            let mut rows = stmt
                .query_map(params![needle], row_to_contact)
                .map_err(|e| AthenError::Other(format!("Query find by identifier: {e}")))?;

            match rows.next() {
                Some(Ok(mut contact)) => {
                    let id_str = contact.id.to_string();
                    contact.identifiers = load_identifiers(&conn, &id_str)
                        .map_err(|e| AthenError::Other(format!("Load identifiers: {e}")))?;
                    Ok(Some(contact))
                }
                Some(Err(e)) => Err(AthenError::Other(format!("Read contact row: {e}"))),
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    async fn list_all(&self) -> Result<Vec<Contact>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let sql = format!("{SELECT_CONTACTS} ORDER BY name ASC");
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AthenError::Other(format!("Prepare list contacts: {e}")))?;

            let rows = stmt
                .query_map([], row_to_contact)
                .map_err(|e| AthenError::Other(format!("Query list contacts: {e}")))?;

            let mut contacts = Vec::new();
            for row in rows {
                let mut contact =
                    row.map_err(|e| AthenError::Other(format!("Read contact row: {e}")))?;
                let id_str = contact.id.to_string();
                contact.identifiers = load_identifiers(&conn, &id_str)
                    .map_err(|e| AthenError::Other(format!("Load identifiers: {e}")))?;
                contacts.push(contact);
            }
            Ok(contacts)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    async fn find_owner(&self) -> Result<Option<Contact>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let sql = format!("{SELECT_CONTACTS} WHERE is_owner = 1 LIMIT 1");
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AthenError::Other(format!("Prepare find owner: {e}")))?;

            let mut rows = stmt
                .query_map([], row_to_contact)
                .map_err(|e| AthenError::Other(format!("Query find owner: {e}")))?;

            match rows.next() {
                Some(Ok(mut contact)) => {
                    let id_str = contact.id.to_string();
                    contact.identifiers = load_identifiers(&conn, &id_str)
                        .map_err(|e| AthenError::Other(format!("Load identifiers: {e}")))?;
                    Ok(Some(contact))
                }
                Some(Err(e)) => Err(AthenError::Other(format!("Read owner row: {e}"))),
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    async fn set_owner(&self, contact_id: &ContactId) -> Result<()> {
        let conn = self.conn.clone();
        let target = *contact_id;
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let tx = conn
                .unchecked_transaction()
                .map_err(|e| AthenError::Other(format!("Begin set_owner tx: {e}")))?;

            // Clear the flag on every other row, then set it on the
            // target. Two statements keep the invariant atomic even if
            // the target row didn't exist (in which case we still leave
            // the table with zero owners — caller's bug, not ours).
            tx.execute(
                "UPDATE contacts SET is_owner = 0 WHERE id != ?1",
                params![target.to_string()],
            )
            .map_err(|e| AthenError::Other(format!("Clear previous owner: {e}")))?;
            tx.execute(
                "UPDATE contacts SET is_owner = 1 WHERE id = ?1",
                params![target.to_string()],
            )
            .map_err(|e| AthenError::Other(format!("Set owner: {e}")))?;

            tx.commit()
                .map_err(|e| AthenError::Other(format!("Commit set_owner tx: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    async fn delete(&self, id: ContactId) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute_batch("PRAGMA foreign_keys = ON;")
                .map_err(|e| AthenError::Other(format!("Enable foreign keys: {e}")))?;
            conn.execute(
                "DELETE FROM contacts WHERE id = ?1",
                params![id.to_string()],
            )
            .map_err(|e| AthenError::Other(format!("Delete contact: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup() -> SqliteContactStore {
        let conn = Connection::open_in_memory().unwrap();
        let conn = Arc::new(Mutex::new(conn));
        let store = SqliteContactStore::new(conn);
        store.init_schema().await.unwrap();
        store
    }

    fn make_contact(name: &str, identifiers: Vec<(&str, IdentifierKind)>) -> Contact {
        Contact {
            id: Uuid::new_v4(),
            name: name.to_string(),
            trust_level: TrustLevel::Unknown,
            trust_manual_override: false,
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
    async fn test_save_and_load() {
        let store = setup().await;
        let contact = make_contact("Alice", vec![("alice@example.com", IdentifierKind::Email)]);
        let id = contact.id;

        store.save(&contact).await.unwrap();
        let loaded = store.load(id).await.unwrap();
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.name, "Alice");
        assert_eq!(loaded.trust_level, TrustLevel::Unknown);
        assert_eq!(loaded.identifiers.len(), 1);
        assert_eq!(loaded.identifiers[0].value, "alice@example.com");
        assert_eq!(loaded.identifiers[0].kind, IdentifierKind::Email);
    }

    #[tokio::test]
    async fn test_save_with_multiple_identifiers() {
        let store = setup().await;
        let contact = make_contact(
            "Bob",
            vec![
                ("bob@example.com", IdentifierKind::Email),
                ("+1234567890", IdentifierKind::Phone),
                ("bob_dev", IdentifierKind::Username),
            ],
        );
        let id = contact.id;

        store.save(&contact).await.unwrap();
        let loaded = store.load(id).await.unwrap().unwrap();
        assert_eq!(loaded.identifiers.len(), 3);
    }

    #[tokio::test]
    async fn test_find_by_email_identifier() {
        let store = setup().await;
        let contact = make_contact("Carol", vec![("carol@test.org", IdentifierKind::Email)]);
        store.save(&contact).await.unwrap();

        let found = store.find_by_identifier("carol@test.org").await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "Carol");
    }

    #[tokio::test]
    async fn test_find_by_phone_identifier() {
        let store = setup().await;
        let contact = make_contact("Dave", vec![("+9876543210", IdentifierKind::Phone)]);
        store.save(&contact).await.unwrap();

        let found = store.find_by_identifier("+9876543210").await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "Dave");
    }

    #[tokio::test]
    async fn test_update_existing_contact() {
        let store = setup().await;
        let mut contact = make_contact("Eve", vec![("eve@example.com", IdentifierKind::Email)]);
        let id = contact.id;
        store.save(&contact).await.unwrap();

        // Update trust level and interaction count.
        contact.trust_level = TrustLevel::Trusted;
        contact.interaction_count = 10;
        contact.last_interaction = Some(Utc::now());
        contact.notes = Some("VIP contact".to_string());
        store.save(&contact).await.unwrap();

        let loaded = store.load(id).await.unwrap().unwrap();
        assert_eq!(loaded.trust_level, TrustLevel::Trusted);
        assert_eq!(loaded.interaction_count, 10);
        assert!(loaded.last_interaction.is_some());
        assert_eq!(loaded.notes, Some("VIP contact".to_string()));
    }

    #[tokio::test]
    async fn test_list_all_contacts() {
        let store = setup().await;
        let alice = make_contact("Alice", vec![("alice@a.com", IdentifierKind::Email)]);
        let bob = make_contact("Bob", vec![("bob@b.com", IdentifierKind::Email)]);
        let carol = make_contact("Carol", vec![]);

        store.save(&alice).await.unwrap();
        store.save(&bob).await.unwrap();
        store.save(&carol).await.unwrap();

        let all = store.list_all().await.unwrap();
        assert_eq!(all.len(), 3);
        // Ordered by name ASC.
        assert_eq!(all[0].name, "Alice");
        assert_eq!(all[1].name, "Bob");
        assert_eq!(all[2].name, "Carol");
    }

    #[tokio::test]
    async fn test_delete_cascades_identifiers() {
        let store = setup().await;
        let contact = make_contact(
            "Frank",
            vec![
                ("frank@example.com", IdentifierKind::Email),
                ("+1111111111", IdentifierKind::Phone),
            ],
        );
        let id = contact.id;
        store.save(&contact).await.unwrap();

        store.delete(id).await.unwrap();
        let loaded = store.load(id).await.unwrap();
        assert!(loaded.is_none());

        // Identifiers should also be gone.
        let found = store.find_by_identifier("frank@example.com").await.unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn test_find_unknown_identifier_returns_none() {
        let store = setup().await;
        let found = store
            .find_by_identifier("nobody@nowhere.com")
            .await
            .unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn test_save_contact_with_no_identifiers() {
        let store = setup().await;
        let contact = make_contact("NoIdent", vec![]);
        let id = contact.id;
        store.save(&contact).await.unwrap();

        let loaded = store.load(id).await.unwrap().unwrap();
        assert_eq!(loaded.name, "NoIdent");
        assert!(loaded.identifiers.is_empty());
    }

    #[tokio::test]
    async fn test_upsert_overwrites_existing() {
        let store = setup().await;
        let mut contact = make_contact("Grace", vec![("grace@old.com", IdentifierKind::Email)]);
        let id = contact.id;
        store.save(&contact).await.unwrap();

        // Change name and identifiers entirely.
        contact.name = "Grace Updated".to_string();
        contact.identifiers = vec![ContactIdentifier {
            value: "grace@new.com".to_string(),
            kind: IdentifierKind::Email,
        }];
        store.save(&contact).await.unwrap();

        let loaded = store.load(id).await.unwrap().unwrap();
        assert_eq!(loaded.name, "Grace Updated");
        assert_eq!(loaded.identifiers.len(), 1);
        assert_eq!(loaded.identifiers[0].value, "grace@new.com");

        // Old identifier should no longer resolve.
        let old = store.find_by_identifier("grace@old.com").await.unwrap();
        assert!(old.is_none());
    }

    #[tokio::test]
    async fn test_concurrent_access() {
        let store = setup().await;

        let mut handles = Vec::new();
        for i in 0..10 {
            let s = store.clone();
            handles.push(tokio::spawn(async move {
                let contact = Contact {
                    id: Uuid::new_v4(),
                    name: format!("Contact {i}"),
                    trust_level: TrustLevel::Neutral,
                    trust_manual_override: false,
                    identifiers: vec![ContactIdentifier {
                        value: format!("user{i}@test.com"),
                        kind: IdentifierKind::Email,
                    }],
                    interaction_count: 0,
                    last_interaction: None,
                    notes: None,
                    blocked: false,
                    is_owner: false,
                };
                s.save(&contact).await.unwrap();
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        let all = store.list_all().await.unwrap();
        assert_eq!(all.len(), 10);
    }

    #[tokio::test]
    async fn test_blocked_contact_round_trip() {
        let store = setup().await;
        let mut contact = make_contact("Spammer", vec![("spam@bad.com", IdentifierKind::Email)]);
        contact.blocked = true;
        contact.trust_manual_override = true;
        let id = contact.id;
        store.save(&contact).await.unwrap();

        let loaded = store.load(id).await.unwrap().unwrap();
        assert!(loaded.blocked);
        assert!(loaded.trust_manual_override);
    }

    #[tokio::test]
    async fn test_all_trust_levels_round_trip() {
        let store = setup().await;
        let levels = [
            TrustLevel::Unknown,
            TrustLevel::Neutral,
            TrustLevel::Known,
            TrustLevel::Trusted,
            TrustLevel::AuthUser,
        ];
        for level in levels {
            let mut contact = make_contact(&format!("{level:?}"), vec![]);
            contact.trust_level = level;
            let id = contact.id;
            store.save(&contact).await.unwrap();

            let loaded = store.load(id).await.unwrap().unwrap();
            assert_eq!(loaded.trust_level, level);
        }
    }

    #[tokio::test]
    async fn test_load_nonexistent_returns_none() {
        let store = setup().await;
        let loaded = store.load(Uuid::new_v4()).await.unwrap();
        assert!(loaded.is_none());
    }

    // ----- Owner-contact tests (Phase 1 of the unified-owner work) -----

    #[tokio::test]
    async fn find_owner_returns_none_when_unset() {
        let store = setup().await;
        let contact = make_contact("Alice", vec![("a@x.com", IdentifierKind::Email)]);
        store.save(&contact).await.unwrap();
        assert!(store.find_owner().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn set_owner_marks_single_row_and_clears_previous() {
        let store = setup().await;
        let mut a = make_contact("Alice", vec![("a@x.com", IdentifierKind::Email)]);
        let mut b = make_contact("Bob", vec![("b@x.com", IdentifierKind::Email)]);
        a.is_owner = true;
        b.is_owner = false;
        let a_id = a.id;
        let b_id = b.id;
        store.save(&a).await.unwrap();
        store.save(&b).await.unwrap();

        store.set_owner(&a_id).await.unwrap();
        let owner = store.find_owner().await.unwrap().unwrap();
        assert_eq!(owner.id, a_id);
        assert!(owner.is_owner);
        // Identifiers are populated by find_owner.
        assert_eq!(owner.identifiers.len(), 1);
        assert_eq!(owner.identifiers[0].value, "a@x.com");

        // Switching the owner clears the prior owner's flag.
        store.set_owner(&b_id).await.unwrap();
        let owner = store.find_owner().await.unwrap().unwrap();
        assert_eq!(owner.id, b_id);
        let reloaded_a = store.load(a_id).await.unwrap().unwrap();
        assert!(!reloaded_a.is_owner);
    }

    #[tokio::test]
    async fn email_identifiers_normalized_to_lowercase_on_save() {
        let store = setup().await;
        let contact = make_contact("Mixed", vec![("Alex@Example.com", IdentifierKind::Email)]);
        let id = contact.id;
        store.save(&contact).await.unwrap();

        // Lookup by either casing resolves the same contact.
        let lo = store.find_by_identifier("alex@example.com").await.unwrap();
        let mixed = store.find_by_identifier("ALEX@Example.com").await.unwrap();
        assert!(lo.is_some());
        assert!(mixed.is_some());
        assert_eq!(lo.as_ref().unwrap().id, id);
        assert_eq!(mixed.as_ref().unwrap().id, id);
        // Stored value is the lowercased form.
        let loaded = store.load(id).await.unwrap().unwrap();
        assert_eq!(loaded.identifiers[0].value, "alex@example.com");
    }

    #[tokio::test]
    async fn migrate_add_is_owner_is_idempotent_on_legacy_table() {
        // Hand-roll a contacts table without `is_owner` to simulate a
        // DB created before the migration shipped. init_schema should
        // happily add the column; running it twice must remain a no-op.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE contacts (\
                 id TEXT PRIMARY KEY, name TEXT NOT NULL, \
                 trust_level TEXT NOT NULL DEFAULT 'Unknown', \
                 trust_manual_override INTEGER NOT NULL DEFAULT 0, \
                 interaction_count INTEGER NOT NULL DEFAULT 0, \
                 last_interaction TEXT, notes TEXT, \
                 blocked INTEGER NOT NULL DEFAULT 0, \
                 created_at TEXT NOT NULL, updated_at TEXT NOT NULL \
             );\
             CREATE TABLE contact_identifiers (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 contact_id TEXT NOT NULL REFERENCES contacts(id) ON DELETE CASCADE, \
                 identifier TEXT NOT NULL, kind TEXT NOT NULL, \
                 UNIQUE(identifier, kind) \
             );",
        )
        .unwrap();

        let conn = Arc::new(Mutex::new(conn));
        let store = SqliteContactStore::new(conn);
        store.init_schema().await.unwrap();
        // Run again — must not fail.
        store.init_schema().await.unwrap();

        // Round-trip a contact and verify is_owner defaults to false.
        let contact = make_contact("Pre-migration", vec![]);
        let id = contact.id;
        store.save(&contact).await.unwrap();
        let loaded = store.load(id).await.unwrap().unwrap();
        assert!(!loaded.is_owner);
    }
}
