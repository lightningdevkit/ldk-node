// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::ops::Deref;
use std::str::FromStr;

use bitcoin::{OutPoint, Txid};
use lightning::impl_writeable_tlv_based;
use lightning::ln::channelmanager::PaymentId;

use crate::data_store::{DataStore, KeepNoEntries, StorableObject, StorableObjectId};
use crate::logger::LdkLogger;
use crate::Error;

/// A sender input we have seen in the original proposal of a receive session.
///
/// These are kept independently of [`PayjoinSession`]s, which are cleaned up after they
/// complete, so that a sender can't reuse inputs across sessions to probe our UTXOs.
///
/// [`PayjoinSession`]: crate::payment::payjoin::payjoin_session::PayjoinSession
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SeenInput {
	/// The sender's input.
	pub outpoint: OutPoint,
	/// The session that first recorded this input.
	pub session_id: PaymentId,
	/// The time, in seconds since start of the UNIX epoch, the input was first seen.
	pub first_seen_at: u64,
}

impl_writeable_tlv_based!(SeenInput, {
	(0, outpoint, required),
	(2, session_id, required),
	(4, first_seen_at, required),
});

impl StorableObjectId for OutPoint {
	fn encode_to_hex_str(&self) -> String {
		format!("{}_{}", self.txid, self.vout)
	}

	fn decode_from_hex_str(s: &str) -> Option<Self> {
		let (txid, vout) = s.split_once('_')?;
		Some(OutPoint { txid: Txid::from_str(txid).ok()?, vout: vout.parse().ok()? })
	}
}

impl StorableObject for SeenInput {
	type Id = OutPoint;

	fn id(&self) -> Self::Id {
		self.outpoint
	}
}

/// Returns whether `outpoint` was already seen by a session other than `session_id`, recording
/// it as seen by `session_id` if it wasn't seen before.
///
/// An input recorded by `session_id` itself is not considered seen, so that a session retried
/// after the input was recorded but before its state transition was persisted doesn't reject
/// itself.
///
/// The check and the insert happen atomically, so concurrent sessions racing on the same input
/// can't both consider it unseen.
pub(crate) async fn check_and_record_seen_input<L: Deref>(
	store: &DataStore<SeenInput, L, KeepNoEntries>, outpoint: OutPoint, session_id: PaymentId,
	now: u64,
) -> Result<bool, Error>
where
	L::Target: LdkLogger,
{
	let mut seen = false;
	store
		.mutate(&outpoint, |existing| match existing {
			// Already recorded: keep the original record and don't write.
			Some(seen_input) => {
				seen = seen_input.session_id != session_id;
				None
			},
			// First sighting: record it.
			None => Some(SeenInput { outpoint, session_id, first_seen_at: now }),
		})
		.await?;
	Ok(seen)
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use bitcoin::hashes::Hash;
	use lightning::util::test_utils::TestLogger;

	use super::*;
	use crate::io::test_utils::InMemoryStore;
	use crate::types::{DynStore, DynStoreWrapper};

	fn new_store() -> DataStore<SeenInput, Arc<TestLogger>, KeepNoEntries> {
		let kv_store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		DataStore::new(
			Vec::new(),
			KeepNoEntries,
			"payjoin_seen_inputs_test".to_string(),
			String::new(),
			kv_store,
			Arc::new(TestLogger::new()),
		)
	}

	fn outpoint(byte: u8, vout: u32) -> OutPoint {
		OutPoint { txid: Txid::from_byte_array([byte; 32]), vout }
	}

	#[test]
	fn outpoint_id_round_trips() {
		for op in [outpoint(0xab, 0), outpoint(0x01, u32::MAX)] {
			let encoded = op.encode_to_hex_str();
			assert_eq!(OutPoint::decode_from_hex_str(&encoded), Some(op));
		}
		assert_eq!(OutPoint::decode_from_hex_str("not_an_outpoint"), None);
		assert_eq!(OutPoint::decode_from_hex_str(&"00".repeat(32)), None);
	}

	#[tokio::test]
	async fn inputs_are_only_seen_by_other_sessions() {
		let store = new_store();
		let session_a = PaymentId([1; 32]);
		let session_b = PaymentId([2; 32]);
		let input = outpoint(0x42, 1);

		// First sighting is recorded and not considered seen.
		assert!(!check_and_record_seen_input(&store, input, session_a, 100).await.unwrap());
		let recorded = store.get(&input).await.unwrap().unwrap();
		assert_eq!(
			recorded,
			SeenInput { outpoint: input, session_id: session_a, first_seen_at: 100 }
		);

		// Retrying the same session doesn't reject itself.
		assert!(!check_and_record_seen_input(&store, input, session_a, 200).await.unwrap());

		// Another session reusing the input is rejected, and the original record is kept.
		assert!(check_and_record_seen_input(&store, input, session_b, 300).await.unwrap());
		assert_eq!(store.get(&input).await.unwrap().unwrap(), recorded);

		// Other inputs are unaffected.
		assert!(!check_and_record_seen_input(&store, outpoint(0x42, 2), session_b, 300)
			.await
			.unwrap());
	}

	#[tokio::test]
	async fn concurrent_sessions_cannot_both_claim_an_input() {
		let store = new_store();
		let input = outpoint(0x42, 1);
		let (a, b) = tokio::join!(
			check_and_record_seen_input(&store, input, PaymentId([1; 32]), 100),
			check_and_record_seen_input(&store, input, PaymentId([2; 32]), 100),
		);
		// Exactly one session gets to use the input.
		assert_ne!(a.unwrap(), b.unwrap());
	}
}
