// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! Contains LSPS4 event types

use crate::lsps0::ser::RequestId;
use crate::prelude::{String, Vec};

use bitcoin::secp256k1::PublicKey;

/// An event which an LSPS4 client should take some action in response to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LSPS4ClientEvent {
	/// Provides the necessary information to generate a payable invoice that then may be given to
	/// the payer.
	/// The LSP will ensure that invoices with this intercept scid are always payable by opening
	/// channels as necessary.
	InvoiceParametersReady {
		/// The identifier of the issued LSPS4 `buy` request, as returned by
		/// [`LSPS4ClientHandler::select_opening_params`].
		///
		/// This can be used to track which request this event corresponds to.
		///
		/// [`LSPS4ClientHandler::select_opening_params`]: crate::lsps4::client::LSPS4ClientHandler::select_opening_params
		request_id: RequestId,
		/// The node id of the LSP.
		counterparty_node_id: PublicKey,
		/// The intercept short channel id to use in the route hint.
		intercept_scid: u64,
		/// The `cltv_expiry_delta` to use in the route hint.
		cltv_expiry_delta: u32,
	},
}

/// An event which an LSPS4 server should take some action in response to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LSPS4ServiceEvent {
	/// Send a webhook to the client to notify them they have pending payments.
	SendWebhook {
		/// The node id of the counterparty.
		counterparty_node_id: PublicKey,
	},
	/// You should open a channel using [`ChannelManager::create_channel`].
	///
	/// [`ChannelManager::create_channel`]: lightning::ln::channelmanager::ChannelManager::create_channel
	OpenChannel {
		/// The node to open channel with.
		their_network_key: PublicKey,
		/// The amount we need to forward to the client after opening.
		amt_to_forward_msat: u64,
	},
}
