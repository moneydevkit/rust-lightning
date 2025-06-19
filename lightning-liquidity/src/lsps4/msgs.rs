//! Message, request, and other primitive types used to implement LSPS4.

use core::convert::TryFrom;

use bitcoin::hashes::hmac::{Hmac, HmacEngine};
use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::hashes::{Hash, HashEngine};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use lightning::util::scid_utils;

use crate::lsps0::ser::{
	string_amount, string_amount_option, LSPSMessage, RequestId, ResponseError,
};
use crate::prelude::{String, Vec};
use crate::utils;

pub(crate) const LSPS4_REGISTER_NODE_METHOD_NAME: &str = "lsps4.register_node";

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
/// A request made to an LSP to register a node.
pub struct RegisterNodeRequest {}

/// A newtype that holds a `short_channel_id` in human readable format of BBBxTTTx000.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct InterceptScid(String);

impl From<u64> for InterceptScid {
	fn from(scid: u64) -> Self {
		let block = scid_utils::block_from_scid(scid);
		let tx_index = scid_utils::tx_index_from_scid(scid);
		let vout = scid_utils::vout_from_scid(scid);

		Self(format!("{}x{}x{}", block, tx_index, vout))
	}
}

impl InterceptScid {
	/// Try to convert a [`InterceptScid`] into a u64 used by LDK.
	pub fn to_scid(&self) -> Result<u64, ()> {
		utils::scid_from_human_readable_string(&self.0)
	}
}

/// A response to a [`RegisterNodeRequest`].
///
/// Includes information needed to construct an invoice.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct RegisterNodeResponse {
	/// The intercept short channel id used by LSP to identify need to open channel.
	pub jit_channel_scid: InterceptScid,
	/// The locktime expiry delta the lsp requires.
	pub lsp_cltv_expiry_delta: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// An enum that captures all the valid JSON-RPC requests in the LSPS4 protocol.
pub enum LSPS4Request {
	/// A request to register a node with an LSP.
	RegisterNode(RegisterNodeRequest),
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// An enum that captures all the valid JSON-RPC responses in the LSPS4 protocol.
pub enum LSPS4Response {
	/// A successful response to a [`LSPS4Request::RegisterNode`] request.
	RegisterNode(RegisterNodeResponse),
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// An enum that captures all valid JSON-RPC messages in the LSPS4 protocol.
pub enum LSPS4Message {
	/// An LSPS4 JSON-RPC request.
	Request(RequestId, LSPS4Request),
	/// An LSPS4 JSON-RPC response.
	Response(RequestId, LSPS4Response),
}

impl TryFrom<LSPSMessage> for LSPS4Message {
	type Error = ();

	fn try_from(message: LSPSMessage) -> Result<Self, Self::Error> {
		if let LSPSMessage::LSPS4(message) = message {
			return Ok(message);
		}

		Err(())
	}
}

impl From<LSPS4Message> for LSPSMessage {
	fn from(message: LSPS4Message) -> Self {
		LSPSMessage::LSPS4(message)
	}
}
