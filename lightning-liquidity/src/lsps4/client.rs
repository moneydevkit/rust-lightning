// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option. You may not use this file except in accordance with one or both of these
// licenses.

//! Contains the main LSPS4 client object, [`LSPS4ClientHandler`].

use crate::events::{LiquidityEvent, EventQueue};
use crate::lsps0::ser::{LSPSProtocolMessageHandler, LSPSRequestId, LSPSResponseError};
use crate::lsps4::event::LSPS4ClientEvent;
use crate::message_queue::MessageQueue;
use crate::prelude::{new_hash_map, new_hash_set, HashMap, HashSet};
use crate::sync::{Arc, Mutex, RwLock};

use lightning::ln::msgs::{ErrorAction, LightningError};
use lightning::sign::EntropySource;
use lightning::util::errors::APIError;
use lightning::util::logger::Level;
use lightning::util::persist::KVStore;

use bitcoin::secp256k1::PublicKey;

use std::string::String;


use core::default::Default;
use core::ops::Deref;

use crate::lsps4::msgs::{
	RegisterNodeRequest, RegisterNodeResponse, LSPS4Message, LSPS4Request,
	LSPS4Response,
};

/// Client-side configuration options for JIT channels.
#[derive(Clone, Debug, Copy, Default)]
pub struct LSPS4ClientConfig {}

struct PeerState {
	pending_register_node_requests: HashSet<LSPSRequestId>,
}

impl PeerState {
	fn new() -> Self {
		let pending_register_node_requests = new_hash_set();
		Self { pending_register_node_requests }
	}
}

/// The main object allowing to send and receive LSPS4 messages.
pub struct LSPS4ClientHandler<ES: Deref, K: Deref + Clone>
where
	ES::Target: EntropySource,
	K::Target: KVStore,
{
	entropy_source: ES,
	pending_messages: Arc<MessageQueue>,
	pending_events: Arc<EventQueue<K>>,
	per_peer_state: RwLock<HashMap<PublicKey, Mutex<PeerState>>>,
	_config: LSPS4ClientConfig,
}

impl<ES: Deref, K: Deref + Clone> LSPS4ClientHandler<ES, K>
where
	ES::Target: EntropySource,
	K::Target: KVStore,
{
	/// Constructs an `LSPS4ClientHandler`.
	pub(crate) fn new(
		entropy_source: ES, pending_messages: Arc<MessageQueue>, pending_events: Arc<EventQueue<K>>,
		_config: LSPS4ClientConfig,
	) -> Self {
		Self {
			entropy_source,
			pending_messages,
			pending_events,
			per_peer_state: RwLock::new(new_hash_map()),
			_config,
		}
	}

	/// Requests the LSP to register the node.
	///
	/// The user will receive the LSP's response via an [`InvoiceParametersReady`] event.
	///
	/// [`InvoiceParametersReady`]: crate::lsps4::event::LSPS4ClientEvent::InvoiceParametersReady
	pub fn register_node(
		&self, counterparty_node_id: PublicKey
	) -> Result<LSPSRequestId, APIError> {
		let request_id = crate::utils::generate_request_id(&self.entropy_source);

		{
			let mut outer_state_lock = self.per_peer_state.write().unwrap();
			let inner_state_lock = outer_state_lock
				.entry(counterparty_node_id)
				.or_insert(Mutex::new(PeerState::new()));
			let mut peer_state_lock = inner_state_lock.lock().unwrap();

			if !peer_state_lock
				.pending_register_node_requests
				.insert(request_id.clone())
			{
				return Err(APIError::APIMisuseError {
					err: "Failed due to duplicate request_id. This should never happen!"
						.to_string(),
				});
			}
		}

		let request = LSPS4Request::RegisterNode(RegisterNodeRequest {});
		let msg = LSPS4Message::Request(request_id.clone(), request).into();
		let mut message_queue_notifier = self.pending_messages.notifier();
		message_queue_notifier.enqueue(&counterparty_node_id, msg);

		Ok(request_id)
	}


	fn handle_register_node_response(
		&self, request_id: LSPSRequestId, counterparty_node_id: &PublicKey, result: RegisterNodeResponse,
	) -> Result<(), LightningError> {
		let outer_state_lock = self.per_peer_state.read().unwrap();
		match outer_state_lock.get(counterparty_node_id) {
			Some(inner_state_lock) => {
				let mut peer_state = inner_state_lock.lock().unwrap();

				if !peer_state.pending_register_node_requests.remove(&request_id) {
					return Err(LightningError {
						err: format!(
							"Received register_node response for an unknown request: {:?}",
							request_id
						),
						action: ErrorAction::IgnoreAndLog(Level::Info),
					});
				}

				if let Ok(intercept_scid) = result.jit_channel_scid.to_scid() {
					let mut event_queue_notifier = self.pending_events.notifier();
					event_queue_notifier.enqueue(LiquidityEvent::LSPS4Client(
						LSPS4ClientEvent::InvoiceParametersReady {
							request_id,
							counterparty_node_id: *counterparty_node_id,
							intercept_scid,
							cltv_expiry_delta: result.lsp_cltv_expiry_delta,
						},
					));
				} else {
					return Err(LightningError {
						err: format!(
							"Received register_node response with an invalid intercept scid {:?}",
							result.jit_channel_scid
						),
						action: ErrorAction::IgnoreAndLog(Level::Info),
					});
				}
			},
			None => {
				return Err(LightningError {
					err: format!(
						"Received register_node response from unknown peer: {:?}",
						counterparty_node_id
					),
					action: ErrorAction::IgnoreAndLog(Level::Info),
				});
			},
		}
		Ok(())
	}
}

impl<ES: Deref, K: Deref + Clone> LSPSProtocolMessageHandler for LSPS4ClientHandler<ES, K>
where
	ES::Target: EntropySource,
	K::Target: KVStore,
{
	type ProtocolMessage = LSPS4Message;
	const PROTOCOL_NUMBER: Option<u16> = Some(4);

	fn handle_message(
		&self, message: Self::ProtocolMessage, counterparty_node_id: &PublicKey,
	) -> Result<(), LightningError> {
		match message {
			LSPS4Message::Response(request_id, response) => match response {
				LSPS4Response::RegisterNode(result) => {
					self.handle_register_node_response(request_id, counterparty_node_id, result)
				},
			},
			_ => {
				debug_assert!(
					false,
					"Client handler received LSPS4 request message. This should never happen."
				);
				Err(LightningError { err: format!("Client handler received LSPS4 request message from node {:?}. This should never happen.", counterparty_node_id), action: ErrorAction::IgnoreAndLog(Level::Info)})
			},
		}
	}
}

#[cfg(test)]
mod tests {}
