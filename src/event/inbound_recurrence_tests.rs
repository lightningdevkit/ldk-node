// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Component tests drive the production event handler without a chain server or live channels.
//! LDK claim confirmations are delivered explicitly, independently of claimable notifications.

use std::future::{poll_fn, Future};
use std::task::Poll;

use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use lightning::blinded_path::payment::Bolt12OfferContext;
use lightning::io;
use lightning::offers::invoice_request::InvoiceRequestFields;
use lightning::offers::offer::{
	OfferId, Recurrence, RecurrenceBase, RecurrenceLimit, RecurrencePeriod, RecurrenceType,
};
use lightning::util::persist::{PageToken, PaginatedKVStore, PaginatedListResponse};
use lightning::util::ser::BigSize;
use lightning_types::payment::PaymentSecret;

use super::*;
use crate::builder::NodeBuilder;
use crate::entropy::NodeEntropy;
use crate::io::test_utils::InMemoryStore;
use crate::io::{INBOUND_RECURRENCE_PRIMARY_NAMESPACE, PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE};
use crate::Node;

/// Select a single failing write namespace, or park one reservation write to force contention.
#[derive(Default)]
struct WriteControl {
	fail_namespace: Option<&'static str>,
	gate_next_recurrence_write: bool,
}

/// Keep bytes independent of each node's cache so a rebuild exercises actual recovery.
#[derive(Clone)]
struct TestStore {
	inner: Arc<InMemoryStore>,
	control: Arc<Mutex<WriteControl>>,
	write_entered: Arc<tokio::sync::Notify>,
	release_write: Arc<tokio::sync::Notify>,
}

impl TestStore {
	/// Start with working persistence; failures are enabled only after node construction.
	fn new() -> Self {
		Self {
			inner: Arc::new(InMemoryStore::new()),
			control: Arc::new(Mutex::new(WriteControl::default())),
			write_entered: Arc::new(tokio::sync::Notify::new()),
			release_write: Arc::new(tokio::sync::Notify::new()),
		}
	}
}

impl KVStore for TestStore {
	/// Delegate reads so fault injection affects writes, not recovery itself.
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> impl Future<Output = Result<Vec<u8>, io::Error>> + Send + 'static {
		KVStore::read(&*self.inner, primary_namespace, secondary_namespace, key)
	}

	/// Fail before modifying bytes, or hold one write while another handler tries to reserve.
	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> impl Future<Output = Result<(), io::Error>> + Send + 'static {
		let (fail, gate) = {
			let mut control = self.control.lock().unwrap();
			let fail = control.fail_namespace == Some(primary_namespace);
			let gate = primary_namespace == INBOUND_RECURRENCE_PRIMARY_NAMESPACE
				&& std::mem::take(&mut control.gate_next_recurrence_write);
			(fail, gate)
		};
		let entered = Arc::clone(&self.write_entered);
		let release = Arc::clone(&self.release_write);
		let inner = Arc::clone(&self.inner);
		let primary_namespace = primary_namespace.to_owned();
		let secondary_namespace = secondary_namespace.to_owned();
		let key = key.to_owned();
		async move {
			if gate {
				entered.notify_one();
				release.notified().await;
			}
			if fail {
				return Err(io::Error::new(
					io::ErrorKind::Other,
					"injected recurrence test failure",
				));
			}
			// InMemoryStore changes bytes when write is called, not when its future is polled.
			// Defer the call so the gate and injected failure take effect before any write.
			KVStore::write(&*inner, &primary_namespace, &secondary_namespace, &key, buf).await
		}
	}

	/// Keep normal deletion semantics for event acknowledgements and node teardown.
	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> impl Future<Output = Result<(), io::Error>> + Send + 'static {
		KVStore::remove(&*self.inner, primary_namespace, secondary_namespace, key, lazy)
	}

	/// Delegate discovery to the same bytes read by rebuilt nodes.
	fn list(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> impl Future<Output = Result<Vec<String>, io::Error>> + Send + 'static {
		KVStore::list(&*self.inner, primary_namespace, secondary_namespace)
	}
}

