//! Message, request, and other primitive types used to implement LSPS4.

use core::convert::TryFrom;

use bitcoin::hashes::hmac::{Hmac, HmacEngine};
use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::hashes::{Hash, HashEngine};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use lightning::util::scid_utils;

use crate::lsps0::ser::{
	string_amount, string_amount_option, LSPSMessage, LSPSRequestId, LSPSResponseError,
};

use std::string::String;
use std::vec::Vec;
use crate::utils;

pub(crate) const LSPS4_REGISTER_NODE_METHOD_NAME: &str = "lsps4.register_node";

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
/// A request made to an LSP to register a node.
pub struct RegisterNodeRequest {
	/// An optional signed claim, lowercase-hex encoded, granting this node a
	/// non-standard fee policy. Absent or unverifiable claims leave the node on
	/// the standard policy.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub fee_claim: Option<String>,
}

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
	Request(LSPSRequestId, LSPS4Request),
	/// An LSPS4 JSON-RPC response.
	Response(LSPSRequestId, LSPS4Response),
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

#[cfg(test)]
mod tests {
	use super::*;
	use crate::alloc::string::ToString;

	#[test]
	fn register_node_request_with_claim_round_trips() {
		let request = RegisterNodeRequest { fee_claim: Some("deadbeef".to_string()) };
		let json_str = r#"{"fee_claim":"deadbeef"}"#;

		assert_eq!(json_str, serde_json::json!(request).to_string());
		assert_eq!(request, serde_json::from_str(json_str).unwrap());
	}

	#[test]
	fn register_node_request_without_claim_omits_field() {
		let request = RegisterNodeRequest { fee_claim: None };

		assert_eq!("{}", serde_json::json!(request).to_string());
	}

	#[test]
	fn legacy_empty_object_decodes_to_no_claim() {
		let request: RegisterNodeRequest = serde_json::from_str("{}").unwrap();

		assert_eq!(request, RegisterNodeRequest { fee_claim: None });
	}
}
