// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use lightning::util::ser::{Readable, Writeable};
use lightning::{impl_writeable_tlv_based, log_error};
use lightning::util::logger::Logger;
use lightning::util::persist::KVStoreSync;

use bitcoin::secp256k1::PublicKey;

use lightning::io::{self, Cursor};

use std::collections::HashMap;
use std::ops::Deref;
use crate::sync::RwLock;


use crate::lsps4::fee_policy::{FeePolicy, FeeTier};
use crate::lsps4::utils;

/// The Intercepted HTLC store information will be persisted under this key.
pub(crate) const INTERCEPT_SCID_STORE_PERSISTENCE_PRIMARY_NAMESPACE: &str = "intercept_scids";
pub(crate) const INTERCEPT_SCID_STORE_PERSISTENCE_SECONDARY_NAMESPACE: &str = "";


/// Represents an intercepted HTLC that is stored in the data store
#[derive(Clone, Debug, PartialEq)]
pub struct ScidWithPeer {
	scid: u64,
	peer_id: PublicKey,
	policy: FeePolicy,
}

impl ScidWithPeer {
	pub fn new(
		scid: u64, peer_id: PublicKey, policy: FeePolicy,
	) -> Self {
		Self { scid, peer_id, policy }
	}

	pub fn store_key(&self) -> String {
		utils::to_string(&self.scid.to_be_bytes())
	}

	pub fn scid(&self) -> u64 {
		self.scid
	}

	pub fn peer_id(&self) -> PublicKey {
		self.peer_id
	}

	pub fn policy(&self) -> &FeePolicy {
		&self.policy
	}
}

impl_writeable_tlv_based!(ScidWithPeer, {
	(0, scid, required),
	(2, peer_id, required),
	(4, policy, (default_value, FeePolicy::Flat(FeeTier::Standard))),
});

pub struct ScidStore<L: Deref, KV: Deref + Clone>
where L::Target: Logger, KV::Target: KVStoreSync {
	peer_by_scid: RwLock<HashMap<u64, PublicKey>>,
	scid_by_peer: RwLock<HashMap<PublicKey, u64>>,
	policy_by_peer: RwLock<HashMap<PublicKey, FeePolicy>>,
	kv_store: KV,
	logger: L
}

