// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::sync::Arc;

use bdk_chain::Merge;
use bdk_wallet::{ChangeSet, WalletPersister};

use crate::config::AddressType;
use crate::io::utils::{
	read_bdk_wallet_change_set, write_bdk_wallet_change_descriptor, write_bdk_wallet_descriptor,
	write_bdk_wallet_indexer, write_bdk_wallet_local_chain, write_bdk_wallet_network,
	write_bdk_wallet_tx_graph,
};
use crate::logger::{log_error, LdkLogger, Logger};
use crate::types::DynStore;
pub(crate) struct KVStoreWalletPersister {
	latest_change_set: Option<ChangeSet>,
	kv_store: Arc<DynStore>,
	logger: Arc<Logger>,
	address_type: AddressType,
}

impl KVStoreWalletPersister {
	pub(crate) fn new(
		kv_store: Arc<DynStore>, logger: Arc<Logger>, address_type: AddressType,
	) -> Self {
		Self { latest_change_set: None, kv_store, logger, address_type }
	}
}

impl WalletPersister for KVStoreWalletPersister {
	type Error = std::io::Error;

	fn initialize(persister: &mut Self) -> Result<ChangeSet, Self::Error> {
		// Return immediately if we have already been initialized.
		if let Some(latest_change_set) = persister.latest_change_set.as_ref() {
			return Ok(latest_change_set.clone());
		}

		let change_set_opt = read_bdk_wallet_change_set(
			Arc::clone(&persister.kv_store),
			Arc::clone(&persister.logger),
			persister.address_type,
		)?;

		let change_set = match change_set_opt {
			Some(persisted_change_set) => persisted_change_set,
			None => {
				// BDK docs state: "The implementation must return all data currently stored in the
				// persister. If there is no data, return an empty changeset (using
				// ChangeSet::default())."
				ChangeSet::default()
			},
		};
		persister.latest_change_set = Some(change_set.clone());
		Ok(change_set)
	}

	fn persist(persister: &mut Self, change_set: &ChangeSet) -> Result<(), Self::Error> {
		if change_set.is_empty() {
			return Ok(());
		}

		// We're allowed to fail here if we're not initialized, BDK docs state: "This method can fail if the
		// persister is not initialized."
		let latest_change_set = persister.latest_change_set.as_mut().ok_or_else(|| {
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
					persister.logger,
					"Wallet change set doesn't match persisted descriptor. This should never happen."
				);
				return Err(std::io::Error::new(
					std::io::ErrorKind::InvalidData,
					"Wallet change set doesn't match persisted descriptor. This should never happen."
				));
			} else {
				latest_change_set.descriptor = Some(descriptor.clone());
				write_bdk_wallet_descriptor(
					&descriptor,
					Arc::clone(&persister.kv_store),
					Arc::clone(&persister.logger),
					persister.address_type,
				)?;
			}
		}

		if let Some(change_descriptor) = change_set.change_descriptor.as_ref() {
			if latest_change_set.change_descriptor.is_some()
				&& latest_change_set.change_descriptor.as_ref() != Some(change_descriptor)
			{
				debug_assert!(false, "Wallet change_descriptor must never change");
				log_error!(
					persister.logger,
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
					Arc::clone(&persister.kv_store),
					Arc::clone(&persister.logger),
					persister.address_type,
				)?;
			}
		}

		if let Some(network) = change_set.network {
			if latest_change_set.network.is_some() && latest_change_set.network != Some(network) {
				debug_assert!(false, "Wallet network must never change");
				log_error!(
					persister.logger,
					"Wallet change set doesn't match persisted network. This should never happen."
				);
				return Err(std::io::Error::new(
					std::io::ErrorKind::InvalidData,
					"Wallet change set doesn't match persisted network. This should never happen.",
				));
			} else {
				latest_change_set.network = Some(network);
				write_bdk_wallet_network(
					&network,
					Arc::clone(&persister.kv_store),
					Arc::clone(&persister.logger),
					persister.address_type,
				)?;
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
			write_bdk_wallet_indexer(
				&latest_change_set.indexer,
				Arc::clone(&persister.kv_store),
				Arc::clone(&persister.logger),
				persister.address_type,
			)?;
		}

		if !change_set.tx_graph.is_empty() {
			latest_change_set.tx_graph.merge(change_set.tx_graph.clone());
			write_bdk_wallet_tx_graph(
				&latest_change_set.tx_graph,
				Arc::clone(&persister.kv_store),
				Arc::clone(&persister.logger),
				persister.address_type,
			)?;
		}

		if !change_set.local_chain.is_empty() {
			latest_change_set.local_chain.merge(change_set.local_chain.clone());
			write_bdk_wallet_local_chain(
				&latest_change_set.local_chain,
				Arc::clone(&persister.kv_store),
				Arc::clone(&persister.logger),
				persister.address_type,
			)?;
		}

		Ok(())
	}
}