impl PaginatedKVStore for TestStore {
	/// The builder loads recurrence records through paginated listing.
	fn list_paginated(
		&self, primary_namespace: &str, secondary_namespace: &str, page_token: Option<PageToken>,
	) -> impl Future<Output = Result<PaginatedListResponse, io::Error>> + Send + 'static {
		PaginatedKVStore::list_paginated(
			&*self.inner,
			primary_namespace,
			secondary_namespace,
			page_token,
		)
	}
}

/// Build without starting network tasks, reusing only persisted bytes between instances.
fn test_node(store: &TestStore) -> Node {
	let mut builder = NodeBuilder::new();
	builder.set_log_facade_logger().set_network(bitcoin::Network::Regtest);
	#[cfg(not(feature = "uniffi"))]
	let entropy = NodeEntropy::from_seed_bytes([42; 64]);
	#[cfg(feature = "uniffi")]
	let entropy = NodeEntropy::from_seed_bytes(vec![42; 64]).unwrap();
	builder.build_with_store(entropy, store.clone()).unwrap()
}

/// Wire the same production dependencies as Node::start, without spawning its event processor.
fn test_handler(node: &Node) -> EventHandler<Arc<Logger>> {
	let bump_handler = Arc::new(BumpTransactionEventHandler::new(
		Arc::clone(&node.tx_broadcaster),
		Arc::new(crate::LdkWallet::new(Arc::clone(&node.wallet), Arc::clone(&node.logger))),
		Arc::clone(&node.keys_manager),
		Arc::clone(&node.logger),
	));
	EventHandler::new(
		Arc::clone(&node.event_queue),
		Arc::clone(&node.wallet),
		bump_handler,
		Arc::clone(&node.channel_manager),
		Arc::clone(&node.connection_manager),
		Arc::clone(&node.output_sweeper),
		Arc::clone(&node.network_graph),
		Arc::clone(&node.liquidity_source),
		Arc::clone(&node.payment_store),
		Arc::clone(&node.inbound_recurrence_store),
		Arc::clone(&node.peer_store),
		Arc::clone(&node.keys_manager),
		None,
		Arc::clone(&node.onion_messenger),
		None,
		None,
		Arc::clone(&node.runtime),
		Arc::clone(&node.logger),
		Arc::clone(&node.config),
	)
}

/// Decode public request fields because LDK keeps recurrence constructors crate-private.
/// The fixture includes only payer identity, counter, optional start, and cancellation.
fn context(counter: u32, start: Option<u32>, cancel: bool) -> Bolt12OfferContext {
	let payer =
		PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[1; 32]).unwrap());
	// Request counters use big-endian integers with leading zero bytes omitted.
	let encode_number = |number: u32| {
		number.to_be_bytes().into_iter().skip_while(|byte| *byte == 0).collect::<Vec<_>>()
	};
	let mut fields = vec![(0, payer.encode()), (3, encode_number(counter))];
	if let Some(start) = start {
		fields.push((5, encode_number(start)));
	}
	if cancel {
		fields.push((7, Vec::new()));
	}
	let mut tlvs = Vec::new();
	for (typ, value) in fields {
		BigSize(typ).write(&mut tlvs).unwrap();
		BigSize(value.len() as u64).write(&mut tlvs).unwrap();
		tlvs.extend(value);
	}
	let mut bytes = BigSize(tlvs.len() as u64).encode();
	bytes.extend(tlvs);
	let invoice_request = InvoiceRequestFields::read(&mut &bytes[..]).unwrap();
	Bolt12OfferContext {
		offer_id: OfferId([1; 32]),
		offer_recurrence: Some(Recurrence {
			recurrence_type: RecurrenceType::Compulsory(
				start.map(|_| RecurrenceBase { proportional: false, basetime: 1_700_000_000 }),
			),
			recurrence_period: RecurrencePeriod::Days(1),
			recurrence_paywindow: None,
			recurrence_limit: None,
		}),
		invoice_request,
		invoice_recurrence_basetime: Some(1_700_000_000),
		payment_metadata: None,
	}
}

