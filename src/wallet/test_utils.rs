// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

/// Builds a [`FundingContribution`] for tests through its `Readable` impl — the only path open
/// outside `rust-lightning`, which keeps its builder private. The length-prefixed stream contains
/// the required TLV records (the given estimated fee in satoshis, feerate, max feerate, and the
/// is-splice flag) plus the given contributed outputs.
///
/// [`FundingContribution`]: lightning::ln::funding::FundingContribution
pub(crate) fn test_funding_contribution_with_outputs(
	estimated_fee_sat: u64, feerate: u64, outputs: &[bitcoin::TxOut],
) -> lightning::ln::funding::FundingContribution {
	test_funding_contribution_with_parts(estimated_fee_sat, feerate, &[], outputs, None)
}

/// Builds a [`FundingContribution`] for tests from its parts: the given estimated fee, an input
/// spending output 0 — which must be P2WPKH — of each given previous transaction, the given
/// contributed outputs and change output, and the given input-selection feerate (also used as
/// the maximum), with the is-splice flag set.
///
/// [`FundingContribution`]: lightning::ln::funding::FundingContribution
pub(crate) fn test_funding_contribution_with_parts(
	estimated_fee_sat: u64, feerate: u64, prevtxs: &[bitcoin::Transaction],
	outputs: &[bitcoin::TxOut], change_output: Option<&bitcoin::TxOut>,
) -> lightning::ln::funding::FundingContribution {
	use lightning::util::ser::{BigSize, Writeable};
	use lightning::util::wallet_utils::ConfirmedUtxo;
	let mut records = vec![1, 8]; // (1, estimated_fee)
	records.extend_from_slice(&estimated_fee_sat.to_be_bytes());
	if !prevtxs.is_empty() {
		let mut input_bytes = Vec::new();
		for prevtx in prevtxs {
			ConfirmedUtxo::new_p2wpkh(prevtx.clone(), 0)
				.expect("test prevtx output 0 must be P2WPKH")
				.write(&mut input_bytes)
				.expect("in-memory write must succeed");
		}
		records.push(3); // (3, inputs)
		BigSize(input_bytes.len() as u64)
			.write(&mut records)
			.expect("in-memory write must succeed");
		records.extend_from_slice(&input_bytes);
	}
	if !outputs.is_empty() {
		let mut output_bytes = Vec::new();
		for output in outputs {
			output.write(&mut output_bytes).expect("in-memory write must succeed");
		}
		records.push(5); // (5, outputs)
		BigSize(output_bytes.len() as u64)
			.write(&mut records)
			.expect("in-memory write must succeed");
		records.extend_from_slice(&output_bytes);
	}
	if let Some(change_output) = change_output {
		let change_bytes = change_output.encode();
		records.push(7); // (7, change_output)
		BigSize(change_bytes.len() as u64)
			.write(&mut records)
			.expect("in-memory write must succeed");
		records.extend_from_slice(&change_bytes);
	}
	records.extend_from_slice(&[9, 8]); // (9, feerate)
	records.extend_from_slice(&feerate.to_be_bytes());
	records.extend_from_slice(&[11, 8]); // (11, max_feerate)
	records.extend_from_slice(&feerate.to_be_bytes());
	records.extend_from_slice(&[13, 1, 1]); // (13, is_splice: true)
	let mut tlv_bytes = Vec::new();
	// BigSize length prefix over the TLV records above.
	BigSize(records.len() as u64).write(&mut tlv_bytes).expect("in-memory write must succeed");
	tlv_bytes.extend(records);
	lightning::util::ser::Readable::read(&mut &tlv_bytes[..])
		.expect("hand-built TLV stream must decode")
}
