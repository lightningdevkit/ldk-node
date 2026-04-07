// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.
#![allow(dead_code)] // TODO: Temporal warning silencer. Will be removed in later commit.

use crate::io::utils::check_namespace_key_validity;
use crate::logger::{LdkLogger, Logger};
use crate::types::DynStore;

use lightning::util::persist::{
	KVStore, KVStoreSync, NETWORK_GRAPH_PERSISTENCE_KEY,
	NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE, NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
	SCORER_PERSISTENCE_KEY, SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
};
use lightning::{io, log_error};

use std::future::Future;
use std::sync::Arc;

/// A 3-tiered [`KVStore`]/[`KVStoreSync`] implementation that routes data across
/// storage backends that may be local or remote:
/// - a primary store for durable, authoritative persistence,
/// - an optional backup store that maintains an additional durable copy of
///   primary-backed data, and
/// - an optional ephemeral store for non-critical, rebuildable cached data.
///
/// When a backup store is configured, writes and removals for primary-backed data
/// are issued to the primary and backup stores concurrently and only succeed once
/// both stores complete successfully.
///
/// Reads and lists do not consult the backup store during normal operation.
/// Ephemeral data is read from and written to the ephemeral store when configured.
///
/// Note that dual-store writes and removals are not atomic across the primary and
/// backup stores. If one store succeeds and the other fails, the operation
/// returns an error even though one store may already reflect the change.
pub(crate) struct TierStore {
	inner: Arc<TierStoreInner>,
}

impl TierStore {
	pub fn new(primary_store: Arc<DynStore>, logger: Arc<Logger>) -> Self {
		let inner = Arc::new(TierStoreInner::new(primary_store, Arc::clone(&logger)));

		Self { inner }
	}

	/// Configures a backup store for primary-backed data.
	///
	/// Once set, writes and removals targeting the primary tier succeed only if both
	/// the primary and backup stores succeed. The two operations are issued
	/// concurrently, and any failure is returned to the caller.
	///
	/// Note: dual-store writes/removals are not atomic. An error may be returned
	/// after the primary store has already been updated if the backup store fails.
	///
	/// The backup store is not consulted for normal reads or lists.
	pub fn set_backup_store(&mut self, backup: Arc<DynStore>) {
		debug_assert_eq!(Arc::strong_count(&self.inner), 1);

		let inner = Arc::get_mut(&mut self.inner).expect(
			"TierStore should not be shared during configuration. No other references should exist",
		);

		inner.backup_store = Some(backup);
	}

	/// Configures the ephemeral store for non-critical, rebuildable data.
	///
	/// When configured, selected cache-like data is routed to this store instead of
	/// the primary store.
	pub fn set_ephemeral_store(&mut self, ephemeral: Arc<DynStore>) {
		debug_assert_eq!(Arc::strong_count(&self.inner), 1);

		let inner = Arc::get_mut(&mut self.inner).expect(
			"TierStore should not be shared during configuration. No other references should exist",
		);

		inner.ephemeral_store = Some(ephemeral);
	}
}

impl KVStore for TierStore {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> impl Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
		let inner = Arc::clone(&self.inner);

		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();

		async move { inner.read_internal(primary_namespace, secondary_namespace, key).await }
	}

	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
		let inner = Arc::clone(&self.inner);

		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();

		async move { inner.write_internal(primary_namespace, secondary_namespace, key, buf).await }
	}

	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
		let inner = Arc::clone(&self.inner);

		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();

		async move { inner.remove_internal(primary_namespace, secondary_namespace, key, lazy).await }
	}

	fn list(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> impl Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
		let inner = Arc::clone(&self.inner);

		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();

		async move { inner.list_internal(primary_namespace, secondary_namespace).await }
	}
}

impl KVStoreSync for TierStore {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<Vec<u8>> {
		self.inner.read_internal_sync(
			primary_namespace.to_string(),
			secondary_namespace.to_string(),
			key.to_string(),
		)
	}

	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> io::Result<()> {
		self.inner.write_internal_sync(
			primary_namespace.to_string(),
			secondary_namespace.to_string(),
			key.to_string(),
			buf,
		)
	}

	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> io::Result<()> {
		self.inner.remove_internal_sync(
			primary_namespace.to_string(),
			secondary_namespace.to_string(),
			key.to_string(),
			lazy,
		)
	}

	fn list(&self, primary_namespace: &str, secondary_namespace: &str) -> io::Result<Vec<String>> {
		self.inner
			.list_internal_sync(primary_namespace.to_string(), secondary_namespace.to_string())
	}
}