/// Give each attempt a distinct preimage/hash while allowing reuse of a counter's context.
fn purpose(context: Bolt12OfferContext, id: PaymentId) -> PaymentPurpose {
	PaymentPurpose::Bolt12OfferPayment {
		payment_preimage: Some(PaymentPreimage(id.0)),
		payment_secret: PaymentSecret([42; 32]),
		payment_context: context,
	}
}

/// Inject a claimable notification; confirmation is deliberately a separate test step.
fn claimable(context: Bolt12OfferContext, id: PaymentId) -> LdkEvent {
	LdkEvent::PaymentClaimable {
		receiver_node_id: None,
		payment_hash: PaymentPreimage(id.0).into(),
		onion_fields: None,
		amount_msat: 100_000,
		counterparty_skimmed_fee_msat: 0,
		purpose: purpose(context, id),
		receiving_channel_ids: Vec::new(),
		claim_deadline: None,
		payment_id: Some(id),
	}
}

/// Inject LDK's final confirmation rather than inferring success from acceptance.
fn claimed(context: Bolt12OfferContext, id: PaymentId) -> LdkEvent {
	LdkEvent::PaymentClaimed {
		receiver_node_id: None,
		payment_hash: PaymentPreimage(id.0).into(),
		amount_msat: 100_000,
		purpose: purpose(context, id),
		htlcs: Vec::new(),
		sender_intended_total_msat: Some(100_000),
		onion_fields: None,
		payment_id: Some(id),
	}
}

/// Read the handler's current relationship snapshot without relying on listing order.
async fn details(
	handler: &EventHandler<Arc<Logger>>, context: &Bolt12OfferContext,
) -> InboundRecurrenceDetails {
	let id =
		InboundRecurrenceId::new(context.offer_id, context.invoice_request.payer_signing_pubkey);
	handler.inbound_recurrence_store.get(&id).await.unwrap().unwrap()
}

/// A context must supply all recurrence inputs and refer to a valid, non-cancelling period.
#[test]
fn inbound_recurrence_context_validation() {
	for start in [None, Some(3)] {
		let valid = context(1, start, false);
		let (saved, counter) = InboundRecurrenceDetails::from_payment_context(&valid).unwrap();
		assert_eq!(counter, 1);
		assert_eq!(saved.offer_id, valid.offer_id);
		assert_eq!(saved.payer_signing_pubkey, valid.invoice_request.payer_signing_pubkey);
		assert_eq!(Some(saved.recurrence), valid.offer_recurrence);
		assert_eq!(saved.basetime, valid.invoice_recurrence_basetime);
		assert_eq!(saved.initial_start, start);
		assert_eq!(saved.status, RecurrenceStatus::Active);
		assert_eq!(saved.claim_status, InboundRecurrenceClaimStatus::Idle);
		assert!(saved.completed_payments.is_empty());

		let mut missing_terms = valid.clone();
		missing_terms.offer_recurrence = None;
		let mut missing_request = valid.clone();
		missing_request.invoice_request.invoice_request_recurrence = None;
		let mut missing_basetime = valid.clone();
		missing_basetime.invoice_recurrence_basetime = None;
		let mut mismatched_form = valid.clone();
		mismatched_form.invoice_request =
			context(1, if start.is_some() { None } else { Some(3) }, false).invoice_request;
		for invalid in [
			missing_terms,
			missing_request,
			missing_basetime,
			mismatched_form,
			context(1, start, true),
		] {
			assert!(InboundRecurrenceDetails::from_payment_context(&invalid).is_none());
		}
	}
	// The limit is an inclusive absolute period index, not the payer's counter.
	let mut at_limit = context(1, Some(3), false);
	at_limit.offer_recurrence.as_mut().unwrap().recurrence_limit = Some(RecurrenceLimit(4));
	assert!(InboundRecurrenceDetails::from_payment_context(&at_limit).is_some());
	let mut over_limit = context(2, Some(3), false);
	over_limit.offer_recurrence = at_limit.offer_recurrence;
	assert!(InboundRecurrenceDetails::from_payment_context(&over_limit).is_none());
	assert!(InboundRecurrenceDetails::from_payment_context(&context(1, Some(u32::MAX), false))
		.is_none());
}

