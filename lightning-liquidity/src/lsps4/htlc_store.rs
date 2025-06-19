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
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::lsps4::utils;

/// The Intercepted HTLC store information will be persisted under this key.
pub(crate) const INTERCEPTED_HTLC_STORE_PERSISTENCE_PRIMARY_NAMESPACE: &str = "intercepted_htlcs";
pub(crate) const INTERCEPTED_HTLC_STORE_PERSISTENCE_SECONDARY_NAMESPACE: &str = "";


/// Represents an intercepted HTLC that is stored in the data store
#[derive(Clone, Debug, PartialEq)]
pub struct InterceptedHtlc {
	/// Unique identifier for this intercepted HTLC
	id: InterceptId,
	/// The short channel ID of the next hop that the HTLC was originally intended for
	requested_next_hop_scid: u64,
	/// The amount in millisatoshis that should be forwarded to the next hop
	expected_outbound_amount_msat: u64,
	/// The payment hash of the HTLC
	payment_hash: PaymentHash,
	/// Timestamp (seconds since UNIX_EPOCH) when the HTLC was intercepted
	intercepted_at: u64,
	/// The next node ID that this HTLC should be forwarded to
	next_node_id: PublicKey,
}

impl InterceptedHtlc {
	/// Create a new intercepted HTLC
	pub fn new(
		intercept_id: InterceptId, requested_next_hop_scid: u64,
		expected_outbound_amount_msat: u64, payment_hash: PaymentHash, next_node_id: PublicKey,
	) -> Self {
		let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
		Self {
			id: intercept_id,
			requested_next_hop_scid,
			expected_outbound_amount_msat,
			payment_hash,
			intercepted_at: now,
			next_node_id,
		}
	}

	pub fn id(&self) -> InterceptId {
		self.id
	}

	pub fn requested_next_hop_scid(&self) -> u64 {
		self.requested_next_hop_scid
	}

	pub fn expected_outbound_amount_msat(&self) -> u64 {
		self.expected_outbound_amount_msat
	}

	pub fn payment_hash(&self) -> PaymentHash {
		self.payment_hash
	}

	pub fn intercepted_at(&self) -> u64 {
		self.intercepted_at
	}

	pub fn next_node_id(&self) -> PublicKey {
		self.next_node_id
	}
}

impl_writeable_tlv_based!(InterceptedHtlc, {
	(0, id, required),
	(2, requested_next_hop_scid, required),
	(4, expected_outbound_amount_msat, required),
	(6, payment_hash, required),
	(10, intercepted_at, required),
	(12, next_node_id, required),
});

pub struct HTLCStore<L: Deref, KV: Deref + Clone>
where L::Target: Logger, KV::Target: KVStore {
	htlcs: Mutex<HashMap<InterceptId, InterceptedHtlc>>,
	kv_store: KV,
	logger: L
}

impl<L: Deref, KV: Deref + Clone> HTLCStore<L, KV>
where L::Target: Logger, KV::Target: KVStore {
	pub(crate) fn new(
		kv_store: KV, logger: L,
	) -> Result<Self, io::Error> {
		let mut htlcs = Vec::new();

		for stored_key in kv_store.list(
			INTERCEPTED_HTLC_STORE_PERSISTENCE_PRIMARY_NAMESPACE,
			INTERCEPTED_HTLC_STORE_PERSISTENCE_SECONDARY_NAMESPACE,
		)? {
			let mut reader = Cursor::new(kv_store.read(
				INTERCEPTED_HTLC_STORE_PERSISTENCE_PRIMARY_NAMESPACE,
				INTERCEPTED_HTLC_STORE_PERSISTENCE_SECONDARY_NAMESPACE,
				&stored_key,
			)?);
			let htlc = InterceptedHtlc::read(&mut reader).map_err(|e| {
				log_error!(logger, "Failed to deserialize InterceptedHtlc: {}", e);
				std::io::Error::new(
					std::io::ErrorKind::InvalidData,
					"Failed to deserialize InterceptedHtlc",
				)
			})?;
			htlcs.push(htlc);
		}

		let htlcs =
			Mutex::new(HashMap::from_iter(htlcs.into_iter().map(|obj| (obj.id(), obj))));

		Ok(Self { htlcs, kv_store, logger })
	}

	pub(crate) fn insert(&self, htlc: InterceptedHtlc) -> Result<bool, io::Error> {
		let mut locked_htlcs = self.htlcs.lock().unwrap();

		if locked_htlcs.contains_key(&htlc.id()) {
			return Ok(false);
		}

		self.persist(&htlc)?;
		let updated = locked_htlcs.insert(htlc.id(), htlc).is_some();
		Ok(updated)
	}

	pub(crate) fn remove(&self, id: &InterceptId) -> Result<(), io::Error> {
		let removed = self.htlcs.lock().unwrap().remove(id).is_some();
		if removed {
			let store_key = utils::to_string(&id.0);
			self.kv_store
				.remove(INTERCEPTED_HTLC_STORE_PERSISTENCE_PRIMARY_NAMESPACE, INTERCEPTED_HTLC_STORE_PERSISTENCE_SECONDARY_NAMESPACE, &store_key, false)
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

	pub(crate) fn get(&self, id: &InterceptId) -> Option<InterceptedHtlc> {
		self.htlcs.lock().unwrap().get(id).cloned()
	}

	pub(crate) fn list_filter<F: FnMut(&&InterceptedHtlc) -> bool>(&self, f: F) -> Vec<InterceptedHtlc> {
		self.htlcs.lock().unwrap().values().filter(f).cloned().collect::<Vec<InterceptedHtlc>>()
	}

	/// Get all intercepted HTLCs for a specific node
	pub fn get_htlcs_by_node_id(&self, node_id: &PublicKey) -> Vec<InterceptedHtlc> {
		let mut htlcs = self.list_filter(|htlc| htlc.next_node_id() == *node_id);
		htlcs.sort_by_key(|htlc| htlc.expected_outbound_amount_msat());
		htlcs
	}

	/// Get all intercepted HTLCs that are older than the specified threshold in seconds.
	pub fn get_expired_htlcs(&self, now: u64, threshold_seconds: u64) -> Vec<InterceptedHtlc> {
		self.list_filter(|htlc| {
			// Only include HTLCs that are older than the threshold
			now.saturating_sub(htlc.intercepted_at()) > threshold_seconds
		})
	}

	fn persist(&self, htlc: &InterceptedHtlc) -> Result<(), io::Error> {
		let store_key = utils::to_string(&htlc.id().0);
		let data = htlc.encode();
		self.kv_store
			.write(INTERCEPTED_HTLC_STORE_PERSISTENCE_PRIMARY_NAMESPACE, INTERCEPTED_HTLC_STORE_PERSISTENCE_SECONDARY_NAMESPACE, &store_key, &data)
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

	pub fn add_intercepted_htlc(
		&self, intercept_id: InterceptId, requested_next_hop_scid: u64,
		expected_outbound_amount_msat: u64, payment_hash: PaymentHash, next_node_id: PublicKey,
	) -> Result<bool, io::Error> {
		let htlc = InterceptedHtlc::new(
			intercept_id,
			requested_next_hop_scid,
			expected_outbound_amount_msat,
			payment_hash,
			next_node_id,
		);
		self.insert(htlc)
	}
}