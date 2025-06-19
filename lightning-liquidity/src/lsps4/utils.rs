//! Utilities for implementing the LSPS4 standard.
use std::fmt::Write;

/// Computes the forward fee given a payment size and the fee parameters.
///
/// Returns [`Option::None`] when the computation overflows.
pub fn compute_forward_fee(
	payment_size_msat: u64, fee_proportional: u64,
) -> Option<u64> {
	payment_size_msat.checked_mul(fee_proportional).and_then(|f| f.checked_add(999999)).and_then(|f| f.checked_div(1000000))
}

/// Convert a byte slice to a hex string.
#[inline]
pub fn to_string(value: &[u8]) -> String {
	let mut res = String::with_capacity(2 * value.len());
	for v in value {
		write!(&mut res, "{:02x}", v).expect("Unable to write");
	}
	res
}

#[cfg(test)]
mod tests {
	use super::*;
	use proptest::prelude::*;

	const MAX_VALUE_MSAT: u64 = 21_000_000_0000_0000_000;

	fn arb_forward_fee_params() -> impl Strategy<Value = (u64, u64)> {
		(0u64..MAX_VALUE_MSAT, 0u64..MAX_VALUE_MSAT)
	}

	proptest! {
		#[test]
		fn test_compute_forward_fee((payment_size_msat, fee_proportional) in arb_forward_fee_params()) {
			if let Some(res) = compute_forward_fee(payment_size_msat, fee_proportional) {
				assert_eq!(res as f32, (payment_size_msat as f32 * fee_proportional as f32));
			} else {
				// Check we actually overflowed.
				let max_value = u64::MAX as u128;
				assert!((payment_size_msat as u128 * fee_proportional as u128) > max_value);
			}
		}
	}
}