struct TierStoreInner {
	/// The authoritative store for durable data.
	primary_store: Arc<DynStore>,
	/// The store used for non-critical, rebuildable cached data.
	ephemeral_store: Option<Arc<DynStore>>,
	/// An optional second durable store for primary-backed data.
	backup_store: Option<Arc<DynStore>>,
	logger: Arc<Logger>,
}

impl TierStoreInner {
	/// Creates a tier store with the primary data store.
	pub fn new(primary_store: Arc<DynStore>, logger: Arc<Logger>) -> Self {
		Self { primary_store, ephemeral_store: None, backup_store: None, logger }
	}

	/// Reads from the primary data store.
	async fn read_primary(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<Vec<u8>> {
		match KVStore::read(
			self.primary_store.as_ref(),
			primary_namespace,
			secondary_namespace,
			key,
		)
		.await
		{
			Ok(data) => Ok(data),
			Err(e) => {
				log_error!(
					self.logger,
					"Failed to read from primary store for key {}/{}/{}: {}.",
					primary_namespace,
					secondary_namespace,
					key,
					e
				);
				Err(e)
			},
		}
	}

	fn read_primary_sync(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<Vec<u8>> {
		match KVStoreSync::read(
			self.primary_store.as_ref(),
			primary_namespace,
			secondary_namespace,
			key,
		) {
			Ok(data) => Ok(data),
			Err(e) => {
				log_error!(
					self.logger,
					"Failed to read from primary store for key {}/{}/{}: {}.",
					primary_namespace,
					secondary_namespace,
					key,
					e
				);
				Err(e)
			},
		}
	}

	/// Lists keys from the primary data store.
	async fn list_primary(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> io::Result<Vec<String>> {
		match KVStore::list(self.primary_store.as_ref(), primary_namespace, secondary_namespace)
			.await
		{
			Ok(keys) => Ok(keys),
			Err(e) => {
				log_error!(
					self.logger,
					"Failed to list from primary store for namespace {}/{}: {}.",
					primary_namespace,
					secondary_namespace,
					e
				);
				Err(e)
			},
		}
	}

	fn list_primary_sync(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> io::Result<Vec<String>> {
		match KVStoreSync::list(self.primary_store.as_ref(), primary_namespace, secondary_namespace)
		{
			Ok(keys) => Ok(keys),
			Err(e) => {
				log_error!(
					self.logger,
					"Failed to list keys in namespace {}/{} from primary store: {}.",
					primary_namespace,
					secondary_namespace,
					e
				);
				Err(e)
			},
		}
	}

	async fn write_primary_backup_async(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> io::Result<()> {
		let primary_fut = KVStore::write(
			self.primary_store.as_ref(),
			primary_namespace,
			secondary_namespace,
			key,
			buf.clone(),
		);

		if let Some(backup_store) = self.backup_store.as_ref() {
			let backup_fut = KVStore::write(
				backup_store.as_ref(),
				primary_namespace,
				secondary_namespace,
				key,
				buf,
			);

			let (primary_res, backup_res) = tokio::join!(primary_fut, backup_fut);

			self.handle_primary_backup_results(
				"write",
				primary_namespace,
				secondary_namespace,
				key,
				primary_res,
				backup_res,
			)
		} else {
			primary_fut.await
		}
	}

	fn write_primary_backup_sync(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> io::Result<()> {
		if let Some(backup_store) = self.backup_store.as_ref() {
			let primary_res = KVStoreSync::write(
				self.primary_store.as_ref(),
				primary_namespace,
				secondary_namespace,
				key,
				buf.clone(),
			);
			let backup_res = KVStoreSync::write(
				backup_store.as_ref(),
				primary_namespace,
				secondary_namespace,
				key,
				buf,
			);

			self.handle_primary_backup_results(
				"write",
				primary_namespace,
				secondary_namespace,
				key,
				primary_res,
				backup_res,
			)
		} else {
			KVStoreSync::write(
				self.primary_store.as_ref(),
				primary_namespace,
				secondary_namespace,
				key,
				buf,
			)
		}
	}

	async fn remove_primary_backup_async(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> io::Result<()> {
		let primary_fut = KVStore::remove(
			self.primary_store.as_ref(),
			primary_namespace,
			secondary_namespace,
			key,
			lazy,
		);

		if let Some(backup_store) = self.backup_store.as_ref() {
			let backup_fut = KVStore::remove(
				backup_store.as_ref(),
				primary_namespace,
				secondary_namespace,
				key,
				lazy,
			);

			let (primary_res, backup_res) = tokio::join!(primary_fut, backup_fut);

			self.handle_primary_backup_results(
				"removal",
				primary_namespace,
				secondary_namespace,
				key,
				primary_res,
				backup_res,
			)
		} else {
			primary_fut.await
		}
	}

	fn remove_primary_backup_sync(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> io::Result<()> {
		if let Some(backup_store) = self.backup_store.as_ref() {
			let primary_res = KVStoreSync::remove(
				self.primary_store.as_ref(),
				primary_namespace,
				secondary_namespace,
				key,
				lazy,
			);
			let backup_res = KVStoreSync::remove(
				backup_store.as_ref(),
				primary_namespace,
				secondary_namespace,
				key,
				lazy,
			);

			self.handle_primary_backup_results(
				"removal",
				primary_namespace,
				secondary_namespace,
				key,
				primary_res,
				backup_res,
			)
		} else {
			KVStoreSync::remove(
				self.primary_store.as_ref(),
				primary_namespace,
				secondary_namespace,
				key,
				lazy,
			)
		}
	}

	async fn read_internal(
		&self, primary_namespace: String, secondary_namespace: String, key: String,
	) -> io::Result<Vec<u8>> {
		check_namespace_key_validity(
			primary_namespace.as_str(),
			secondary_namespace.as_str(),
			Some(key.as_str()),
			"read",
		)?;

		if let Some(eph_store) =
			self.ephemeral_store(&primary_namespace, &secondary_namespace, &key)
		{
			// We don't retry ephemeral-store reads here. Local failures are treated as
			// terminal for this access path rather than falling back to another store.
			KVStore::read(eph_store.as_ref(), &primary_namespace, &secondary_namespace, &key).await
		} else {
			self.read_primary(&primary_namespace, &secondary_namespace, &key).await
		}
	}

	fn read_internal_sync(
		&self, primary_namespace: String, secondary_namespace: String, key: String,
	) -> io::Result<Vec<u8>> {
		check_namespace_key_validity(
			primary_namespace.as_str(),
			secondary_namespace.as_str(),
			Some(key.as_str()),
			"read",
		)?;

		if let Some(eph_store) =
			self.ephemeral_store(&primary_namespace, &secondary_namespace, &key)
		{
			KVStoreSync::read(eph_store.as_ref(), &primary_namespace, &secondary_namespace, &key)
		} else {
			self.read_primary_sync(&primary_namespace, &secondary_namespace, &key)
		}
	}

	async fn write_internal(
		&self, primary_namespace: String, secondary_namespace: String, key: String, buf: Vec<u8>,
	) -> io::Result<()> {
		check_namespace_key_validity(
			primary_namespace.as_str(),
			secondary_namespace.as_str(),
			Some(key.as_str()),
			"write",
		)?;

		if let Some(eph_store) =
			self.ephemeral_store(&primary_namespace, &secondary_namespace, &key)
		{
			KVStore::write(
				eph_store.as_ref(),
				primary_namespace.as_str(),
				secondary_namespace.as_str(),
				key.as_str(),
				buf,
			)
			.await
		} else {
			self.write_primary_backup_async(
				primary_namespace.as_str(),
				secondary_namespace.as_str(),
				key.as_str(),
				buf,
			)
			.await
		}
	}

	fn write_internal_sync(
		&self, primary_namespace: String, secondary_namespace: String, key: String, buf: Vec<u8>,
	) -> io::Result<()> {
		check_namespace_key_validity(
			primary_namespace.as_str(),
			secondary_namespace.as_str(),
			Some(key.as_str()),
			"write",
		)?;

		if let Some(ephemeral_store) =
			self.ephemeral_store(&primary_namespace, &secondary_namespace, &key)
		{
			KVStoreSync::write(
				ephemeral_store.as_ref(),
				primary_namespace.as_str(),
				secondary_namespace.as_str(),
				key.as_str(),
				buf,
			)
		} else {
			self.write_primary_backup_sync(
				primary_namespace.as_str(),
				secondary_namespace.as_str(),
				key.as_str(),
				buf,
			)
		}
	}

	async fn remove_internal(
		&self, primary_namespace: String, secondary_namespace: String, key: String, lazy: bool,
	) -> io::Result<()> {
		check_namespace_key_validity(
			primary_namespace.as_str(),
			secondary_namespace.as_str(),
			Some(key.as_str()),
			"remove",
		)?;

		if let Some(eph_store) =
			self.ephemeral_store(&primary_namespace, &secondary_namespace, &key)
		{
			KVStore::remove(
				eph_store.as_ref(),
				primary_namespace.as_str(),
				secondary_namespace.as_str(),
				key.as_str(),
				lazy,
			)
			.await
		} else {
			self.remove_primary_backup_async(
				primary_namespace.as_str(),
				secondary_namespace.as_str(),
				key.as_str(),
				lazy,
			)
			.await
		}
	}

	fn remove_internal_sync(
		&self, primary_namespace: String, secondary_namespace: String, key: String, lazy: bool,
	) -> io::Result<()> {
		check_namespace_key_validity(
			primary_namespace.as_str(),
			secondary_namespace.as_str(),
			Some(key.as_str()),
			"remove",
		)?;

		if let Some(ephemeral_store) =
			self.ephemeral_store(&primary_namespace, &secondary_namespace, &key)
		{
			KVStoreSync::remove(
				ephemeral_store.as_ref(),
				primary_namespace.as_str(),
				secondary_namespace.as_str(),
				key.as_str(),
				lazy,
			)
		} else {
			self.remove_primary_backup_sync(
				primary_namespace.as_str(),
				secondary_namespace.as_str(),
				key.as_str(),
				lazy,
			)
		}
	}

	async fn list_internal(
		&self, primary_namespace: String, secondary_namespace: String,
	) -> io::Result<Vec<String>> {
		check_namespace_key_validity(
			primary_namespace.as_str(),
			secondary_namespace.as_str(),
			None,
			"list",
		)?;

		match (primary_namespace.as_str(), secondary_namespace.as_str()) {
			(
				NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
				NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
			)
			| (SCORER_PERSISTENCE_PRIMARY_NAMESPACE, _) => {
				if let Some(eph_store) = self.ephemeral_store.as_ref() {
					// We don't retry ephemeral-store lists here. Local failures are treated as
					// terminal for this access path rather than falling back to another store.
					KVStore::list(eph_store.as_ref(), &primary_namespace, &secondary_namespace)
						.await
				} else {
					self.list_primary(&primary_namespace, &secondary_namespace).await
				}
			},
			_ => self.list_primary(&primary_namespace, &secondary_namespace).await,
		}
	}

	fn list_internal_sync(
		&self, primary_namespace: String, secondary_namespace: String,
	) -> io::Result<Vec<String>> {
		check_namespace_key_validity(
			primary_namespace.as_str(),
			secondary_namespace.as_str(),
			None,
			"list",
		)?;

		match (primary_namespace.as_str(), secondary_namespace.as_str()) {
			(
				NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
				NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
			)
			| (SCORER_PERSISTENCE_PRIMARY_NAMESPACE, _) => {
				if let Some(ephemeral_store) = self.ephemeral_store.as_ref() {
					KVStoreSync::list(
						ephemeral_store.as_ref(),
						&primary_namespace,
						&secondary_namespace,
					)
				} else {
					self.list_primary_sync(&primary_namespace, &secondary_namespace)
				}
			},
			_ => self.list_primary_sync(&primary_namespace, &secondary_namespace),
		}
	}

	fn ephemeral_store(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> Option<&Arc<DynStore>> {
		self.ephemeral_store
			.as_ref()
			.filter(|_s| is_ephemeral_cached_key(primary_namespace, secondary_namespace, key))
	}

	fn handle_primary_backup_results(
		&self, op: &str, primary_namespace: &str, secondary_namespace: &str, key: &str,
		primary_res: io::Result<()>, backup_res: io::Result<()>,
	) -> io::Result<()> {
		match (primary_res, backup_res) {
			(Ok(()), Ok(())) => Ok(()),
			(Err(primary_err), Ok(())) => Err(primary_err),
			(Ok(()), Err(backup_err)) => Err(backup_err),
			(Err(primary_err), Err(backup_err)) => {
				log_error!(
					self.logger,
					"Primary and backup {}s both failed for key {}/{}/{}: primary={}, backup={}",
					op,
					primary_namespace,
					secondary_namespace,
					key,
					primary_err,
					backup_err
				);
				Err(primary_err)
			},
		}
	}
}

fn is_ephemeral_cached_key(pn: &str, sn: &str, key: &str) -> bool {
	matches!(
		(pn, sn, key),
		(NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE, _, NETWORK_GRAPH_PERSISTENCE_KEY)
			| (SCORER_PERSISTENCE_PRIMARY_NAMESPACE, _, SCORER_PERSISTENCE_KEY)
	)
}

#[cfg(test)]
mod tests {
	use std::panic::RefUnwindSafe;
	use std::path::PathBuf;
	use std::sync::Arc;

	use lightning::util::logger::Level;
	use lightning::util::persist::{
		CHANNEL_MANAGER_PERSISTENCE_KEY, CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
		CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
	};
	use lightning_persister::fs_store::v1::FilesystemStore;

	use crate::io::test_utils::{do_read_write_remove_list_persist, random_storage_path};
	use crate::io::tier_store::TierStore;
	use crate::logger::Logger;
	use crate::types::DynStore;
	use crate::types::DynStoreWrapper;

	use super::*;

	impl RefUnwindSafe for TierStore {}

	struct CleanupDir(PathBuf);
	impl Drop for CleanupDir {
		fn drop(&mut self) {
			let _ = std::fs::remove_dir_all(&self.0);
		}
	}

	fn setup_tier_store(primary_store: Arc<DynStore>, logger: Arc<Logger>) -> TierStore {
		TierStore::new(primary_store, logger)
	}

	#[test]
	fn write_read_list_remove() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());

		let _cleanup = CleanupDir(base_dir.clone());

		let primary_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStore::new(base_dir.join("primary"))));
		let tier = setup_tier_store(primary_store, logger);

		do_read_write_remove_list_persist(&tier);
	}

	#[test]
	fn ephemeral_routing() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());

		let _cleanup = CleanupDir(base_dir.clone());

		let primary_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStore::new(base_dir.join("primary"))));
		let mut tier = setup_tier_store(Arc::clone(&primary_store), logger);

		let ephemeral_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStore::new(base_dir.join("ephemeral"))));
		tier.set_ephemeral_store(Arc::clone(&ephemeral_store));

