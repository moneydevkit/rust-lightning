// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use lightning::util::ser::{Readable, Writeable};
use lightning::{impl_writeable_tlv_based, log_error};
use lightning::ln::channelmanager::InterceptId;
use lightning::ln::types::ChannelId;

use lightning::util::logger::Logger;
use lightning::util::persist::KVStore;
use lightning_types::payment::PaymentHash;

use bitcoin::secp256k1::PublicKey;

use lightning::io::{self, Cursor};

use std::collections::HashMap;
use std::ops::Deref;
use std::time::{SystemTime, UNIX_EPOCH};
use crate::sync::{Arc, Mutex, MutexGuard, RwLock};


use crate::lsps4::utils;

/// The Intercepted HTLC store information will be persisted under this key.
pub(crate) const INTERCEPT_SCID_STORE_PERSISTENCE_PRIMARY_NAMESPACE: &str = "intercept_scids";
pub(crate) const INTERCEPT_SCID_STORE_PERSISTENCE_SECONDARY_NAMESPACE: &str = "";


/// Represents an intercepted HTLC that is stored in the data store
#[derive(Clone, Debug, PartialEq)]
pub struct ScidWithPeer {
	scid: u64,
	peer_id: PublicKey,
}

impl ScidWithPeer {
	pub fn new(
		scid: u64, peer_id: PublicKey,
	) -> Self {
		Self {
			scid,
			peer_id,
		}
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
}

impl_writeable_tlv_based!(ScidWithPeer, {
	(0, scid, required),
	(2, peer_id, required),
});

pub struct ScidStore<L: Deref, KV: Deref + Clone>
where L::Target: Logger, KV::Target: KVStore {
	peer_by_scid: RwLock<HashMap<u64, PublicKey>>,
	scid_by_peer: RwLock<HashMap<PublicKey, u64>>,
	kv_store: KV,
	logger: L
}

impl<L: Deref, KV: Deref + Clone> ScidStore<L, KV>
where L::Target: Logger, KV::Target: KVStore {
	pub(crate) fn new(
		kv_store: KV, logger: L,
	) -> Result<Self, io::Error> {
		let mut scids = Vec::new();

		for stored_key in kv_store.list(
			INTERCEPT_SCID_STORE_PERSISTENCE_PRIMARY_NAMESPACE,
			INTERCEPT_SCID_STORE_PERSISTENCE_SECONDARY_NAMESPACE,
		)? {
			let mut reader = Cursor::new(kv_store.read(
				INTERCEPT_SCID_STORE_PERSISTENCE_PRIMARY_NAMESPACE,
				INTERCEPT_SCID_STORE_PERSISTENCE_SECONDARY_NAMESPACE,
				&stored_key,
			)?);
			let scid = ScidWithPeer::read(&mut reader).map_err(|e| {
				log_error!(logger, "Failed to deserialize InterceptScid: {}", e);
				std::io::Error::new(
					std::io::ErrorKind::InvalidData,
					"Failed to deserialize InterceptScid",
				)
			})?;
			scids.push(scid);
		}

		let peer_by_scid =
			RwLock::new(HashMap::from_iter(scids.iter().map(|obj| (obj.scid(),obj.peer_id().clone()))));

		let scid_by_peer =
			RwLock::new(HashMap::from_iter(scids.iter().map(|obj| (obj.peer_id().clone(), obj.scid()))));

		Ok(Self { peer_by_scid, scid_by_peer, kv_store, logger })
	}

	pub(crate) fn insert(&self, scid: ScidWithPeer) -> Result<bool, io::Error> {
		let mut locked_peer_by_scid = self.peer_by_scid.write().unwrap();
		let mut locked_scid_by_peer = self.scid_by_peer.write().unwrap();

		self.persist(&scid)?;
		let updated = locked_peer_by_scid.insert(scid.scid(), scid.peer_id().clone()).is_some();
		locked_scid_by_peer.insert(scid.peer_id().clone(), scid.scid());
		Ok(updated)
	}

	pub(crate) fn remove(&self, scid: u64) -> Result<(), io::Error> {
		let mut locked_peer_by_scid = self.peer_by_scid.write().unwrap();
		let mut locked_scid_by_peer = self.scid_by_peer.write().unwrap();

		let removed = locked_peer_by_scid.remove(&scid);
		if let Some(peer_id) = removed {
			locked_scid_by_peer.remove(&peer_id);
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
			.write(INTERCEPT_SCID_STORE_PERSISTENCE_PRIMARY_NAMESPACE, INTERCEPT_SCID_STORE_PERSISTENCE_SECONDARY_NAMESPACE, &store_key, &data)
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
		let scid = ScidWithPeer::new(
			scid,
			peer_id,
		);
		self.insert(scid)
	}

	pub fn get_peer(&self, scid: u64) -> Option<PublicKey> {
		self.peer_by_scid.read().unwrap().get(&scid).cloned()
	}

	pub fn get_scid(&self, peer_id: &PublicKey) -> Option<u64> {
		self.scid_by_peer.read().unwrap().get(peer_id).cloned()
	}
}