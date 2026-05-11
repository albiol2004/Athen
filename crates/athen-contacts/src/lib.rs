//! Contact and trust management for Athen.
//!
//! Trust levels, risk multiplier calculation, contact resolution.

pub mod owner;
pub mod trust;

pub use owner::{assert_disjoint_from_owner, OwnerLookup};

use async_trait::async_trait;
use athen_core::contact::{Contact, ContactId};
use athen_core::error::Result;

/// Persistence trait for contact storage.
///
/// Implementations provide the backing store for contacts.
/// `InMemoryContactStore` is provided for testing; production
/// code should use a SQLite-backed implementation.
#[async_trait]
pub trait ContactStore: Send + Sync {
    async fn save(&self, contact: &Contact) -> Result<()>;
    async fn load(&self, id: ContactId) -> Result<Option<Contact>>;
    async fn find_by_identifier(&self, identifier: &str) -> Result<Option<Contact>>;
    async fn list_all(&self) -> Result<Vec<Contact>>;
    async fn delete(&self, id: ContactId) -> Result<()>;

    /// Return the single contact marked `is_owner = true`, if any.
    ///
    /// The owner contact represents Athen's user across every channel:
    /// inbound events whose sender identifier matches one of this
    /// contact's attached identifiers are treated as
    /// `TrustLevel::AuthUser`.
    async fn find_owner(&self) -> Result<Option<Contact>>;

    /// Mark `contact_id` as the owner, clearing the flag on every other
    /// contact in the same store. Single-row invariant: at most one
    /// owner exists at any time.
    async fn set_owner(&self, contact_id: &ContactId) -> Result<()>;
}

/// In-memory contact store for testing purposes.
pub struct InMemoryContactStore {
    contacts: tokio::sync::RwLock<Vec<Contact>>,
}

impl InMemoryContactStore {
    pub fn new() -> Self {
        Self {
            contacts: tokio::sync::RwLock::new(Vec::new()),
        }
    }
}

impl Default for InMemoryContactStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ContactStore for InMemoryContactStore {
    async fn save(&self, contact: &Contact) -> Result<()> {
        let mut contacts = self.contacts.write().await;
        if let Some(pos) = contacts.iter().position(|c| c.id == contact.id) {
            contacts[pos] = contact.clone();
        } else {
            contacts.push(contact.clone());
        }
        Ok(())
    }

    async fn load(&self, id: ContactId) -> Result<Option<Contact>> {
        let contacts = self.contacts.read().await;
        Ok(contacts.iter().find(|c| c.id == id).cloned())
    }

    async fn find_by_identifier(&self, identifier: &str) -> Result<Option<Contact>> {
        let contacts = self.contacts.read().await;
        Ok(contacts
            .iter()
            .find(|c| c.identifiers.iter().any(|i| i.value == identifier))
            .cloned())
    }

    async fn list_all(&self) -> Result<Vec<Contact>> {
        let contacts = self.contacts.read().await;
        Ok(contacts.clone())
    }

    async fn delete(&self, id: ContactId) -> Result<()> {
        let mut contacts = self.contacts.write().await;
        contacts.retain(|c| c.id != id);
        Ok(())
    }

    async fn find_owner(&self) -> Result<Option<Contact>> {
        let contacts = self.contacts.read().await;
        Ok(contacts.iter().find(|c| c.is_owner).cloned())
    }

    async fn set_owner(&self, contact_id: &ContactId) -> Result<()> {
        let mut contacts = self.contacts.write().await;
        for c in contacts.iter_mut() {
            c.is_owner = c.id == *contact_id;
        }
        Ok(())
    }
}
