// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Store implementation for [`StaticInvoice`]s.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::hashes::Hash;
use lightning::blinded_path::message::BlindedMessagePath;
use lightning::impl_writeable_tlv_based;
use lightning::offers::static_invoice::StaticInvoice;
use lightning::util::persist::KVStore;
use lightning::util::ser::{Readable, Writeable};

use crate::hex_utils;
use crate::io::STATIC_INVOICE_STORE_PRIMARY_NAMESPACE;
use crate::payment::asynchronous::rate_limiter::RateLimiter;
use crate::types::DynStore;

struct PersistedStaticInvoice {
	invoice: StaticInvoice,
	request_path: BlindedMessagePath,
}

impl_writeable_tlv_based!(PersistedStaticInvoice, {
	(0, invoice, required),
	(2, request_path, required)
});

#[derive(Debug)]
pub(crate) enum StaticInvoiceStoreError {
	RateLimited,
	Io(lightning::io::Error),
}

pub(crate) struct StaticInvoiceStore {
	kv_store: Arc<DynStore>,
	// A request whose storage operation fails returns its token. ChannelManager processes these
	// events serially and replays a failed event before handling later ones, so this keeps
	// retries admitted until storage recovers. If these operations are ever processed
	// concurrently, admission must be tracked per request instead.
	request_rate_limiter: Mutex<RateLimiter>,
	persist_rate_limiter: Mutex<RateLimiter>,
}

impl StaticInvoiceStore {
	const RATE_LIMITER_BUCKET_CAPACITY: u32 = 5;
	const RATE_LIMITER_REFILL_INTERVAL: Duration = Duration::from_millis(100);
	const RATE_LIMITER_MAX_IDLE: Duration = Duration::from_secs(600);

	pub(crate) fn new(kv_store: Arc<DynStore>) -> Self {
		Self {
			kv_store,
			request_rate_limiter: Mutex::new(RateLimiter::new(
				Self::RATE_LIMITER_BUCKET_CAPACITY,
				Self::RATE_LIMITER_REFILL_INTERVAL,
				Self::RATE_LIMITER_MAX_IDLE,
			)),
			persist_rate_limiter: Mutex::new(RateLimiter::new(
				Self::RATE_LIMITER_BUCKET_CAPACITY,
				Self::RATE_LIMITER_REFILL_INTERVAL,
				Self::RATE_LIMITER_MAX_IDLE,
			)),
		}
	}

	pub(crate) async fn handle_static_invoice_requested(
		&self, recipient_id: &[u8], invoice_slot: u16,
	) -> Result<Option<(StaticInvoice, BlindedMessagePath)>, StaticInvoiceStoreError> {
		if !self.request_rate_limiter.lock().expect("lock").allow(recipient_id) {
			return Err(StaticInvoiceStoreError::RateLimited);
		}

		let res = self.read_static_invoice(recipient_id, invoice_slot).await;
		if res.is_err() {
			self.request_rate_limiter.lock().expect("lock").refund(recipient_id);
		}
		res.map_err(StaticInvoiceStoreError::Io)
	}

	pub(crate) async fn handle_persist_static_invoice(
		&self, invoice: StaticInvoice, invoice_request_path: BlindedMessagePath, invoice_slot: u16,
		recipient_id: Vec<u8>,
	) -> Result<(), StaticInvoiceStoreError> {
		if !self.persist_rate_limiter.lock().expect("lock").allow(&recipient_id) {
			return Err(StaticInvoiceStoreError::RateLimited);
		}

		let res = self
			.write_static_invoice(invoice, invoice_request_path, invoice_slot, &recipient_id)
			.await;
		if res.is_err() {
			self.persist_rate_limiter.lock().expect("lock").refund(&recipient_id);
		}
		res.map_err(StaticInvoiceStoreError::Io)
	}