/// One counter has one owner. Rejected attempts cannot damage the accepted payment's details.
#[test]
fn inbound_recurrence_reservation_ownership() {
	let store = TestStore::new();
	let node = test_node(&store);
	let handler = test_handler(&node);
	node.runtime.block_on(async {
		let ctx = context(0, None, false);
		let a = PaymentId([1; 32]);
		let b = PaymentId([2; 32]);
		// Reject a skipped initial counter even when no competing reservation exists.
		handler.handle_event(claimable(context(1, None, false), PaymentId([3; 32]))).await.unwrap();
		assert!(handler.inbound_recurrence_store.list_filter(|_| true).await.is_empty());
		handler.handle_event(claimable(ctx.clone(), a)).await.unwrap();
		let reserved = details(&handler, &ctx).await;
		assert_eq!(reserved.claim_status, InboundRecurrenceClaimStatus::Pending(a));
		assert!(reserved.completed_payments.is_empty());
		let accepted_payment = handler.payment_store.get(&a).await.unwrap().unwrap();
		assert_eq!(accepted_payment.status, PaymentStatus::Pending);
		handler.handle_event(claimable(ctx.clone(), a)).await.unwrap();

		// A saved rejected payment should become failed without changing its hash or owner A.
		let mut rejected = accepted_payment.clone();
		rejected.id = b;
		if let PaymentKind::Bolt12Offer { hash, preimage, .. } = &mut rejected.kind {
			*hash = Some(PaymentPreimage(b.0).into());
			*preimage = Some(PaymentPreimage(b.0));
		}
		handler.payment_store.insert(rejected.clone()).await.unwrap();
		handler.handle_event(claimable(ctx.clone(), b)).await.unwrap();
		let rejected_saved = handler.payment_store.get(&b).await.unwrap().unwrap();
		assert_eq!(rejected_saved.status, PaymentStatus::Failed);
		assert_eq!(rejected_saved.kind, rejected.kind);
		handler.handle_event(claimable(context(1, None, false), PaymentId([3; 32]))).await.unwrap();

		// Invalid replay context must not downgrade the already accepted payment.
		let mut changed = ctx.clone();
		changed.invoice_recurrence_basetime = Some(1_700_000_001);
		handler.handle_event(claimable(changed, a)).await.unwrap();
		assert_eq!(details(&handler, &ctx).await, reserved);
		assert_eq!(handler.payment_store.get(&a).await.unwrap(), Some(accepted_payment));
		assert!(handler.payment_store.get(&PaymentId([3; 32])).await.unwrap().is_none());
		assert!(handler.event_queue.next_event().is_none());
	});
}

/// Only a matching confirmation advances progress; historical replay leaves a newer owner intact.
#[test]
fn inbound_recurrence_confirmed_progress_and_replay() {
	let store = TestStore::new();
	let node = test_node(&store);
	let handler = test_handler(&node);
	node.runtime.block_on(async {
		let ctx = context(0, None, false);
		let a = PaymentId([1; 32]);
		let b = PaymentId([2; 32]);
		assert!(handler.handle_event(claimed(ctx.clone(), a)).await.is_err());
		assert!(handler.payment_store.get(&a).await.unwrap().is_none());
		assert!(handler.event_queue.next_event().is_none());
		handler.handle_event(claimable(ctx.clone(), a)).await.unwrap();
		let reserved = details(&handler, &ctx).await;
		assert!(handler.handle_event(claimed(ctx.clone(), b)).await.is_err());
		assert_eq!(details(&handler, &ctx).await, reserved);
		handler.handle_event(claimed(ctx.clone(), a)).await.unwrap();
		let paid = details(&handler, &ctx).await;
		assert_eq!(paid.completed_payments, vec![a]);
		assert_eq!(paid.claim_status, InboundRecurrenceClaimStatus::Idle);
		assert_eq!(handler.payment_store.get(&a).await.unwrap().unwrap().status, PaymentStatus::Succeeded);
		assert!(matches!(handler.event_queue.next_event(), Some(Event::PaymentReceived { payment_id, .. }) if payment_id == a));
		handler.event_queue.event_handled().await.unwrap();

		handler.handle_event(claimable(context(1, None, false), b)).await.unwrap();
		let next = details(&handler, &ctx).await;
		assert_eq!(next.claim_status, InboundRecurrenceClaimStatus::Pending(b));
		for event in [claimable(ctx.clone(), a), claimed(ctx.clone(), a), claimable(ctx.clone(), PaymentId([3; 32]))] {
			handler.handle_event(event).await.unwrap();
			assert_eq!(details(&handler, &ctx).await, next);
		}
		// Reusing an earlier successful payment ID cannot pay the next counter.
		assert!(!handler.reserve_inbound_recurrence(a, &purpose(context(1, None, false), a), 0).await.unwrap());
	});
}