impl<L: Deref, KV: Deref + Clone> ScidStore<L, KV>
where L::Target: Logger, KV::Target: KVStoreSync {
	pub(crate) fn new(
		kv_store: KV, logger: L,
	) -> Result<Self, io::Error> {
		let mut scids = Vec::new();

		let stored_keys = kv_store.list(
			INTERCEPT_SCID_STORE_PERSISTENCE_PRIMARY_NAMESPACE,
			INTERCEPT_SCID_STORE_PERSISTENCE_SECONDARY_NAMESPACE,
		)?;

		for stored_key in stored_keys {
			let data = kv_store.read(
				INTERCEPT_SCID_STORE_PERSISTENCE_PRIMARY_NAMESPACE,
				INTERCEPT_SCID_STORE_PERSISTENCE_SECONDARY_NAMESPACE,
				&stored_key,
			)?;
			let mut reader = Cursor::new(data);
			let scid = ScidWithPeer::read(&mut reader).map_err(|e| {
				log_error!(logger, "Failed to deserialize InterceptScid: {}", e);
				io::Error::new(
					io::ErrorKind::InvalidData,
					"Failed to deserialize InterceptScid",
				)
			})?;
			scids.push(scid);
		}

		let peer_by_scid =
			RwLock::new(HashMap::from_iter(scids.iter().map(|obj| (obj.scid(), obj.peer_id()))));

		let scid_by_peer =
			RwLock::new(HashMap::from_iter(scids.iter().map(|obj| (obj.peer_id(), obj.scid()))));

		let policy_by_peer = RwLock::new(HashMap::from_iter(
			scids.iter().map(|obj| (obj.peer_id(), obj.policy().clone())),
		));

		Ok(Self { peer_by_scid, scid_by_peer, policy_by_peer, kv_store, logger })
	}

	pub(crate) fn insert(&self, scid: ScidWithPeer) -> Result<bool, io::Error> {
		use lightning::log_info;
		log_info!(self.logger, "[LSPS4 ScidStore] Inserting SCID {} for peer {}", scid.scid(), scid.peer_id());

		// Persist first
		self.persist(&scid)?;

		// Then insert into the maps
		let mut locked_peer_by_scid = self.peer_by_scid.write().unwrap();
		let mut locked_scid_by_peer = self.scid_by_peer.write().unwrap();
		let mut locked_policy_by_peer = self.policy_by_peer.write().unwrap();
		let updated = locked_peer_by_scid.insert(scid.scid(), scid.peer_id().clone()).is_some();
		locked_scid_by_peer.insert(scid.peer_id().clone(), scid.scid());
		locked_policy_by_peer.insert(scid.peer_id().clone(), scid.policy().clone());

		log_info!(
			self.logger,
			"[LSPS4 ScidStore] Successfully inserted SCID {} for peer {} (was_update: {})",
			scid.scid(),
			scid.peer_id(),
			updated
		);

		Ok(updated)
	}

	pub(crate) fn remove(&self, scid: u64) -> Result<(), io::Error> {
		let mut locked_peer_by_scid = self.peer_by_scid.write().unwrap();
		let mut locked_scid_by_peer = self.scid_by_peer.write().unwrap();
		let mut locked_policy_by_peer = self.policy_by_peer.write().unwrap();

		let removed = locked_peer_by_scid.remove(&scid);
		if let Some(peer_id) = removed {
			locked_scid_by_peer.remove(&peer_id);
			locked_policy_by_peer.remove(&peer_id);
			let store_key = utils::to_string(&scid.to_be_bytes());
			self.kv_store
				.remove(INTERCEPT_SCID_STORE_PERSISTENCE_PRIMARY_NAMESPACE, INTERCEPT_SCID_STORE_PERSISTENCE_SECONDARY_NAMESPACE, &store_key, false)
				.map_err(|e| {
					log_error!(
						self.logger,
						"Removing htlc with intercept id {} failed due to: {}",
						store_key,
						e
					);
					e
				})?;
		}
		Ok(())
	}

	fn persist(&self, scid: &ScidWithPeer) -> Result<(), io::Error> {
		let store_key = scid.store_key();
		let data = scid.encode();
		self.kv_store
			.write(INTERCEPT_SCID_STORE_PERSISTENCE_PRIMARY_NAMESPACE, INTERCEPT_SCID_STORE_PERSISTENCE_SECONDARY_NAMESPACE, &store_key, data)
			.map_err(|e| {
				log_error!(
					self.logger,
					"Write for key {} failed due to: {}",
					store_key,
					e
				);
				e
			})?;
		Ok(())
	}

	pub fn add_intercepted_scid(
		&self, scid: u64, peer_id: PublicKey,
	) -> Result<bool, io::Error> {
		let scid = ScidWithPeer::new(scid, peer_id, FeePolicy::Flat(FeeTier::Standard));
		self.insert(scid)
	}

	pub fn get_peer(&self, scid: u64) -> Option<PublicKey> {
		use lightning::log_debug;
		let result = self.peer_by_scid.read().unwrap().get(&scid).cloned();
		log_debug!(
			self.logger,
			"[LSPS4 ScidStore] get_peer({}) = {:?}",
			scid,
			result
		);
		result
	}

	pub fn get_scid(&self, peer_id: &PublicKey) -> Option<u64> {
		use lightning::log_debug;
		let result = self.scid_by_peer.read().unwrap().get(peer_id).cloned();
		log_debug!(
			self.logger,
			"[LSPS4 ScidStore] get_scid({}) = {:?}",
			peer_id,
			result
		);
		result
	}

	pub fn get_policy(&self, peer_id: &PublicKey) -> Option<FeePolicy> {
		self.policy_by_peer.read().unwrap().get(peer_id).cloned()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use lightning::impl_writeable_tlv_based;

	/// A copy of the pre-policy `ScidWithPeer` layout (tlv 0/2 only) used to prove that records
	/// persisted before the `policy` field existed still decode, defaulting to `Flat(Standard)`.
	struct LegacyScidWithPeer {
		scid: u64,
		peer_id: PublicKey,
	}

	impl_writeable_tlv_based!(LegacyScidWithPeer, {
		(0, scid, required),
		(2, peer_id, required),
	});

	fn test_peer() -> PublicKey {
		// The secp256k1 generator point: a valid compressed public key.
		PublicKey::from_slice(&[
			0x02, 0x79, 0xBE, 0x66, 0x7E, 0xF9, 0xDC, 0xBB, 0xAC, 0x55, 0xA0, 0x62, 0x95, 0xCE,
			0x87, 0x0B, 0x07, 0x02, 0x9B, 0xFC, 0xDB, 0x2D, 0xCE, 0x28, 0xD9, 0x59, 0xF2, 0x81,
			0x5B, 0x16, 0xF8, 0x17, 0x98,
		])
		.unwrap()
	}

	#[test]
	fn round_trips_with_policy() {
		let record = ScidWithPeer::new(42, test_peer(), FeePolicy::Flat(FeeTier::ZeroFee));
		let bytes = record.encode();
		let decoded = ScidWithPeer::read(&mut &bytes[..]).unwrap();
		assert_eq!(record, decoded);
		assert_eq!(decoded.policy(), &FeePolicy::Flat(FeeTier::ZeroFee));
	}

	#[test]
	fn legacy_record_defaults_to_standard_policy() {
		let legacy = LegacyScidWithPeer { scid: 42, peer_id: test_peer() };
		let bytes = legacy.encode();
		let decoded = ScidWithPeer::read(&mut &bytes[..]).unwrap();
		assert_eq!(decoded.scid(), 42);
		assert_eq!(decoded.peer_id(), test_peer());
		assert_eq!(decoded.policy(), &FeePolicy::Flat(FeeTier::Standard));
	}

	use bitcoin::secp256k1::{Secp256k1, SecretKey};
	use lightning::util::test_utils::{TestLogger, TestStore};
	use std::sync::Arc;

	fn other_peer() -> PublicKey {
		PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[0x24; 32]).unwrap())
	}

	fn test_store() -> ScidStore<Arc<TestLogger>, Arc<TestStore>> {
		ScidStore::new(Arc::new(TestStore::new(false)), Arc::new(TestLogger::new())).unwrap()
	}

	#[test]
	fn insert_with_policy_then_get_policy_returns_it() {
		let store = test_store();
		store
			.insert(ScidWithPeer::new(42, test_peer(), FeePolicy::Flat(FeeTier::ZeroFee)))
			.unwrap();

		assert_eq!(store.get_policy(&test_peer()), Some(FeePolicy::Flat(FeeTier::ZeroFee)));
		assert_eq!(store.get_policy(&other_peer()), None);
	}

	#[test]
	fn load_rebuilds_policy_map() {
		let kv_store = Arc::new(TestStore::new(false));
		{
			let store =
				ScidStore::new(kv_store.clone(), Arc::new(TestLogger::new())).unwrap();
			store
				.insert(ScidWithPeer::new(42, test_peer(), FeePolicy::Flat(FeeTier::ZeroFee)))
				.unwrap();
		}

		let reloaded = ScidStore::new(kv_store, Arc::new(TestLogger::new())).unwrap();
		assert_eq!(reloaded.get_policy(&test_peer()), Some(FeePolicy::Flat(FeeTier::ZeroFee)));
	}

	#[test]
	fn default_record_resolves_to_standard_policy() {
		let store = test_store();
		store.add_intercepted_scid(42, test_peer()).unwrap();

		assert_eq!(store.get_policy(&test_peer()), Some(FeePolicy::Flat(FeeTier::Standard)));
	}
}