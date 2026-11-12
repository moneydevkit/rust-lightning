//! Utilities for implementing the LSPS4 standard.
use std::fmt::Write;

/// Computes the forward fee given a payment size and the fee parameters.
///
/// Returns [`Option::None`] when the computation overflows.
pub fn compute_forward_fee(payment_size_msat: u64, fee_proportional: u64) -> Option<u64> {
	payment_size_msat
		.checked_mul(fee_proportional)
		.and_then(|f| f.checked_add(999999))
		.and_then(|f| f.checked_div(1000000))
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

	#[test]
	fn test_compute_forward_fee() {
		// Fee is in parts per million, rounded up.
		assert_eq!(compute_forward_fee(0, 2_000), Some(0));
		assert_eq!(compute_forward_fee(1_000_000, 0), Some(0));
		assert_eq!(compute_forward_fee(1_000_000, 2_000), Some(2_000));
		assert_eq!(compute_forward_fee(1, 1), Some(1));
		assert_eq!(compute_forward_fee(999_999, 1), Some(1));
		assert_eq!(compute_forward_fee(1_000_001, 1), Some(2));
		// The intermediate product overflows `u64` before the division.
		assert_eq!(compute_forward_fee(u64::MAX, 2), None);
		assert_eq!(compute_forward_fee(u64::MAX, 1), None);
	}
}
