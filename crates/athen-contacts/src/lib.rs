//! Contact and trust management for Athen.
//!
//! Trust levels, risk multiplier calculation, contact resolution.

pub mod trust;

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
}
