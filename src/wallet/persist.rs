// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use bdk_chain::Merge;
use bdk_wallet::{AsyncWalletPersister, ChangeSet};
use lightning::impl_writeable_tlv_based;
use lightning::util::persist::KVStore;
use lightning::util::ser::{Readable, Writeable};

use crate::io::utils::{
	read_bdk_wallet_change_set, write_bdk_wallet_change_descriptor, write_bdk_wallet_descriptor,
	write_bdk_wallet_indexer, write_bdk_wallet_local_chain, write_bdk_wallet_network,
	write_bdk_wallet_tx_graph,
};
use crate::io::{
	BDK_WALLET_ADDRESS_POOL_KEY, BDK_WALLET_ADDRESS_POOL_PRIMARY_NAMESPACE,
	BDK_WALLET_ADDRESS_POOL_SECONDARY_NAMESPACE,
};
use crate::logger::{log_error, LdkLogger, Logger};
use crate::types::DynStore;

/// The persisted derivation indices of the wallet's address pool.
///
/// The record is advisory and reconstructible: it is written before the reveals it references
/// are necessarily persisted, so it may briefly lead the persisted [`ChangeSet::indexer`].
/// Readers must validate each index against the wallet's revealed range before handing out any
/// of the addresses; dropped entries are re-derived to the same indices by the next refill.
///
/// [`ChangeSet::indexer`]: bdk_wallet::ChangeSet::indexer
struct AddressPoolRecord {
	indices: Vec<u32>,
}

impl_writeable_tlv_based!(AddressPoolRecord, {
	(0, indices, required_vec),
});

pub(crate) struct KVStoreWalletPersister {
	latest_change_set: Option<ChangeSet>,
	pending_change_set: ChangeSet,
	kv_store: Arc<DynStore>,
	logger: Arc<Logger>,
}

impl KVStoreWalletPersister {
	pub(crate) fn new(kv_store: Arc<DynStore>, logger: Arc<Logger>) -> Self {
		Self { latest_change_set: None, pending_change_set: ChangeSet::default(), kv_store, logger }
	}

	async fn initialize_inner(&mut self) -> Result<ChangeSet, std::io::Error> {
		// Return immediately if we have already been initialized.
		if let Some(latest_change_set) = self.latest_change_set.as_ref() {
			return Ok(latest_change_set.clone());
		}

		let change_set_opt = read_bdk_wallet_change_set(&*self.kv_store, &*self.logger).await?;

		let change_set = match change_set_opt {
			Some(persisted_change_set) => persisted_change_set,
			None => {
				// BDK docs state: "The implementation must return all data currently stored in the
				// persister. If there is no data, return an empty changeset (using
				// ChangeSet::default())."
				ChangeSet::default()
			},
		};
		self.latest_change_set = Some(change_set.clone());
		Ok(change_set)
	}