		let data = vec![42u8; 32];

		KVStoreSync::write(
			&tier,
			NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_KEY,
			data.clone(),
		)
		.unwrap();

		KVStoreSync::write(
			&tier,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
			data.clone(),
		)
		.unwrap();

		let primary_read_ng = KVStoreSync::read(
			&*primary_store,
			NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_KEY,
		);
		let ephemeral_read_ng = KVStoreSync::read(
			&*ephemeral_store,
			NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_KEY,
		);

		let primary_read_cm = KVStoreSync::read(
			&*primary_store,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
		);
		let ephemeral_read_cm = KVStoreSync::read(
			&*ephemeral_store,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
		);

		assert!(primary_read_ng.is_err());
		assert_eq!(ephemeral_read_ng.unwrap(), data);

		assert!(ephemeral_read_cm.is_err());
		assert_eq!(primary_read_cm.unwrap(), data);
	}

	#[test]
	fn backup_write_is_part_of_success_path() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());

		let _cleanup = CleanupDir(base_dir.clone());

		let primary_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStore::new(base_dir.join("primary"))));
		let mut tier = setup_tier_store(Arc::clone(&primary_store), logger);

		let backup_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStore::new(base_dir.join("backup"))));
		tier.set_backup_store(Arc::clone(&backup_store));

		let data = vec![42u8; 32];

		KVStoreSync::write(
			&tier,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
			data.clone(),
		)
		.unwrap();

		let primary_read = KVStoreSync::read(
			&*primary_store,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
		);
		let backup_read = KVStoreSync::read(
			&*backup_store,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
		);

		assert_eq!(primary_read.unwrap(), data);
		assert_eq!(backup_read.unwrap(), data);
	}

	#[test]
	fn backup_remove_is_part_of_success_path() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());

		let _cleanup = CleanupDir(base_dir.clone());

		let primary_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStore::new(base_dir.join("primary"))));
		let mut tier = setup_tier_store(Arc::clone(&primary_store), logger);

		let backup_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStore::new(base_dir.join("backup"))));
		tier.set_backup_store(Arc::clone(&backup_store));

		let data = vec![42u8; 32];
		let key = CHANNEL_MANAGER_PERSISTENCE_KEY;

		KVStoreSync::write(
			&tier,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			key,
			data,
		)
		.unwrap();

		KVStoreSync::remove(
			&tier,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			key,
			true,
		)
		.unwrap();

		let primary_read = KVStoreSync::read(
			&*primary_store,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			key,
		);
		let backup_read = KVStoreSync::read(
			&*backup_store,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			key,
		);

		assert!(primary_read.is_err());
		assert!(backup_read.is_err());
	}
}