/// Completion uses the offer's absolute period limit and must never wrap the next period to zero.
#[test]
fn inbound_recurrence_completion_boundaries() {
	for (start, limit, payments, expected_status) in [
		(None, None, 2, RecurrenceStatus::Active),
		(Some(3), Some(4), 2, RecurrenceStatus::Completed),
		(Some(u32::MAX), None, 1, RecurrenceStatus::Completed),
	] {
		let store = TestStore::new();
		let node = test_node(&store);
		let handler = test_handler(&node);
		node.runtime.block_on(async {
			let mut first = context(0, start, false);
			first.offer_recurrence.as_mut().unwrap().recurrence_limit = limit.map(RecurrenceLimit);
			for counter in 0..payments {
				let mut ctx = context(counter, start, false);
				ctx.offer_recurrence = first.offer_recurrence;
				let id = PaymentId([counter as u8 + 1; 32]);
				handler.handle_event(claimable(ctx.clone(), id)).await.unwrap();
				assert_eq!(details(&handler, &first).await.status, RecurrenceStatus::Active);
				handler.handle_event(claimed(ctx, id)).await.unwrap();
			}
			let saved = details(&handler, &first).await;
			assert_eq!(saved.status, expected_status);
			assert_eq!(saved.completed_payments.len(), payments as usize);
			assert_eq!(saved.claim_status, InboundRecurrenceClaimStatus::Idle);
			if expected_status == RecurrenceStatus::Completed {
				let mut next = context(payments, start, false);
				next.offer_recurrence = first.offer_recurrence;
				assert!(!handler
					.reserve_inbound_recurrence(
						PaymentId([9; 32]),
						&purpose(next, PaymentId([9; 32])),
						0
					)
					.await
					.unwrap());
				// A terminal relationship still recognizes earlier successful notifications.
				handler.handle_event(claimable(first.clone(), PaymentId([1; 32]))).await.unwrap();
				assert_eq!(details(&handler, &first).await, saved);
			}
		});
	}
}