	async fn persist_inner(
		latest_change_set_opt: &mut Option<ChangeSet>, kv_store: &Arc<DynStore>,
		logger: &Arc<Logger>, change_set: &ChangeSet,
	) -> Result<(), std::io::Error> {
		if change_set.is_empty() {
			return Ok(());
		}

		let kv_store = kv_store.as_ref();

		// We're allowed to fail here if we're not initialized, BDK docs state: "This method can fail if the
		// persister is not initialized."
		let latest_change_set = latest_change_set_opt.as_mut().ok_or_else(|| {
			std::io::Error::new(
				std::io::ErrorKind::Other,
				"Wallet must be initialized before calling persist",
			)
		})?;

		// Check that we'd never accidentally override any persisted data if the change set doesn't
		// match our descriptor/change_descriptor/network.
		if let Some(descriptor) = change_set.descriptor.as_ref() {
			if latest_change_set.descriptor.is_some()
				&& latest_change_set.descriptor.as_ref() != Some(descriptor)
			{
				debug_assert!(false, "Wallet descriptor must never change");
				log_error!(
					logger,
					"Wallet change set doesn't match persisted descriptor. This should never happen."
				);
				return Err(std::io::Error::new(
					std::io::ErrorKind::InvalidData,
					"Wallet change set doesn't match persisted descriptor. This should never happen."
				));
			} else {
				latest_change_set.descriptor = Some(descriptor.clone());
				write_bdk_wallet_descriptor(&descriptor, &*kv_store, Arc::clone(&logger)).await?;
			}
		}

		if let Some(change_descriptor) = change_set.change_descriptor.as_ref() {
			if latest_change_set.change_descriptor.is_some()
				&& latest_change_set.change_descriptor.as_ref() != Some(change_descriptor)
			{
				debug_assert!(false, "Wallet change_descriptor must never change");
				log_error!(
					logger,
					"Wallet change set doesn't match persisted change_descriptor. This should never happen."
				);
				return Err(std::io::Error::new(
					std::io::ErrorKind::InvalidData,
					"Wallet change set doesn't match persisted change_descriptor. This should never happen."
				));
			} else {
				latest_change_set.change_descriptor = Some(change_descriptor.clone());
				write_bdk_wallet_change_descriptor(
					&change_descriptor,
					&*kv_store,
					Arc::clone(&logger),
				)
				.await?;
			}
		}

		if let Some(network) = change_set.network {
			if latest_change_set.network.is_some() && latest_change_set.network != Some(network) {
				debug_assert!(false, "Wallet network must never change");
				log_error!(
					logger,
					"Wallet change set doesn't match persisted network. This should never happen."
				);
				return Err(std::io::Error::new(
					std::io::ErrorKind::InvalidData,
					"Wallet change set doesn't match persisted network. This should never happen.",
				));
			} else {
				latest_change_set.network = Some(network);
				write_bdk_wallet_network(&network, &*kv_store, Arc::clone(&logger)).await?;
			}
		}

		debug_assert!(
			latest_change_set.descriptor.is_some()
				&& latest_change_set.change_descriptor.is_some()
				&& latest_change_set.network.is_some(),
			"descriptor, change_descriptor, and network are mandatory ChangeSet fields"
		);

		// Merge and persist the sub-changesets individually if necessary.
		//
		// According to the BDK team the individual sub-changesets can be persisted
		// individually/non-atomically, "(h)owever, the localchain tip is used by block-by-block
		// chain sources as a reference as to where to sync from, so I would persist that last", "I
		// would write in this order: indexer, tx_graph, local_chain", which is why we follow this
		// particular order.
		if !change_set.indexer.is_empty() {
			latest_change_set.indexer.merge(change_set.indexer.clone());
			write_bdk_wallet_indexer(&latest_change_set.indexer, &*kv_store, Arc::clone(&logger))
				.await?;
		}

		if !change_set.tx_graph.is_empty() {
			latest_change_set.tx_graph.merge(change_set.tx_graph.clone());
			write_bdk_wallet_tx_graph(&latest_change_set.tx_graph, &*kv_store, Arc::clone(&logger))
				.await?;
		}

		if !change_set.local_chain.is_empty() {
			latest_change_set.local_chain.merge(change_set.local_chain.clone());
			write_bdk_wallet_local_chain(
				&latest_change_set.local_chain,
				&*kv_store,
				Arc::clone(&logger),
			)
			.await?;
		}

		Ok(())
	}

	pub(super) async fn persist_changeset(
		&mut self, change_set: ChangeSet,
	) -> Result<(), std::io::Error> {
		self.stage(change_set);
		self.persist_staged().await
	}

	/// Merges the given change set into the pending one without persisting it, to be flushed by
	/// [`Self::persist_staged`] or any later persist call.
	///
	/// Staging is synchronous, letting callers that run as abortable tasks hand a change set
	/// over without an intervening await point: once staged, an abort leaves it pending for the
	/// next persist rather than dropping it.
	pub(super) fn stage(&mut self, change_set: ChangeSet) {
		self.pending_change_set.merge(change_set);
	}

	/// Persists the pending change set, retaining it for retry on failure.
	pub(super) async fn persist_staged(&mut self) -> Result<(), std::io::Error> {
		Self::persist_inner(
			&mut self.latest_change_set,
			&self.kv_store,
			&self.logger,
			&self.pending_change_set,
		)
		.await?;
		let _ = std::mem::take(&mut self.pending_change_set);
		Ok(())
	}

