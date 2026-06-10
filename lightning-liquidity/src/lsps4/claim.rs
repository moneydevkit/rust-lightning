// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Signed fee-policy claims presented by a registering node.
//!
//! A node may carry a claim that grants it a non-standard [`FeePolicy`] (for example a zero-fee
//! grant for funding we do not skim). The claim is minted off-band by an issuer the LSP trusts and
//! presented verbatim in `register_node`; the LSP verifies it locally against its configured issuer
//! keys, with no network I/O. With no issuer key configured every claim is rejected, so every peer
//! falls back to `Flat(Standard)` and behaviour is identical to a node that never sees a claim.
//!
//! The wire format is versioned and TLV-encoded rather than a fixed concatenation, so later schemes
//! (more policy arms, an issuer key-id, per-key scope) are additive: a new tag, not a re-issue.
//! The signed bytes are the *opaque* encoded [`ClaimPayload`], carried as a byte string inside
//! [`SignedFeeClaim`]. The verifier hashes exactly those bytes, so it never has to reproduce the
//! payload's TLV layout to check the signature — it only re-reads the payload after the signature
//! has already covered it.

use bitcoin::hashes::{sha256, Hash};
use bitcoin::hex::FromHex;
use bitcoin::secp256k1::{schnorr, Message, PublicKey, Secp256k1, Verification, XOnlyPublicKey};

use lightning::impl_writeable_tlv_based;
use lightning::util::ser::{Readable, Writeable};

use std::vec::Vec;

use crate::lsps4::fee_policy::FeePolicy;

/// The only signature scheme this version understands: a BIP340 Schnorr signature over secp256k1.
const CLAIM_SCHEME_V1: u8 = 1;

/// The signed half of a claim: the issuer commits to *this node* getting *this policy*.
///
/// Encoded and hashed as an opaque byte string by [`SignedFeeClaim`]; the verifier re-reads it only
/// after the signature has been checked over those exact bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ClaimPayload {
	/// Signature scheme tag; only [`CLAIM_SCHEME_V1`] is accepted.
	scheme: u8,
	/// The node the grant is bound to. Must equal the registering peer's id.
	node_id: PublicKey,
	/// The policy the issuer is granting.
	policy: FeePolicy,
}

impl_writeable_tlv_based!(ClaimPayload, {
	(0, scheme, required),
	(2, node_id, required),
	(4, policy, required),
});

/// The wire container: an opaque [`ClaimPayload`] blob plus the issuer's detached signature over it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SignedFeeClaim {
	/// The encoded [`ClaimPayload`]. Kept as bytes so the signature is verified over exactly what
	/// was signed, independent of how this verifier would re-serialize the payload.
	payload: Vec<u8>,
	/// BIP340 Schnorr signature over `SHA256(payload)`.
	sig: schnorr::Signature,
}

impl_writeable_tlv_based!(SignedFeeClaim, {
	(0, payload, required),
	(2, sig, required),
});

/// Why a presented claim was not honoured. Every variant resolves the peer to `Flat(Standard)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ClaimError {
	/// The hex, the outer container, or the inner payload failed to decode.
	Malformed,
	/// The payload's scheme tag is not one this version understands.
	UnknownScheme(u8),
	/// No configured issuer key verified the signature (an empty issuer set always lands here).
	BadSignature,
	/// The claim is valid but bound to a different node than the one presenting it.
	NodeIdMismatch,
}