/// Fail every distinct persistence boundary, rebuild from bytes, and replay the unfinished event.
/// A durable reservation or success must survive even when the following store write fails.
#[test]
fn inbound_recurrence_persistence_failure_and_retry() {
	for (confirming, namespace) in [
		(false, INBOUND_RECURRENCE_PRIMARY_NAMESPACE),
		(false, PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE),
		(true, INBOUND_RECURRENCE_PRIMARY_NAMESPACE),
		(true, PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE),
		(true, EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE),
	] {
		let store = TestStore::new();
		let node = test_node(&store);
		let handler = test_handler(&node);
		let ctx = context(0, None, false);
		let a = PaymentId([1; 32]);
		let reservation_saved = confirming || namespace != INBOUND_RECURRENCE_PRIMARY_NAMESPACE;
		let progress_saved = confirming && namespace != INBOUND_RECURRENCE_PRIMARY_NAMESPACE;
		node.runtime.block_on(async {
			if confirming {
				handler.handle_event(claimable(ctx.clone(), a)).await.unwrap();
			}
			store.control.lock().unwrap().fail_namespace = Some(namespace);
			let event =
				if confirming { claimed(ctx.clone(), a) } else { claimable(ctx.clone(), a) };
			handler.channel_manager.get_and_clear_needs_persistence();
			assert!(handler.handle_event(event).await.is_err(), "namespace {namespace}");
			let payment = handler.payment_store.get(&a).await.unwrap();
			if !confirming {
				assert!(payment.is_none());
				// Claim and failure calls both notify ChannelManager persistence, even without
				// live HTLCs. Neither may run before the prerequisite store writes succeed.
				assert!(!handler.channel_manager.get_and_clear_needs_persistence());
			} else {
				let expected = if namespace == EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE {
					PaymentStatus::Succeeded
				} else {
					PaymentStatus::Pending
				};
				assert_eq!(payment.unwrap().status, expected);
			}
		});
		drop(handler);
		drop(node);
		store.control.lock().unwrap().fail_namespace = None;
		let node = test_node(&store);
		let handler = test_handler(&node);
		node.runtime.block_on(async {
			let id = InboundRecurrenceId::new(ctx.offer_id, ctx.invoice_request.payer_signing_pubkey);
			let restored = handler.inbound_recurrence_store.get(&id).await.unwrap();
			assert_eq!(restored.is_some(), reservation_saved);
			if let Some(saved) = restored {
				assert_eq!(saved.completed_payments, if progress_saved { vec![a] } else { vec![] });
				assert_eq!(saved.claim_status, if progress_saved { InboundRecurrenceClaimStatus::Idle } else { InboundRecurrenceClaimStatus::Pending(a) });
				if !progress_saved {
					assert!(!handler.reserve_inbound_recurrence(PaymentId([2; 32]), &purpose(ctx.clone(), PaymentId([2; 32])), 0).await.unwrap());
				}
			}
			assert!(handler.event_queue.next_event().is_none());
			let event = if confirming { claimed(ctx.clone(), a) } else { claimable(ctx.clone(), a) };
			handler.channel_manager.get_and_clear_needs_persistence();
			handler.handle_event(event).await.unwrap();
			let saved = details(&handler, &ctx).await;
			if confirming {
				assert_eq!(saved.completed_payments, vec![a]);
				assert_eq!(saved.claim_status, InboundRecurrenceClaimStatus::Idle);
				assert_eq!(handler.payment_store.get(&a).await.unwrap().unwrap().status, PaymentStatus::Succeeded);
				assert!(matches!(handler.event_queue.next_event(), Some(Event::PaymentReceived { payment_id, .. }) if payment_id == a));
				handler.event_queue.event_handled().await.unwrap();
				assert!(handler.event_queue.next_event().is_none());
			} else {
				assert!(handler.channel_manager.get_and_clear_needs_persistence());
				assert!(saved.completed_payments.is_empty());
				assert_eq!(saved.claim_status, InboundRecurrenceClaimStatus::Pending(a));
				assert_eq!(handler.payment_store.get(&a).await.unwrap().unwrap().status, PaymentStatus::Pending);
			}
		});
	}
}

/// Hold the first reservation write open and poll a competing handler before releasing it.
/// Exactly one payment may own the counter, even when the first reservation is not yet durable.
#[test]
fn inbound_recurrence_concurrent_reservations() {
	let store = TestStore::new();
	let node = test_node(&store);
	let handler = test_handler(&node);
	node.runtime.block_on(async {
		let ctx = context(0, None, false);
		let a = PaymentId([1; 32]);
		let b = PaymentId([2; 32]);
		store.control.lock().unwrap().gate_next_recurrence_write = true;
		let first = handler.handle_event(claimable(ctx.clone(), a));
		tokio::pin!(first);
		tokio::select! {
			_ = store.write_entered.notified() => {},
			_ = &mut first => panic!("first reservation must wait at the write barrier"),
		}
		let second = handler.handle_event(claimable(ctx.clone(), b));
		tokio::pin!(second);
		poll_fn(|cx| {
			assert!(matches!(second.as_mut().poll(cx), Poll::Pending));
			Poll::Ready(())
		})
		.await;
		store.release_write.notify_one();
		let (first_result, second_result) = tokio::join!(first, second);
		first_result.unwrap();
		second_result.unwrap();
		assert_eq!(
			details(&handler, &ctx).await.claim_status,
			InboundRecurrenceClaimStatus::Pending(a)
		);
		assert_eq!(
			handler.payment_store.get(&a).await.unwrap().unwrap().status,
			PaymentStatus::Pending
		);
		assert!(handler.payment_store.get(&b).await.unwrap().is_none());
	});
}