	/// Reads the persisted address-pool derivation indices, or an empty list if none were
	/// persisted yet.
	pub(super) async fn read_address_pool(&self) -> Result<Vec<u32>, std::io::Error> {
		let reader = match KVStore::read(
			&*self.kv_store,
			BDK_WALLET_ADDRESS_POOL_PRIMARY_NAMESPACE,
			BDK_WALLET_ADDRESS_POOL_SECONDARY_NAMESPACE,
			BDK_WALLET_ADDRESS_POOL_KEY,
		)
		.await
		{
			Ok(reader) => reader,
			Err(e) if e.kind() == lightning::io::ErrorKind::NotFound => return Ok(Vec::new()),
			Err(e) => return Err(e.into()),
		};
		let record = match AddressPoolRecord::read(&mut &*reader) {
			Ok(record) => record,
			Err(e) => {
				// The record is a reconstructible cache: a decode failure (corruption, or a
				// future version's incompatible encoding) at worst costs the pool's indices, so
				// degrade to an empty pool rather than failing the node's startup.
				log_error!(self.logger, "Dropping undecodable address pool: {}", e);
				return Ok(Vec::new());
			},
		};
		Ok(record.indices)
	}

	/// Persists the address-pool derivation indices.
	///
	/// See [`AddressPoolRecord`] for what readers may assume about the persisted indices.
	pub(super) async fn persist_address_pool(
		&mut self, indices: Vec<u32>,
	) -> Result<(), std::io::Error> {
		let record = AddressPoolRecord { indices };
		KVStore::write(
			&*self.kv_store,
			BDK_WALLET_ADDRESS_POOL_PRIMARY_NAMESPACE,
			BDK_WALLET_ADDRESS_POOL_SECONDARY_NAMESPACE,
			BDK_WALLET_ADDRESS_POOL_KEY,
			record.encode(),
		)
		.await
		.map_err(|e| {
			log_error!(self.logger, "Failed to persist address pool: {}", e);
			e.into()
		})
	}
}

impl AsyncWalletPersister for KVStoreWalletPersister {
	type Error = std::io::Error;

	fn initialize<'a>(
		persister: &'a mut Self,
	) -> Pin<Box<dyn Future<Output = Result<ChangeSet, Self::Error>> + Send + 'a>>
	where
		Self: 'a,
	{
		Box::pin(persister.initialize_inner())
	}

	fn persist<'a>(
		persister: &'a mut Self, change_set: &'a ChangeSet,
	) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>>
	where
		Self: 'a,
	{
		Box::pin(Self::persist_inner(
			&mut persister.latest_change_set,
			&persister.kv_store,
			&persister.logger,
			change_set,
		))
	}
}

#[cfg(test)]
mod tests {
	use std::future::Future;
	use std::sync::Arc;
	use std::time::Duration;

	use bdk_wallet::{AsyncWalletPersister, ChangeSet, Wallet as BdkWallet};
	use bitcoin::Network;
	use lightning::io;
	use lightning::util::persist::{KVStore, PageToken, PaginatedKVStore, PaginatedListResponse};

	use super::KVStoreWalletPersister;
	use crate::io::test_utils::InMemoryStore;
	use crate::logger::Logger;
	use crate::types::{DynStore, DynStoreWrapper};

	const EXTERNAL_DESCRIPTOR: &str = "wpkh(tprv8ZgxMBicQKsPdy6LMhUtFHAgpocR8GC6QmwMSFpZs7h6Eziw3SpThFfczTDh5rW2krkqffa11UpX3XkeTTB2FvzZKWXqPY54Y6Rq4AQ5R8L/84'/1'/0'/0/*)";
	const INTERNAL_DESCRIPTOR: &str = "wpkh(tprv8ZgxMBicQKsPdy6LMhUtFHAgpocR8GC6QmwMSFpZs7h6Eziw3SpThFfczTDh5rW2krkqffa11UpX3XkeTTB2FvzZKWXqPY54Y6Rq4AQ5R8L/84'/1'/0'/1/*)";

