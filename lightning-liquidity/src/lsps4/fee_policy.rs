// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Fee policy for the LSPS4 forwarding skim.
//!
//! The LSP skims a forwarding fee from every JIT-channel HTLC. This module carries the policy
//! describing *what* to skim for a given peer and resolves it to a concrete msat amount via the
//! single function [`resolve_skim`].
//!
//! The initial version only ever constructs [`FeePolicy::Flat`], and the only tier the service
//! resolves is [`FeeTier::Standard`], so for any realistically-sized HTLC the skim matches the
//! previous hard-coded 2%. The richer arms exist so later milestones (per-peer policies, zero-fee
//! grants) are purely additive.

use lightning::impl_writeable_tlv_based_enum;

/// The rate at which a peer's forwarded HTLCs are skimmed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeeTier {
	/// Skim at the LSP's configured proportional rate (`forwarding_fee_proportional_millionths`).
	Standard,
	/// Never skim. Used for grant recipients whose funding we do not take a cut of.
	ZeroFee,
	/// Skim at an explicit rate: `base_msat` plus `ppm` proportional millionths.
	Custom {
		/// Proportional rate in millionths applied to the HTLC amount.
		ppm: u64,
		/// Flat fee in millisatoshis added on top of the proportional component.
		base_msat: u64,
	},
}

/// The discount applied to a peer. v1 only constructs [`FeePolicy::Flat`]; richer arms
/// (time-limited, volume-capped, ...) are reserved as future tags so the wire format stays
/// additive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeePolicy {
	/// A flat policy that applies the same [`FeeTier`] to every HTLC.
	Flat(FeeTier),
}

impl_writeable_tlv_based_enum!(FeeTier,
	(0, Standard) => {},
	(2, ZeroFee) => {},
	(4, Custom) => {
		(0, ppm, required),
		(2, base_msat, required),
	},
);

impl_writeable_tlv_based_enum!(FeePolicy,
	{0, Flat} => (),
);

/// Resolve a [`FeePolicy`] to the msat amount to skim from a single HTLC.
///
/// `standard_ppm` is the LSP's configured proportional rate, used only by [`FeeTier::Standard`].
///
/// The skim is waived in exactly one case: when it would consume the entire HTLC. A zero-value
/// forward is rejected by the channel (`channel.rs` force-closes on a 0-msat `update_add_htlc`),
/// so skimming the whole amount would break the forward; that is the only reason we ever waive.
/// The proportional component is computed in 128-bit precision, so a very large HTLC is skimmed
/// correctly rather than (as the previous `u64` arithmetic did) overflowing and forwarding the
/// whole amount for free.
pub(crate) fn resolve_skim(policy: &FeePolicy, htlc_amount_msat: u64, standard_ppm: u64) -> u64 {
	let fee_msat = match policy {
		FeePolicy::Flat(FeeTier::ZeroFee) => 0,
		FeePolicy::Flat(FeeTier::Standard) => proportional_fee_msat(htlc_amount_msat, standard_ppm),
		FeePolicy::Flat(FeeTier::Custom { ppm, base_msat }) => {
			base_msat.saturating_add(proportional_fee_msat(htlc_amount_msat, *ppm))
		},
	};

	if fee_msat >= htlc_amount_msat {
		0
	} else {
		fee_msat
	}
}