/// Verify a hex-encoded [`SignedFeeClaim`] and return the granted [`FeePolicy`].
///
/// A claim is accepted when it decodes, carries [`CLAIM_SCHEME_V1`], is signed by *any* of
/// `issuer_pubkeys`, and is bound to `counterparty`. An empty `issuer_pubkeys` rejects every claim
/// ([`ClaimError::BadSignature`]), which is how the feature stays inert until a key is configured.
///
/// `scheme` is read and matched *before* the signature because it selects the verification rules:
/// a future scheme could sign a different digest, so there is no single "verify first" step that
/// works across schemes. Only `scheme == 1` (BIP340 over `SHA256(payload)`) exists today.
///
/// Every configured key is trusted equally — any one of them may grant any [`FeePolicy`]. That is
/// fine for a single issuer. Once a second, less-trusted issuer exists (e.g. a promo key that must
/// not be able to grant a permanent zero-fee), the trust set has to carry per-key scope checked
/// against the granted policy. That gap is deliberate, not forgotten; a keyed `&[(key, scope)]`
/// shape plus a payload key-id are additive when it lands.
pub(crate) fn verify_claim<C: Verification>(
	secp_ctx: &Secp256k1<C>, fee_claim: &str, issuer_pubkeys: &[XOnlyPublicKey],
	counterparty: &PublicKey,
) -> Result<FeePolicy, ClaimError> {
	let bytes = <Vec<u8>>::from_hex(fee_claim).map_err(|_| ClaimError::Malformed)?;
	let signed = SignedFeeClaim::read(&mut &bytes[..]).map_err(|_| ClaimError::Malformed)?;
	let payload =
		ClaimPayload::read(&mut &signed.payload[..]).map_err(|_| ClaimError::Malformed)?;

	if payload.scheme != CLAIM_SCHEME_V1 {
		return Err(ClaimError::UnknownScheme(payload.scheme));
	}

	let digest = Message::from_digest(sha256::Hash::hash(&signed.payload).to_byte_array());
	let verified =
		issuer_pubkeys.iter().any(|pk| secp_ctx.verify_schnorr(&signed.sig, &digest, pk).is_ok());
	if !verified {
		return Err(ClaimError::BadSignature);
	}

	if payload.node_id != *counterparty {
		return Err(ClaimError::NodeIdMismatch);
	}

	Ok(payload.policy)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::lsps4::fee_policy::FeeTier;
	use crate::lsps4::utils;
	use bitcoin::secp256k1::{Keypair, SecretKey, VerifyOnly};

	/// A throwaway verification context for the test call sites. Production threads the service's
	/// long-lived context instead.
	fn verify_ctx() -> Secp256k1<VerifyOnly> {
		Secp256k1::verification_only()
	}

	/// Fixed issuer secret used to mint the in-tree test vector. Test-only; never a real key.
	const ISSUER_SECRET: [u8; 32] = [0x42; 32];
	/// Fixed node secret whose public key the test vector binds the grant to.
	const NODE_SECRET: [u8; 32] = [0x11; 32];

	fn issuer_keypair() -> Keypair {
		let secp = Secp256k1::new();
		Keypair::from_secret_key(&secp, &SecretKey::from_slice(&ISSUER_SECRET).unwrap())
	}

	fn issuer_xonly() -> XOnlyPublicKey {
		issuer_keypair().x_only_public_key().0
	}

	fn node_id() -> PublicKey {
		let secp = Secp256k1::new();
		PublicKey::from_secret_key(&secp, &SecretKey::from_slice(&NODE_SECRET).unwrap())
	}

	/// Mint a claim the way the off-band issuer is expected to: sign `SHA256(payload)` with a
	/// deterministic (no-aux-rand) BIP340 signature so the resulting hex is a stable vector.
	fn mint_claim(node_id: PublicKey, policy: FeePolicy, sk: &[u8; 32]) -> String {
		let secp = Secp256k1::new();
		let keypair = Keypair::from_secret_key(&secp, &SecretKey::from_slice(sk).unwrap());
		let payload = ClaimPayload { scheme: CLAIM_SCHEME_V1, node_id, policy }.encode();
		let digest = Message::from_digest(sha256::Hash::hash(&payload).to_byte_array());
		let sig = secp.sign_schnorr_no_aux_rand(&digest, &keypair);
		utils::to_string(&SignedFeeClaim { payload, sig }.encode())
	}

	#[test]
	fn claim_payload_round_trips() {
		let payload = ClaimPayload {
			scheme: CLAIM_SCHEME_V1,
			node_id: node_id(),
			policy: FeePolicy::Flat(FeeTier::ZeroFee),
		};
		let bytes = payload.encode();
		assert_eq!(payload, ClaimPayload::read(&mut &bytes[..]).unwrap());
	}

	#[test]
	fn valid_claim_yields_granted_policy() {
		let claim = mint_claim(node_id(), FeePolicy::Flat(FeeTier::ZeroFee), &ISSUER_SECRET);
		assert_eq!(
			verify_claim(&verify_ctx(), &claim, &[issuer_xonly()], &node_id()),
			Ok(FeePolicy::Flat(FeeTier::ZeroFee))
		);
	}

	#[test]
	fn empty_issuer_set_rejects() {
		let claim = mint_claim(node_id(), FeePolicy::Flat(FeeTier::ZeroFee), &ISSUER_SECRET);
		assert_eq!(
			verify_claim(&verify_ctx(), &claim, &[], &node_id()),
			Err(ClaimError::BadSignature)
		);
	}

	#[test]
	fn wrong_issuer_key_rejects() {
		let claim = mint_claim(node_id(), FeePolicy::Flat(FeeTier::ZeroFee), &ISSUER_SECRET);
		// An issuer key that did not sign this claim.
		let secp = Secp256k1::new();
		let other = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[0x07; 32]).unwrap())
			.x_only_public_key()
			.0;
		assert_eq!(
			verify_claim(&verify_ctx(), &claim, &[other], &node_id()),
			Err(ClaimError::BadSignature)
		);
	}

	#[test]
	fn forged_signature_rejects() {
		// Mint with the wrong secret, then check against the real issuer's key.
		let claim = mint_claim(node_id(), FeePolicy::Flat(FeeTier::ZeroFee), &[0x07; 32]);
		assert_eq!(
			verify_claim(&verify_ctx(), &claim, &[issuer_xonly()], &node_id()),
			Err(ClaimError::BadSignature)
		);
	}

	#[test]
	fn node_id_mismatch_rejects() {
		let claim = mint_claim(node_id(), FeePolicy::Flat(FeeTier::ZeroFee), &ISSUER_SECRET);
		// A different counterparty than the one the claim is bound to.
		let secp = Secp256k1::new();
		let other =
			PublicKey::from_secret_key(&secp, &SecretKey::from_slice(&[0x22; 32]).unwrap());
		assert_eq!(
			verify_claim(&verify_ctx(), &claim, &[issuer_xonly()], &other),
			Err(ClaimError::NodeIdMismatch)
		);
	}

	#[test]
	fn unknown_scheme_rejects() {
		let secp = Secp256k1::new();
		let keypair = issuer_keypair();
		let payload = ClaimPayload { scheme: 2, node_id: node_id(), policy: FeePolicy::Flat(FeeTier::ZeroFee) }
			.encode();
		let digest = Message::from_digest(sha256::Hash::hash(&payload).to_byte_array());
		let sig = secp.sign_schnorr_no_aux_rand(&digest, &keypair);
		let claim = utils::to_string(&SignedFeeClaim { payload, sig }.encode());
		assert_eq!(
			verify_claim(&verify_ctx(), &claim, &[issuer_xonly()], &node_id()),
			Err(ClaimError::UnknownScheme(2))
		);
	}

	#[test]
	fn malformed_hex_rejects() {
		assert_eq!(
			verify_claim(&verify_ctx(), "zzzz", &[issuer_xonly()], &node_id()),
			Err(ClaimError::Malformed)
		);
	}

	#[test]
	fn malformed_tlv_rejects() {
		// Valid hex, but not a decodable SignedFeeClaim.
		let claim = utils::to_string(&[0xff, 0xff, 0xff]);
		assert_eq!(
			verify_claim(&verify_ctx(), &claim, &[issuer_xonly()], &node_id()),
			Err(ClaimError::Malformed)
		);
	}

	#[test]
	fn multi_issuer_second_key_verifies() {
		// The signing key sits second in the trust set, so this exercises `.any()` past index 0 —
		// the multi-issuer / key-rotation path the config shape is built for.
		let claim = mint_claim(node_id(), FeePolicy::Flat(FeeTier::ZeroFee), &ISSUER_SECRET);
		let secp = Secp256k1::new();
		let other = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[0x07; 32]).unwrap())
			.x_only_public_key()
			.0;
		assert_eq!(
			verify_claim(&verify_ctx(), &claim, &[other, issuer_xonly()], &node_id()),
			Ok(FeePolicy::Flat(FeeTier::ZeroFee))
		);
	}

	#[test]
	fn custom_policy_survives_verify() {
		// Any signed FeePolicy comes back intact, not just ZeroFee.
		let policy = FeePolicy::Flat(FeeTier::Custom { ppm: 1_234, base_msat: 56 });
		let claim = mint_claim(node_id(), policy.clone(), &ISSUER_SECRET);
		assert_eq!(
			verify_claim(&verify_ctx(), &claim, &[issuer_xonly()], &node_id()),
			Ok(policy)
		);
	}

	/// The cross-repo contract the client and the issuer must reproduce byte-for-byte. The issuer
	/// secret, the bound node id, and a `ZeroFee` grant mint exactly this hex; pinning it here
	/// catches any drift in the TLV byte layout or the signing input.
	#[test]
	fn in_tree_test_vector() {
		const EXPECTED_CLAIM_HEX: &str = "73002f002d2c0001010221034f355bdcb7cc0af728ef3cceb9615d90684bb5b2ca5f859ab0f0b704075871aa04040002020002408f3868ca716c39d580d8b54c8d852c22f0f1ea4a174ba13ad571e37fd182aa60e82a88c00225a3f61112804cf1e7c41bd39dbdc7fcb78e779b78423fff47d964";
		const EXPECTED_ISSUER_XONLY: &str =
			"24653eac434488002cc06bbfb7f10fe18991e35f9fe4302dbea6d2353dc0ab1c";

		assert_eq!(utils::to_string(&issuer_xonly().serialize()), EXPECTED_ISSUER_XONLY);

		let claim = mint_claim(node_id(), FeePolicy::Flat(FeeTier::ZeroFee), &ISSUER_SECRET);
		assert_eq!(claim, EXPECTED_CLAIM_HEX);
		assert_eq!(
			verify_claim(&verify_ctx(), &claim, &[issuer_xonly()], &node_id()),
			Ok(FeePolicy::Flat(FeeTier::ZeroFee))
		);
	}
}