	#[derive(Clone)]
	struct GatedStore {
		inner: Arc<InMemoryStore>,
		write_gate: Arc<tokio::sync::RwLock<()>>,
	}

	impl KVStore for GatedStore {
		fn read(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
		) -> impl Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
			KVStore::read(&*self.inner, primary_namespace, secondary_namespace, key)
		}

		fn write(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
		) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
			let inner = Arc::clone(&self.inner);
			let write_gate = Arc::clone(&self.write_gate);
			let primary_namespace = primary_namespace.to_string();
			let secondary_namespace = secondary_namespace.to_string();
			let key = key.to_string();
			async move {
				let _guard = write_gate.read().await;
				KVStore::write(&*inner, &primary_namespace, &secondary_namespace, &key, buf).await
			}
		}

		fn remove(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
		) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
			KVStore::remove(&*self.inner, primary_namespace, secondary_namespace, key, lazy)
		}

		fn list(
			&self, primary_namespace: &str, secondary_namespace: &str,
		) -> impl Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
			KVStore::list(&*self.inner, primary_namespace, secondary_namespace)
		}
	}

	impl PaginatedKVStore for GatedStore {
		fn list_paginated(
			&self, primary_namespace: &str, secondary_namespace: &str,
			page_token: Option<PageToken>,
		) -> impl Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send {
			PaginatedKVStore::list_paginated(
				&*self.inner,
				primary_namespace,
				secondary_namespace,
				page_token,
			)
		}
	}

	#[tokio::test]
	async fn retains_pending_changes_when_persist_is_cancelled() {
		let gated_store = GatedStore {
			inner: Arc::new(InMemoryStore::new()),
			write_gate: Arc::new(tokio::sync::RwLock::new(())),
		};
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(gated_store.clone()));
		let logger = Arc::new(Logger::new_log_facade());
		let mut persister = KVStoreWalletPersister::new(Arc::clone(&store), Arc::clone(&logger));
		AsyncWalletPersister::initialize(&mut persister).await.unwrap();

		let mut wallet = BdkWallet::create(EXTERNAL_DESCRIPTOR, INTERNAL_DESCRIPTOR)
			.network(Network::Regtest)
			.create_wallet_no_persist()
			.unwrap();
		let change_set = wallet.take_staged().unwrap();

		let gate_guard = gated_store.write_gate.write().await;
		{
			let persist_fut = persister.persist_changeset(change_set);
			tokio::pin!(persist_fut);
			let poll_res = tokio::time::timeout(Duration::from_millis(100), &mut persist_fut).await;
			assert!(poll_res.is_err(), "persist should be parked on the gated store write");
		}
		drop(gate_guard);

		persister.persist_changeset(ChangeSet::default()).await.unwrap();

		let mut reloaded_persister = KVStoreWalletPersister::new(store, logger);
		let reloaded = AsyncWalletPersister::initialize(&mut reloaded_persister).await.unwrap();
		assert_eq!(reloaded.network, Some(Network::Regtest));
	}

	#[tokio::test]
	async fn retries_changes_after_persistence_failure() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(Logger::new_log_facade());
		let mut persister = KVStoreWalletPersister::new(Arc::clone(&store), Arc::clone(&logger));
		let mut wallet = BdkWallet::create(EXTERNAL_DESCRIPTOR, INTERNAL_DESCRIPTOR)
			.network(Network::Regtest)
			.create_wallet_no_persist()
			.unwrap();
		let change_set = wallet.take_staged().unwrap();

		assert!(persister.persist_changeset(change_set).await.is_err());
		AsyncWalletPersister::initialize(&mut persister).await.unwrap();
		persister.persist_changeset(ChangeSet::default()).await.unwrap();

		let mut reloaded_persister = KVStoreWalletPersister::new(store, logger);
		let reloaded = AsyncWalletPersister::initialize(&mut reloaded_persister).await.unwrap();
		assert_eq!(reloaded.network, Some(Network::Regtest));
	}
}