/// `amount_msat * ppm / 1_000_000`, rounded up, computed in 128-bit so it can't overflow for any
/// `u64` inputs. Saturates to `u64::MAX`, which the caller reads as "skims the whole HTLC".
fn proportional_fee_msat(amount_msat: u64, ppm: u64) -> u64 {
	// `+ 999_999` before the integer divide is ceiling division: adding `denominator - 1` rounds
	// the result up, so a sub-msat fee skims 1 rather than truncating to 0 (never under-skim).
	let scaled = (amount_msat as u128) * (ppm as u128) + 999_999;
	u64::try_from(scaled / 1_000_000).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::lsps4::utils::compute_forward_fee;
	use lightning::util::ser::{Readable, Writeable};

	/// The legacy inline computation the service used before `resolve_skim` existed, kept here as
	/// the oracle the `Standard` tier must match for any HTLC small enough not to overflow its
	/// `u64` arithmetic.
	fn legacy_standard_skim(amount: u64, ppm: u64) -> u64 {
		match compute_forward_fee(amount, ppm) {
			Some(fee) => {
				let fee = core::cmp::min(fee, amount);
				if amount.saturating_sub(fee) == 0 && fee > 0 {
					0
				} else {
					fee
				}
			},
			None => 0,
		}
	}

	#[test]
	fn standard_matches_legacy_across_sizes() {
		let ppm = 20_000; // 2%
		// All sizes below the u64 overflow threshold (~9.2e14 msat at 2%), where the new 128-bit
		// math and the legacy u64 math agree exactly.
		for amount in [0u64, 1, 999, 1_000, 50_000, 1_000_000, 100_000_000, 1_000_000_000_000] {
			let policy = FeePolicy::Flat(FeeTier::Standard);
			assert_eq!(
				resolve_skim(&policy, amount, ppm),
				legacy_standard_skim(amount, ppm),
				"mismatch at amount={amount}"
			);
		}
	}

	#[test]
	fn large_htlc_skims_instead_of_forwarding_free() {
		// 1e18 msat * 20_000 ppm overflows u64, so the legacy code skimmed nothing and forwarded
		// the whole HTLC for free. The 128-bit math skims the correct 2% instead.
		let amount = 1_000_000_000_000_000_000u64;
		assert_eq!(legacy_standard_skim(amount, 20_000), 0);
		let policy = FeePolicy::Flat(FeeTier::Standard);
		assert_eq!(resolve_skim(&policy, amount, 20_000), 20_000_000_000_000_000);
	}

	#[test]
	fn zero_fee_never_skims() {
		let policy = FeePolicy::Flat(FeeTier::ZeroFee);
		for amount in [0u64, 1, 1_000, u64::MAX] {
			assert_eq!(resolve_skim(&policy, amount, 1_000_000), 0);
		}
	}

	#[test]
	fn custom_adds_base_and_proportional() {
		// 1% proportional plus a flat 100 msat base.
		let policy = FeePolicy::Flat(FeeTier::Custom { ppm: 10_000, base_msat: 100 });
		// 10_000 ppm of 1_000_000 = 10_000, plus 100 base = 10_100.
		assert_eq!(resolve_skim(&policy, 1_000_000, 0), 10_100);
	}

	#[test]
	fn custom_base_only_with_zero_ppm() {
		let policy = FeePolicy::Flat(FeeTier::Custom { ppm: 0, base_msat: 100 });
		assert_eq!(resolve_skim(&policy, 1_000_000, 0), 100);
	}

	#[test]
	fn fee_at_or_above_amount_never_skims_whole_htlc() {
		// fee == amount: 1_000_000 ppm of 1_000 = 1_000 == amount -> 0.
		let policy = FeePolicy::Flat(FeeTier::Standard);
		assert_eq!(resolve_skim(&policy, 1_000, 1_000_000), 0);

		// fee > amount: 2_000_000 ppm of 1_000 = 2_000 > amount -> 0.
		assert_eq!(resolve_skim(&policy, 1_000, 2_000_000), 0);

		// Proportional component saturates to u64::MAX, which is >= the amount -> 0.
		assert_eq!(resolve_skim(&policy, u64::MAX, u64::MAX), 0);

		// A Custom base on its own large enough to swallow the HTLC -> 0.
		let policy = FeePolicy::Flat(FeeTier::Custom { ppm: 0, base_msat: u64::MAX });
		assert_eq!(resolve_skim(&policy, 1_000, 0), 0);
	}

	fn round_trip<T: Readable + Writeable + PartialEq + core::fmt::Debug>(value: &T) {
		let bytes = value.encode();
		let decoded: T = Readable::read(&mut &bytes[..]).unwrap();
		assert_eq!(*value, decoded);
	}

	#[test]
	fn fee_tier_round_trips() {
		round_trip(&FeeTier::Standard);
		round_trip(&FeeTier::ZeroFee);
		round_trip(&FeeTier::Custom { ppm: 12_345, base_msat: 678 });
	}

	#[test]
	fn fee_policy_round_trips() {
		round_trip(&FeePolicy::Flat(FeeTier::Standard));
		round_trip(&FeePolicy::Flat(FeeTier::ZeroFee));
		round_trip(&FeePolicy::Flat(FeeTier::Custom { ppm: 1, base_msat: 2 }));
	}
}
