// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! Contains the main LSPS4 server-side object, [`LSPS4ServiceHandler`].

use crate::events::{Event, EventQueue};
use crate::lsps0::ser::{
	LSPSMessage, ProtocolMessageHandler, RequestId, ResponseError,
	JSONRPC_INTERNAL_ERROR_ERROR_CODE, JSONRPC_INTERNAL_ERROR_ERROR_MESSAGE,
	LSPS0_CLIENT_REJECTED_ERROR_CODE,
};
use crate::lsps4::event::LSPS4ServiceEvent;
use crate::lsps4::htlc_store::{HTLCStore, InterceptedHtlc};
use crate::lsps4::scid_store::ScidStore;
use crate::lsps4::utils::compute_forward_fee;
use crate::message_queue::MessageQueue;
use crate::prelude::hash_map::Entry;
use crate::prelude::{new_hash_map, HashMap, String, ToString, Vec};
use crate::sync::{Arc, Mutex, MutexGuard, RwLock};

use lightning::events::HTLCDestination;
use lightning::ln::channelmanager::{AChannelManager, InterceptId};
use lightning::ln::msgs::{ErrorAction, LightningError};
use lightning::ln::types::ChannelId;
use lightning::{log_debug, log_error, log_info};
use lightning::util::errors::APIError;
use lightning::util::logger::{Level, Logger};

use lightning::util::persist::KVStore;
use lightning_types::payment::PaymentHash;

use bitcoin::secp256k1::PublicKey;

use core::ops::Deref;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::collections::HashSet;

use crate::lsps4::msgs::{
	RegisterNodeRequest, RegisterNodeResponse, LSPS4Message, LSPS4Request,
	LSPS4Response,
};

const HTLC_EXPIRY_THRESHOLD_SECS: u64 = 10;

/// Action to forward a specific HTLC through a channel
#[derive(Debug)]
pub(crate) struct HtlcForwardAction {
	pub htlc: InterceptedHtlc,
	pub channel_id: ChannelId,
}

/// Actions to take for processing HTLCs for a peer
#[derive(Debug)]
pub(crate) struct HtlcProcessingActions {
	pub forwards: Vec<HtlcForwardAction>,
	pub new_channel_needed_msat: Option<u64>,
}

impl HtlcProcessingActions {
	pub fn is_empty(&self) -> bool {
		self.forwards.is_empty() && self.new_channel_needed_msat.is_none()
	}

	pub fn total_forward_amount(&self) -> u64 {
		self.forwards.iter().map(|f| f.htlc.expected_outbound_amount_msat()).sum()
	}
}

/// Server-side configuration options for JIT channels.
#[derive(Clone, Debug)]
pub struct LSPS4ServiceConfig {
	/// The CLTV expiry delta to use for the JIT channels.
	pub cltv_expiry_delta: u32,
}

/// The main object allowing to send and receive LSPS4 messages.
pub struct LSPS4ServiceHandler<CM: Deref + Clone, KV: Deref + Clone, L: Deref + Clone>
where
	CM::Target: AChannelManager,
	KV::Target: KVStore,
	L::Target: Logger,
{
	channel_manager: CM,
	kv_store: KV,
	logger: L,
	pending_messages: Arc<MessageQueue>,
	pending_events: Arc<EventQueue>,
	scid_store: ScidStore<L, KV>,
	htlc_store: HTLCStore<L, KV>,
	connected_peers: RwLock<HashSet<PublicKey>>,
	config: LSPS4ServiceConfig,
}