	async fn read_static_invoice(
		&self, recipient_id: &[u8], invoice_slot: u16,
	) -> Result<Option<(StaticInvoice, BlindedMessagePath)>, lightning::io::Error> {
		let (secondary_namespace, key) = Self::get_storage_location(invoice_slot, recipient_id);

		KVStore::read(
			&*self.kv_store,
			STATIC_INVOICE_STORE_PRIMARY_NAMESPACE,
			&secondary_namespace,
			&key,
		)
		.await
		.and_then(|data| {
			PersistedStaticInvoice::read(&mut &*data)
				.map(|persisted_invoice| {
					Some((persisted_invoice.invoice, persisted_invoice.request_path))
				})
				.map_err(|e| {
					lightning::io::Error::new(
						lightning::io::ErrorKind::InvalidData,
						format!("Failed to parse static invoice: {:?}", e),
					)
				})
		})
		.or_else(|e| if e.kind() == lightning::io::ErrorKind::NotFound { Ok(None) } else { Err(e) })
	}

	async fn write_static_invoice(
		&self, invoice: StaticInvoice, invoice_request_path: BlindedMessagePath, invoice_slot: u16,
		recipient_id: &[u8],
	) -> Result<(), lightning::io::Error> {
		let (secondary_namespace, key) = Self::get_storage_location(invoice_slot, recipient_id);

		let persisted_invoice =
			PersistedStaticInvoice { invoice, request_path: invoice_request_path };

		let mut buf = Vec::new();
		persisted_invoice.write(&mut buf)?;

		// Static invoices will be persisted at "static_invoices/<sha256(recipient_id)>/<invoice_slot>".
		//
		// Example: static_invoices/039058c6f2c0cb492c533b0a4d14ef77cc0f78abccced5287d84a1a2011cfb81/00001
		KVStore::write(
			&*self.kv_store,
			STATIC_INVOICE_STORE_PRIMARY_NAMESPACE,
			&secondary_namespace,
			&key,
			buf,
		)
		.await
	}

	fn get_storage_location(invoice_slot: u16, recipient_id: &[u8]) -> (String, String) {
		let hash = Sha256::hash(recipient_id).to_byte_array();
		let secondary_namespace = hex_utils::to_string(&hash);

		let key = format!("{:05}", invoice_slot);
		(secondary_namespace, key)
	}
}

#[cfg(test)]
mod tests {
	use std::future::Future;
	use std::sync::atomic::{AtomicBool, Ordering};
	use std::sync::Arc;
	use std::time::Duration;

	use bitcoin::key::{Keypair, Secp256k1};
	use bitcoin::secp256k1::{PublicKey, SecretKey};
	use lightning::blinded_path::message::BlindedMessagePath;
	use lightning::blinded_path::payment::{BlindedPayInfo, BlindedPaymentPath};
	use lightning::blinded_path::BlindedHop;
	use lightning::io;
	use lightning::ln::inbound_payment::ExpandedKey;
	use lightning::offers::nonce::Nonce;
	use lightning::offers::offer::OfferBuilder;
	use lightning::offers::static_invoice::{StaticInvoice, StaticInvoiceBuilder};
	use lightning::sign::EntropySource;
	use lightning::util::persist::{KVStore, PageToken, PaginatedKVStore, PaginatedListResponse};
	use lightning_types::features::BlindedHopFeatures;

	use crate::io::test_utils::InMemoryStore;
	use crate::payment::asynchronous::rate_limiter::RateLimiter;
	use crate::payment::asynchronous::static_invoice_store::{
		StaticInvoiceStore, StaticInvoiceStoreError,
	};
	use crate::types::{DynStore, DynStoreWrapper};

	struct FailingStore {
		inner: InMemoryStore,
		fail: Arc<AtomicBool>,
	}

	impl FailingStore {
		fn check(&self) -> Result<(), io::Error> {
			if self.fail.load(Ordering::Relaxed) {
				Err(io::Error::new(io::ErrorKind::Other, "injected storage failure"))
			} else {
				Ok(())
			}
		}
	}

	impl KVStore for FailingStore {
		fn read(
			&self, primary: &str, secondary: &str, key: &str,
		) -> impl Future<Output = Result<Vec<u8>, io::Error>> + Send + 'static {
			let res = self.check().map(|()| self.inner.read(primary, secondary, key));
			async move { res?.await }
		}

