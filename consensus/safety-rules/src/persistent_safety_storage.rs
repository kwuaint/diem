// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    counters,
    logging::{self, LogEntry, LogEvent, LogField},
};
use anyhow::Result;
use consensus_types::{common::Author, safety_data::SafetyData};
use libra_crypto::ed25519::{Ed25519PrivateKey, Ed25519PublicKey};
use libra_global_constants::{CONSENSUS_KEY, EXECUTION_KEY, OWNER_ACCOUNT, SAFETY_DATA, WAYPOINT};
use libra_logger::prelude::*;
use libra_secure_storage::{
    CachedStorage, CryptoStorage, InMemoryStorage, KVStorage, Storage, Value,
};
use libra_types::waypoint::Waypoint;
use std::str::FromStr;

/// SafetyRules needs an abstract storage interface to act as a common utility for storing
/// persistent data to local disk, cloud, secrets managers, or even memory (for tests)
/// Any set function is expected to sync to the remote system before returning.
/// @TODO add access to private key from persistent store
/// @TODO add retrieval of private key based upon public key to persistent store
pub struct PersistentSafetyStorage {
    internal_store: Storage,
}

impl PersistentSafetyStorage {
    pub fn in_memory(
        consensus_private_key: Ed25519PrivateKey,
        execution_private_key: Ed25519PrivateKey,
    ) -> Self {
        let storage = Storage::from(InMemoryStorage::new());
        Self::initialize(
            storage,
            Author::random(),
            consensus_private_key,
            execution_private_key,
            Waypoint::default(),
        )
    }

    /// Use this to instantiate a PersistentStorage for a new data store, one that has no
    /// SafetyRules values set.
    pub fn initialize(
        mut internal_store: Storage,
        author: Author,
        consensus_private_key: Ed25519PrivateKey,
        execution_private_key: Ed25519PrivateKey,
        waypoint: Waypoint,
    ) -> Self {
        Self::initialize_(
            &mut internal_store,
            author,
            consensus_private_key,
            execution_private_key,
            waypoint,
        )
        .expect("Unable to initialize backend storage");
        Self { internal_store }
    }

    fn initialize_(
        internal_store: &mut Storage,
        author: Author,
        consensus_private_key: Ed25519PrivateKey,
        execution_private_key: Ed25519PrivateKey,
        waypoint: Waypoint,
    ) -> Result<()> {
        let result = internal_store.import_private_key(CONSENSUS_KEY, consensus_private_key);
        // Attempting to re-initialize existing storage. This can happen in environments like
        // cluster test. Rather than be rigid here, leave it up to the developer to detect
        // inconsistencies or why they did not reset storage between rounds. Do not repeat the
        // checks again below, because it is just too strange to have a partially configured
        // storage.
        if let Err(libra_secure_storage::Error::KeyAlreadyExists(_)) = result {
            warn!("Attempted to re-initialize existing storage");
            return Ok(());
        }

        internal_store.import_private_key(EXECUTION_KEY, execution_private_key)?;
        internal_store.set(
            SAFETY_DATA,
            Value::SafetyData(SafetyData::new(1, 0, 0, None)),
        )?;
        internal_store.set(OWNER_ACCOUNT, Value::String(author.to_string()))?;
        internal_store.set(WAYPOINT, Value::String(waypoint.to_string()))?;
        Ok(())
    }

    pub fn into_cached(self) -> PersistentSafetyStorage {
        // will be an idempotent operation if the underlying storage is already a CachedStorage
        if let Storage::CachedStorage(cached_storage) = self.internal_store {
            PersistentSafetyStorage {
                internal_store: Storage::CachedStorage(cached_storage),
            }
        } else {
            PersistentSafetyStorage {
                internal_store: Storage::CachedStorage(CachedStorage::new(self.internal_store)),
            }
        }
    }

    /// Use this to instantiate a PersistentStorage with an existing data store. This is intended
    /// for constructed environments.
    pub fn new(internal_store: Storage) -> Self {
        Self { internal_store }
    }

    pub fn author(&self) -> Result<Author> {
        let res = self.internal_store.get(OWNER_ACCOUNT)?;
        let res = res.value.string()?;
        std::str::FromStr::from_str(&res)
    }

    pub fn consensus_key_for_version(
        &self,
        version: Ed25519PublicKey,
    ) -> Result<Ed25519PrivateKey> {
        self.internal_store
            .export_private_key_for_version(CONSENSUS_KEY, version)
            .map_err(|e| e.into())
    }

    pub fn execution_public_key(&self) -> Result<Ed25519PublicKey> {
        Ok(self
            .internal_store
            .get_public_key(EXECUTION_KEY)
            .map(|r| r.public_key)?)
    }

    pub fn safety_data(&self) -> Result<SafetyData> {
        Ok(self
            .internal_store
            .get(SAFETY_DATA)
            .and_then(|r| r.value.safety_data())?)
    }

    pub fn set_safety_data(&mut self, data: SafetyData) -> Result<()> {
        counters::set_state("epoch", data.epoch as i64);
        counters::set_state("last_voted_round", data.last_voted_round as i64);
        counters::set_state("preferred_round", data.preferred_round as i64);
        self.internal_store
            .set(SAFETY_DATA, Value::SafetyData(data))?;
        Ok(())
    }

    pub fn waypoint(&self) -> Result<Waypoint> {
        let waypoint = self
            .internal_store
            .get(WAYPOINT)
            .and_then(|r| r.value.string())?;
        Waypoint::from_str(&waypoint)
            .map_err(|e| anyhow::anyhow!("Unable to parse waypoint: {}", e))
    }

    pub fn set_waypoint(&mut self, waypoint: &Waypoint) -> Result<()> {
        self.internal_store
            .set(WAYPOINT, Value::String(waypoint.to_string()))?;
        send_struct_log!(logging::safety_log(LogEntry::Waypoint, LogEvent::Update)
            .data(LogField::Message.as_str(), waypoint));
        Ok(())
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn internal_store(&mut self) -> &mut Storage {
        &mut self.internal_store
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_crypto::Uniform;
    use libra_types::validator_signer::ValidatorSigner;

    #[test]
    fn test() {
        let private_key = ValidatorSigner::from_int(0).private_key().clone();
        let mut storage = PersistentSafetyStorage::in_memory(
            private_key,
            Ed25519PrivateKey::generate_for_testing(),
        );
        let safety_data = storage.safety_data().unwrap();
        assert_eq!(safety_data.epoch, 1);
        assert_eq!(safety_data.last_voted_round, 0);
        assert_eq!(safety_data.preferred_round, 0);
        storage
            .set_safety_data(SafetyData::new(9, 8, 1, None))
            .unwrap();
        let safety_data = storage.safety_data().unwrap();
        assert_eq!(safety_data.epoch, 9);
        assert_eq!(safety_data.last_voted_round, 8);
        assert_eq!(safety_data.preferred_round, 1);
    }
}