impl<CM: Deref + Clone, KV: Deref + Clone, L: Deref + Clone> LSPS4ServiceHandler<CM, KV, L>
where
	CM::Target: AChannelManager,
	KV::Target: KVStore,
	L::Target: Logger,
{
	/// Constructs a `LSPS4ServiceHandler`.
	pub(crate) fn new(
		pending_messages: Arc<MessageQueue>, pending_events: Arc<EventQueue>, channel_manager: CM,
		config: LSPS4ServiceConfig, kv_store: KV, logger: L,
	) -> Self {
		Self {
			pending_messages,
			pending_events,
			scid_store: ScidStore::new(kv_store.clone(), logger.clone()).unwrap(),
			htlc_store: HTLCStore::new(kv_store.clone(), logger.clone()).unwrap(),
			channel_manager,
			kv_store,
			config,
			logger,
			connected_peers: RwLock::new(HashSet::new()),
		}
	}

	/// Forward [`Event::HTLCIntercepted`] event parameters into this function.
	///
	/// Will generate a [`LSPS4ServiceEvent::OpenChannel`] event if the intercept scid
	/// matches and the user needs more liquidity.
	///
	/// Will do nothing if the intercept scid does not match any of the ones we gave out.
	///
	/// [`Event::HTLCIntercepted`]: lightning::events::Event::HTLCIntercepted
	/// [`LSPS4ServiceEvent::OpenChannel`]: crate::lsps4::event::LSPS4ServiceEvent::OpenChannel
	pub fn htlc_intercepted(
		&self, intercept_scid: u64, intercept_id: InterceptId, expected_outbound_amount_msat: u64,
		payment_hash: PaymentHash,
	) -> Result<(), APIError> {
		if let Some(counterparty_node_id) = self.scid_store.get_peer(intercept_scid) {
			let htlc = InterceptedHtlc::new(
				intercept_id,
				intercept_scid,
				expected_outbound_amount_msat,
				payment_hash,
				counterparty_node_id.clone(),
			);

			if !self.is_peer_connected(&counterparty_node_id) {
				self.htlc_store.insert(htlc).unwrap(); // TODO: handle persistence failures
				self.pending_events.enqueue(crate::events::Event::LSPS4Service(LSPS4ServiceEvent::SendWebhook {
					counterparty_node_id: counterparty_node_id.clone(),
				}));
			} else {
				let actions = self.calculate_htlc_actions_for_peer(counterparty_node_id,vec![htlc.clone()]);

				if actions.new_channel_needed_msat.is_some() {
					self.htlc_store.insert(htlc).unwrap(); // TODO: handle persistence failures
				}

				self.execute_htlc_actions(actions, counterparty_node_id.clone());
			}
		}

		Ok(())
	}

	/// Forward [`Event::ChannelReady`] event parameters into this function.
	///
	/// Will attempt to forward pending htlcs for this counterparty if there are any.
	///
	/// [`Event::ChannelReady`]: lightning::events::Event::ChannelReady
	pub fn channel_ready(
		&self, counterparty_node_id: &PublicKey,
	) -> Result<(), APIError> {

		if self.is_peer_connected(counterparty_node_id) {
			let htlcs = self.htlc_store.get_htlcs_by_node_id(counterparty_node_id);
			self.process_htlcs_for_peer(counterparty_node_id.clone(), htlcs);
		}

		Ok(())
	}

	/// Will attempt to forward any pending intercepted htlcs to this counterparty.
	pub fn peer_connected(&self, counterparty_node_id: PublicKey) {
		log_info!(self.logger, "Peer connected: {} inserting into connected peers map", counterparty_node_id);

		self.connected_peers.write().unwrap().insert(counterparty_node_id);

		let htlcs = self.htlc_store.get_htlcs_by_node_id(&counterparty_node_id);

		log_info!(self.logger, "{} has {} htlcs waiting to be forwarded", counterparty_node_id, htlcs.len());

		self.process_htlcs_for_peer(counterparty_node_id.clone(), htlcs);
	}

	/// Handle expired HTLCs.
	///
	/// Will fail the HTLCs and remove them from the store.
	/// Needs to be called regularly to cleanup old htlcs.
	pub fn handle_expired_htlcs(&self, now: u64) {
		for htlc in self.htlc_store.get_expired_htlcs(now, HTLC_EXPIRY_THRESHOLD_SECS) {
			if let Err(e) = self.channel_manager.get_cm().fail_intercepted_htlc(htlc.id()) {
				log_error!(
					self.logger,
					"HTLC was already failed when we tried to fail it: {:?}",
					e
				);
			}

			if let Err(e) = self.htlc_store.remove(&htlc.id()) {
				log_error!(
					self.logger,
					"Failed to remove expired intercepted HTLC from store: {}",
					e
				);
			}
		}
	}

	fn is_peer_connected(&self, counterparty_node_id: &PublicKey) -> bool {
		self.connected_peers.read().unwrap().contains(counterparty_node_id)
	}

	/// Will update the set of connected peers
	pub fn peer_disconnected(&self, counterparty_node_id: &PublicKey) {
		self.connected_peers.write().unwrap().remove(counterparty_node_id);
	}

	fn handle_register_node_request(
		&self, request_id: RequestId, counterparty_node_id: &PublicKey, _params: RegisterNodeRequest,
	) -> Result<(), LightningError> {
		let intercept_scid = match self.scid_store.get_scid(counterparty_node_id) {
			Some(intercept_scid) => intercept_scid,
			None => {
				let intercept_scid = self.channel_manager.get_cm().get_intercept_scid();
				self.scid_store.add_intercepted_scid(intercept_scid, counterparty_node_id.clone()).unwrap();
				intercept_scid
			}
		};
		self.pending_messages.enqueue(counterparty_node_id, LSPS4Message::Response(request_id, LSPS4Response::RegisterNode(RegisterNodeResponse {
			jit_channel_scid: intercept_scid.into(),
			lsp_cltv_expiry_delta: self.config.cltv_expiry_delta,
		})).into());
		Ok(())
	}

	/// Calculate what actions to take for a list of HTLCs for a specific peer
	/// This is a pure function that doesn't perform any side effects
	pub(crate) fn calculate_htlc_actions_for_peer(
		&self, their_node_id: PublicKey, mut htlcs: Vec<InterceptedHtlc>,
	) -> HtlcProcessingActions {
		let channels = self.channel_manager.get_cm().list_channels_with_counterparty(&their_node_id);

		let mut channel_capacity_map: HashMap<ChannelId, u64> = new_hash_map();
		for channel in &channels {
			channel_capacity_map.insert(channel.channel_id, channel.outbound_capacity_msat);
		}

		log_info!(self.logger, "{} has {} channels with these outbound capacities: {:?}", their_node_id, channels.len(), channel_capacity_map);

		let mut forwards = Vec::new();

		while let Some(htlc) = htlcs.pop() {
			let required_amount = htlc.expected_outbound_amount_msat();
			let mut can_forward = false;

			// Check if any existing channel has sufficient remaining capacity
			for (channel_id, remaining_capacity) in channel_capacity_map.iter_mut() {
				if *remaining_capacity >= required_amount {
					// Plan to forward this HTLC through this channel
					forwards.push(HtlcForwardAction { htlc, channel_id: *channel_id });

					// Update remaining capacity after planning the forward
					*remaining_capacity -= required_amount;
					can_forward = true;
					break;
				}
			}

			if !can_forward {
				// No existing channel has sufficient capacity, need to open a new channel
				// Calculate total amount needed for remaining HTLCs (including current one)
				let mut total_remaining_amount = required_amount;
				for remaining_htlc in &htlcs {
					total_remaining_amount += remaining_htlc.expected_outbound_amount_msat();
				}

				return HtlcProcessingActions {
					forwards,
					new_channel_needed_msat: Some(total_remaining_amount),
				};
			}
		}

		HtlcProcessingActions { forwards, new_channel_needed_msat: None }
	}

	/// Execute the actions calculated by calculate_htlc_actions_for_peer
	pub(crate) fn execute_htlc_actions(
		&self, actions: HtlcProcessingActions, their_node_id: PublicKey,
	) {
		// Execute forwards
		for forward_action in actions.forwards {
			log_debug!(
				self.logger,
				"Executing forward for HTLC {:?} through channel {} with amount {}msat",
				forward_action.htlc.id(),
				forward_action.channel_id,
				forward_action.htlc.expected_outbound_amount_msat()
			);

			if let Err(e) = self.channel_manager.get_cm().forward_intercepted_htlc(
				forward_action.htlc.id(),
				&forward_action.channel_id,
				forward_action.htlc.next_node_id(),
				forward_action.htlc.expected_outbound_amount_msat(),
			) {
				log_error!(self.logger, "Failed to forward intercepted HTLC: {:?}", e);
			}

			// Remove the HTLC from store after forwarding
			if let Err(e) = self.htlc_store.remove(&forward_action.htlc.id()) {
				log_error!(self.logger, "Failed to remove intercepted HTLC from store: {}", e);
			}
		}

		// Handle new channel opening
		if let Some(channel_size_msat) = actions.new_channel_needed_msat {
			log_info!(
				self.logger,
				"Need a new channel with peer {} for {}msat to forward HTLCs",
				their_node_id,
				channel_size_msat
			);

			self.pending_events.enqueue(crate::events::Event::LSPS4Service(LSPS4ServiceEvent::OpenChannel {
				their_network_key: their_node_id,
				amt_to_forward_msat: channel_size_msat,
			}));
		}
	}

	/// Convenience function that calculates and executes HTLC actions in one call
	pub(crate) fn process_htlcs_for_peer(
		&self, their_node_id: PublicKey, htlcs: Vec<InterceptedHtlc>,
	) {
		let actions = self.calculate_htlc_actions_for_peer(their_node_id, htlcs);

		log_info!(self.logger, "Calculated actions for peer {}: {:?}", their_node_id, actions);

		if actions.forwards.is_empty() && actions.new_channel_needed_msat.is_none() {
			log_debug!(self.logger, "No HTLCs to process for peer {}", their_node_id);
			return;
		}

		if !actions.forwards.is_empty() {
			log_debug!(
				self.logger,
				"Processing {} HTLCs for peer {}",
				actions.forwards.len(),
				their_node_id
			);
		}

		self.execute_htlc_actions(actions, their_node_id);
	}

}

impl<CM: Deref + Clone, KV: Deref + Clone, L: Deref + Clone> ProtocolMessageHandler for LSPS4ServiceHandler<CM, KV, L>
where
	CM::Target: AChannelManager,
	KV::Target: KVStore,
	L::Target: Logger,
{
	type ProtocolMessage = LSPS4Message;
	const PROTOCOL_NUMBER: Option<u16> = Some(4);

	fn handle_message(
		&self, message: Self::ProtocolMessage, counterparty_node_id: &PublicKey,
	) -> Result<(), LightningError> {
		match message {
			LSPS4Message::Request(request_id, request) => {
				let res = match request {
					LSPS4Request::RegisterNode(params) => {
						self.handle_register_node_request(request_id, counterparty_node_id, params)
					},
				};
				res
			},
			_ => {
				debug_assert!(
					false,
					"Service handler received LSPS4 response message. This should never happen."
				);
				Err(LightningError { err: format!("Service handler received LSPS4 response message from node {:?}. This should never happen.", counterparty_node_id), action: ErrorAction::IgnoreAndLog(Level::Info)})
			},
		}
	}
}