		fn write(
			&self, primary: &str, secondary: &str, key: &str, buf: Vec<u8>,
		) -> impl Future<Output = Result<(), io::Error>> + Send + 'static {
			let res = self.check().map(|()| self.inner.write(primary, secondary, key, buf));
			async move { res?.await }
		}

		fn remove(
			&self, primary: &str, secondary: &str, key: &str, lazy: bool,
		) -> impl Future<Output = Result<(), io::Error>> + Send + 'static {
			self.inner.remove(primary, secondary, key, lazy)
		}

		fn list(
			&self, primary: &str, secondary: &str,
		) -> impl Future<Output = Result<Vec<String>, io::Error>> + Send + 'static {
			self.inner.list(primary, secondary)
		}
	}

	impl PaginatedKVStore for FailingStore {
		fn list_paginated(
			&self, primary: &str, secondary: &str, page_token: Option<PageToken>,
		) -> impl Future<Output = Result<PaginatedListResponse, io::Error>> + Send + 'static {
			self.inner.list_paginated(primary, secondary, page_token)
		}
	}

	fn single_token_store(kv_store: Arc<DynStore>) -> StaticInvoiceStore {
		let mut store = StaticInvoiceStore::new(kv_store);
		for limiter in [&mut store.request_rate_limiter, &mut store.persist_rate_limiter] {
			*limiter.get_mut().unwrap() =
				RateLimiter::new(1, Duration::from_secs(3600), Duration::from_secs(7200));
		}
		store
	}

	#[tokio::test]
	async fn static_invoice_store_test() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let static_invoice_store = StaticInvoiceStore::new(Arc::clone(&store));

		let static_invoice = invoice();
		let recipient_id = vec![1, 1, 1];
		let invoice_request_path = blinded_path();
		assert!(static_invoice_store
			.handle_persist_static_invoice(
				static_invoice.clone(),
				invoice_request_path.clone(),
				0,
				recipient_id.clone()
			)
			.await
			.is_ok());

		let requested_invoice =
			static_invoice_store.handle_static_invoice_requested(&recipient_id, 0).await.unwrap();

		assert_eq!(requested_invoice.unwrap(), (static_invoice, invoice_request_path));

		assert!(static_invoice_store
			.handle_static_invoice_requested(&recipient_id, 1)
			.await
			.unwrap()
			.is_none());

		assert!(static_invoice_store
			.handle_static_invoice_requested(&[2, 2, 2], 0)
			.await
			.unwrap()
			.is_none());
	}

	#[tokio::test]
	async fn static_invoice_rate_limiting() {
		let kv_store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let mut store = single_token_store(kv_store);
		let recipient_id = vec![1, 1, 1];

		store
			.handle_persist_static_invoice(invoice(), blinded_path(), 0, recipient_id.clone())
			.await
			.unwrap();
		assert!(matches!(
			store
				.handle_persist_static_invoice(invoice(), blinded_path(), 0, recipient_id.clone())
				.await,
			Err(StaticInvoiceStoreError::RateLimited)
		));

		// Missing invoices consume quota too, and rejected requests must not refund it.
		assert!(store.handle_static_invoice_requested(&recipient_id, 1).await.unwrap().is_none());
		for _ in 0..2 {
			assert!(matches!(
				store.handle_static_invoice_requested(&recipient_id, 0).await,
				Err(StaticInvoiceStoreError::RateLimited)
			));
		}
		assert!(store.handle_static_invoice_requested(&[2, 2, 2], 0).await.is_ok());

		store.request_rate_limiter.get_mut().unwrap().advance_refill_interval();
		assert!(store.handle_static_invoice_requested(&recipient_id, 0).await.unwrap().is_some());
	}

	#[tokio::test]
	async fn static_invoice_storage_errors_retain_admission() {
		let fail = Arc::new(AtomicBool::new(true));
		let kv_store: Arc<DynStore> = Arc::new(DynStoreWrapper(FailingStore {
			inner: InMemoryStore::new(),
			fail: Arc::clone(&fail),
		}));
		let store = single_token_store(kv_store);
		let recipient_id = vec![1, 1, 1];
		let invoice = invoice();

		// Retries of a failed event must not exhaust the recipient's quota.
		for _ in 0..3 {
			assert!(matches!(
				store
					.handle_persist_static_invoice(
						invoice.clone(),
						blinded_path(),
						0,
						recipient_id.clone()
					)
					.await,
				Err(StaticInvoiceStoreError::Io(_))
			));
			assert!(matches!(
				store.handle_static_invoice_requested(&recipient_id, 0).await,
				Err(StaticInvoiceStoreError::Io(_))
			));
		}

		fail.store(false, Ordering::Relaxed);
		store
			.handle_persist_static_invoice(invoice.clone(), blinded_path(), 0, recipient_id.clone())
			.await
			.unwrap();
		assert_eq!(
			store.handle_static_invoice_requested(&recipient_id, 0).await.unwrap(),
			Some((invoice, blinded_path()))
		);
	}

	fn invoice() -> StaticInvoice {
		let node_id = recipient_pubkey();
		let payment_paths = payment_paths();
		let now = now();
		let expanded_key = ExpandedKey::new([42; 32]);
		let entropy = FixedEntropy {};
		let nonce = Nonce::from_entropy_source(&entropy);
		let secp_ctx = Secp256k1::new();

		let offer = OfferBuilder::deriving_signing_pubkey(node_id, &expanded_key, nonce, &secp_ctx)
			.path(blinded_path())
			.build()
			.unwrap();

		StaticInvoiceBuilder::for_offer_using_derived_keys(
			&offer,
			payment_paths.clone(),
			vec![blinded_path()],
			now,
			&expanded_key,
			nonce,
			&secp_ctx,
		)
		.unwrap()
		.build_and_sign(&secp_ctx)
		.unwrap()
	}

	fn now() -> Duration {
		std::time::SystemTime::now()
			.duration_since(std::time::SystemTime::UNIX_EPOCH)
			.expect("SystemTime::now() should come after SystemTime::UNIX_EPOCH")
	}

	fn payment_paths() -> Vec<BlindedPaymentPath> {
		vec![
			BlindedPaymentPath::from_blinded_path_and_payinfo(
				pubkey(40),
				pubkey(41),
				vec![
					BlindedHop { blinded_node_id: pubkey(43), encrypted_payload: vec![0; 43] },
					BlindedHop { blinded_node_id: pubkey(44), encrypted_payload: vec![0; 44] },
				],
				BlindedPayInfo {
					fee_base_msat: 1,
					fee_proportional_millionths: 1_000,
					cltv_expiry_delta: 42,
					htlc_minimum_msat: 100,
					htlc_maximum_msat: 1_000_000_000_000,
					features: BlindedHopFeatures::empty(),
				},
			),
			BlindedPaymentPath::from_blinded_path_and_payinfo(
				pubkey(40),
				pubkey(41),
				vec![
					BlindedHop { blinded_node_id: pubkey(45), encrypted_payload: vec![0; 45] },
					BlindedHop { blinded_node_id: pubkey(46), encrypted_payload: vec![0; 46] },
				],
				BlindedPayInfo {
					fee_base_msat: 1,
					fee_proportional_millionths: 1_000,
					cltv_expiry_delta: 42,
					htlc_minimum_msat: 100,
					htlc_maximum_msat: 1_000_000_000_000,
					features: BlindedHopFeatures::empty(),
				},
			),
		]
	}

	fn blinded_path() -> BlindedMessagePath {
		BlindedMessagePath::from_blinded_path(
			pubkey(40),
			pubkey(41),
			vec![
				BlindedHop { blinded_node_id: pubkey(42), encrypted_payload: vec![0; 43] },
				BlindedHop { blinded_node_id: pubkey(43), encrypted_payload: vec![0; 44] },
			],
		)
	}

	fn pubkey(byte: u8) -> PublicKey {
		let secp_ctx = Secp256k1::new();
		PublicKey::from_secret_key(&secp_ctx, &privkey(byte))
	}

	fn privkey(byte: u8) -> SecretKey {
		SecretKey::from_slice(&[byte; 32]).unwrap()
	}

	fn recipient_keys() -> Keypair {
		let secp_ctx = Secp256k1::new();
		Keypair::from_secret_key(&secp_ctx, &SecretKey::from_slice(&[43; 32]).unwrap())
	}

	fn recipient_pubkey() -> PublicKey {
		recipient_keys().public_key()
	}

	struct FixedEntropy;

	impl EntropySource for FixedEntropy {
		fn get_secure_random_bytes(&self) -> [u8; 32] {
			[42; 32]
		}
	}
}
