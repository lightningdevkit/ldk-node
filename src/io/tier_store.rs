// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::io::utils::check_namespace_key_validity;
use crate::logger::{LdkLogger, Logger};
use crate::runtime::Runtime;
use crate::types::DynStore;

use lightning::io;
use lightning::util::persist::{
	KVStore, KVStoreSync, NETWORK_GRAPH_PERSISTENCE_KEY,
	NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE, NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
	SCORER_PERSISTENCE_KEY, SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
};
use lightning::{log_debug, log_error, log_warn};

use tokio::sync::mpsc::{self, error::TrySendError};

use std::future::Future;
use std::sync::Arc;

// todo(enigbe): Uncertain about appropriate queue size and if this would need
// configuring.
#[cfg(not(test))]
const BACKUP_QUEUE_CAPACITY: usize = 100;
#[cfg(test)]
const BACKUP_QUEUE_CAPACITY: usize = 5;

/// A 3-tiered [`KVStoreSync`] implementation that manages data across
/// three distinct storage locations, i.e. primary (preferably remote)
/// store for all critical data, optional ephemeral (local) store for
/// non-critical and easily rebuildable data, and backup (preferably
/// local) to lazily backup the primary store for disaster recovery
/// scenarios.
pub(crate) struct TierStore {
	inner: Arc<TierStoreInner>,
	runtime: Arc<Runtime>,
	logger: Arc<Logger>,
}

impl TierStore {
	pub fn new(primary_store: Arc<DynStore>, runtime: Arc<Runtime>, logger: Arc<Logger>) -> Self {
		let inner = Arc::new(TierStoreInner::new(primary_store, Arc::clone(&logger)));

		Self { inner, runtime, logger }
	}

	/// Configures the local backup store for disaster recovery.
	///
	/// This store serves as a local copy of the critical data for disaster
	/// recovery scenarios. When configured, this method also spawns a background
	/// task that asynchronously processes backup writes and removals to avoid
	/// blocking primary store operations.
	///
	/// The backup operates on a best-effort basis:
	/// - Writes are queued asynchronously (non-blocking)
	/// - No retry logic (We assume local store is unlikely to have transient failures).
	/// - Failures are logged but don't propagate to all the way to caller.
	pub fn set_backup_store(&mut self, backup: Arc<DynStore>) {
		let (tx, rx) = mpsc::channel::<BackupOp>(BACKUP_QUEUE_CAPACITY);

		let backup_clone = Arc::clone(&backup);
		let logger = Arc::clone(&self.logger);

		self.runtime.spawn_background_task(Self::process_backup_operation(
			rx,
			backup_clone,
			logger,
		));

		debug_assert_eq!(Arc::strong_count(&self.inner), 1);

		let inner = Arc::get_mut(&mut self.inner).expect(
			"TierStore should not be shared during configuration. No other references should exist",
		);

		inner.backup_store = Some(backup);
		inner.backup_sender = Some(tx);
	}

	async fn process_backup_operation(
		mut receiver: mpsc::Receiver<BackupOp>, backup_store: Arc<DynStore>, logger: Arc<Logger>,
	) {
		while let Some(op) = receiver.recv().await {
			match Self::apply_backup_operation(&op, &backup_store).await {
				Ok(_) => {},
				Err(e) => {
					log_error!(
						logger,
						"Backup failed permanently for key {}/{}/{}: {}",
						op.primary_namespace(),
						op.secondary_namespace(),
						op.key(),
						e
					);
				},
			}
		}
	}

	async fn apply_backup_operation(op: &BackupOp, store: &Arc<DynStore>) -> io::Result<()> {
		match op {
			BackupOp::Write { primary_namespace, secondary_namespace, key, data } => {
				KVStore::write(
					store.as_ref(),
					primary_namespace,
					secondary_namespace,
					key,
					data.clone(),
				)
				.await
			},
			BackupOp::Remove { primary_namespace, secondary_namespace, key, lazy } => {
				KVStore::remove(store.as_ref(), primary_namespace, secondary_namespace, key, *lazy)
					.await
			},
		}
	}

	/// Configures the local store for non-critical data storage.
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
		self.runtime.block_on(self.inner.read_internal(
			primary_namespace.to_string(),
			secondary_namespace.to_string(),
			key.to_string(),
		))
	}

	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> io::Result<()> {
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();

		self.runtime.block_on(self.inner.write_internal(
			primary_namespace,
			secondary_namespace,
			key,
			buf,
		))
	}

	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> io::Result<()> {
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();

		self.runtime.block_on(self.inner.remove_internal(
			primary_namespace,
			secondary_namespace,
			key,
			lazy,
		))
	}

	fn list(&self, primary_namespace: &str, secondary_namespace: &str) -> io::Result<Vec<String>> {
		self.runtime.block_on(
			self.inner
				.list_internal(primary_namespace.to_string(), secondary_namespace.to_string()),
		)
	}
}

struct TierStoreInner {
	/// For local or remote data.
	primary_store: Arc<DynStore>,
	/// For local non-critical/ephemeral data.
	ephemeral_store: Option<Arc<DynStore>>,
	/// For redundancy (disaster recovery).
	backup_store: Option<Arc<DynStore>>,
	backup_sender: Option<mpsc::Sender<BackupOp>>,
	logger: Arc<Logger>,
}

impl TierStoreInner {
	/// Creates a tier store with the primary (remote) data store.
	pub fn new(primary_store: Arc<DynStore>, logger: Arc<Logger>) -> Self {
		Self {
			primary_store,
			ephemeral_store: None,
			backup_store: None,
			backup_sender: None,
			logger,
		}
	}

	/// Queues data for asynchronous backup/write to the configured backup store.
	///
	/// We perform a non-blocking send to avoid impacting primary storage operations.
	/// This is a no-op if backup store is not configured.
	///
	/// ## Returns
	/// - `Ok(())`: Backup was successfully queued or no backup is configured
	/// - `Err(WouldBlock)`: Backup queue is full - data was not queued
	/// - `Err(BrokenPipe)`: Backup queue is no longer available
	fn enqueue_backup_write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> io::Result<()> {
		if let Some(backup_sender) = &self.backup_sender {
			let backup_res = backup_sender.try_send(BackupOp::Write {
				primary_namespace: primary_namespace.to_string(),
				secondary_namespace: secondary_namespace.to_string(),
				key: key.to_string(),
				data: buf,
			});
			if let Err(e) = backup_res {
				match e {
					// Assuming the channel is only full for a short time, should we explore
					// retrying here to add some resiliency?
					TrySendError::Full(op) => {
						log_warn!(
							self.logger,
							"Backup queue is full. Cannot write data for key: {}/{}/{}",
							op.primary_namespace(),
							op.secondary_namespace(),
							op.key()
						);
						let e = io::Error::new(
							io::ErrorKind::WouldBlock,
							"Backup queue is currently full.",
						);
						return Err(e);
					},
					TrySendError::Closed(op) => {
						log_error!(
							self.logger,
							"Backup queue is closed. Cannot write data for key: {}/{}/{}",
							op.primary_namespace(),
							op.secondary_namespace(),
							op.key()
						);
						let e =
							io::Error::new(io::ErrorKind::BrokenPipe, "Backup queue is closed.");
						return Err(e);
					},
				}
			}
		}
		Ok(())
	}

	/// Queues the removal of data from the configured backup store.
	///
	/// We perform a non-blocking send to avoid impacting primary storage operations.
	/// This is a no-op if backup store is not configured.
	///
	/// # Returns
	/// - `Ok(())`: Backup was successfully queued or no backup is configured
	/// - `Err(WouldBlock)`: Backup queue is full - data was not queued
	/// - `Err(BrokenPipe)`: Backup system is no longer available
	fn enqueue_backup_remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> io::Result<()> {
		if let Some(backup_sender) = &self.backup_sender {
			let removal_res = backup_sender.try_send(BackupOp::Remove {
				primary_namespace: primary_namespace.to_string(),
				secondary_namespace: secondary_namespace.to_string(),
				key: key.to_string(),
				lazy,
			});
			if let Err(e) = removal_res {
				match e {
					TrySendError::Full(op) => {
						log_warn!(
							self.logger,
							"Backup queue is full. Cannot remove data for key: {}/{}/{}",
							op.primary_namespace(),
							op.secondary_namespace(),
							op.key()
						);
						let e = io::Error::new(
							io::ErrorKind::WouldBlock,
							"Backup queue is currently full.",
						);
						return Err(e);
					},
					TrySendError::Closed(op) => {
						log_error!(
							self.logger,
							"Backup queue is closed. Cannot remove data for key: {}/{}/{}",
							op.primary_namespace(),
							op.secondary_namespace(),
							op.key()
						);
						let e =
							io::Error::new(io::ErrorKind::BrokenPipe, "Backup queue is closed.");
						return Err(e);
					},
				}
			}
		}
		Ok(())
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

	async fn primary_write_then_schedule_backup(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> io::Result<()> {
		match KVStore::write(
			self.primary_store.as_ref(),
			primary_namespace,
			secondary_namespace,
			key,
			buf.clone(),
		)
		.await
		{
			Ok(()) => {
				if let Err(e) =
					self.enqueue_backup_write(primary_namespace, secondary_namespace, key, buf)
				{
					// We don't propagate backup errors here, opting to log only.
					log_warn!(
						self.logger,
						"Failed to queue backup write for key: {}/{}/{}. Error: {}",
						primary_namespace,
						secondary_namespace,
						key,
						e
					)
				}

				Ok(())
			},
			Err(e) => {
				log_debug!(
					self.logger,
					"Skipping backup write due to primary write failure for key: {}/{}/{}.",
					primary_namespace,
					secondary_namespace,
					key
				);
				Err(e)
			},
		}
	}

	async fn primary_remove_then_schedule_backup(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> io::Result<()> {
		match KVStore::remove(
			self.primary_store.as_ref(),
			primary_namespace,
			secondary_namespace,
			key,
			lazy,
		)
		.await
		{
			Ok(()) => {
				if let Err(e) =
					self.enqueue_backup_remove(primary_namespace, secondary_namespace, key, lazy)
				{
					// We don't propagate backup errors here, opting to silently log.
					log_warn!(
						self.logger,
						"Failed to queue backup removal for key: {}/{}/{}. Error: {}",
						primary_namespace,
						secondary_namespace,
						key,
						e
					)
				}

				Ok(())
			},
			Err(e) => {
				log_debug!(
					self.logger,
					"Skipping backup removal due to primary removal failure for key: {}/{}/{}.",
					primary_namespace,
					secondary_namespace,
					key
				);
				Err(e)
			},
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

		if let Some(eph_store) = self
			.ephemeral_store
			.as_ref()
			.filter(|_s| is_ephemeral_cached_key(&primary_namespace, &secondary_namespace, &key))
		{
			// We only try once here (without retry logic) because local failure might be indicative
			// of a more serious issue (e.g. full memory, memory corruption, permissions change) that
			// do not self-resolve such that retrying would negate the latency benefits.

			// The following questions remain:
			// 1. Are there situations where local transient errors may warrant a retry?
			// 2. Can we reliably identify/detect these transient errors?
			// 3. Should we fall back to the primary or backup stores in the event of any error?
			KVStore::read(eph_store.as_ref(), &primary_namespace, &secondary_namespace, &key).await
		} else {
			self.read_primary(&primary_namespace, &secondary_namespace, &key).await
		}
	}

	async fn write_internal(
		&self, primary_namespace: String, secondary_namespace: String, key: String, buf: Vec<u8>,
	) -> io::Result<()> {
		if let Some(eph_store) = self
			.ephemeral_store
			.as_ref()
			.filter(|_s| is_ephemeral_cached_key(&primary_namespace, &secondary_namespace, &key))
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
			self.primary_write_then_schedule_backup(
				primary_namespace.as_str(),
				secondary_namespace.as_str(),
				key.as_str(),
				buf,
			)
			.await
		}
	}

	async fn remove_internal(
		&self, primary_namespace: String, secondary_namespace: String, key: String, lazy: bool,
	) -> io::Result<()> {
		if let Some(eph_store) = self
			.ephemeral_store
			.as_ref()
			.filter(|_s| is_ephemeral_cached_key(&primary_namespace, &secondary_namespace, &key))
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
			self.primary_remove_then_schedule_backup(
				primary_namespace.as_str(),
				secondary_namespace.as_str(),
				key.as_str(),
				lazy,
			)
			.await
		}
	}

	async fn list_internal(
		&self, primary_namespace: String, secondary_namespace: String,
	) -> io::Result<Vec<String>> {
		match (primary_namespace.as_str(), secondary_namespace.as_str()) {
			(
				NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
				NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
			)
			| (SCORER_PERSISTENCE_PRIMARY_NAMESPACE, _) => {
				if let Some(eph_store) = self.ephemeral_store.as_ref() {
					KVStore::list(eph_store.as_ref(), &primary_namespace, &secondary_namespace)
						.await
				} else {
					self.list_primary(&primary_namespace, &secondary_namespace).await
				}
			},
			_ => self.list_primary(&primary_namespace, &secondary_namespace).await,
		}
	}
}

enum BackupOp {
	Write { primary_namespace: String, secondary_namespace: String, key: String, data: Vec<u8> },
	Remove { primary_namespace: String, secondary_namespace: String, key: String, lazy: bool },
}

impl BackupOp {
	fn primary_namespace(&self) -> &str {
		match self {
			BackupOp::Write { primary_namespace, .. }
			| BackupOp::Remove { primary_namespace, .. } => primary_namespace,
		}
	}

	fn secondary_namespace(&self) -> &str {
		match self {
			BackupOp::Write { secondary_namespace, .. }
			| BackupOp::Remove { secondary_namespace, .. } => secondary_namespace,
		}
	}

	fn key(&self) -> &str {
		match self {
			BackupOp::Write { key, .. } | BackupOp::Remove { key, .. } => key,
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
	use std::thread;
	use std::time::Duration;

	use lightning::util::logger::Level;
	use lightning::util::persist::{
		CHANNEL_MANAGER_PERSISTENCE_KEY, CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
		CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
	};
	use lightning_persister::fs_store::FilesystemStore;

	use crate::io::test_utils::{do_read_write_remove_list_persist, random_storage_path};
	use crate::io::tier_store::TierStore;
	use crate::logger::Logger;
	use crate::runtime::Runtime;
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

	fn setup_tier_store(
		primary_store: Arc<DynStore>, logger: Arc<Logger>, runtime: Arc<Runtime>,
	) -> TierStore {
		TierStore::new(primary_store, runtime, logger)
	}

	#[test]
	fn write_read_list_remove() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());

		let runtime = Arc::new(Runtime::new(Arc::clone(&logger)).unwrap());
		let _cleanup = CleanupDir(base_dir.clone());

		let primary_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStore::new(base_dir.join("primary"))));
		let tier = setup_tier_store(primary_store, logger, runtime);

		do_read_write_remove_list_persist(&tier);
	}

	#[test]
	fn ephemeral_routing() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());

		let runtime = Arc::new(Runtime::new(Arc::clone(&logger)).unwrap());

		let _cleanup = CleanupDir(base_dir.clone());

		let primary_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStore::new(base_dir.join("primary"))));
		let mut tier = setup_tier_store(Arc::clone(&primary_store), logger, runtime);

		let ephemeral_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStore::new(base_dir.join("ephemeral"))));
		tier.set_ephemeral_store(Arc::clone(&ephemeral_store));

		let data = vec![42u8; 32];

		// Non-critical
		KVStoreSync::write(
			&tier,
			NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_KEY,
			data.clone(),
		)
		.unwrap();

		// Critical
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
	fn lazy_backup() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());
		let runtime = Arc::new(Runtime::new(Arc::clone(&logger)).unwrap());

		let _cleanup = CleanupDir(base_dir.clone());

		let primary_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStore::new(base_dir.join("primary"))));
		let mut tier = setup_tier_store(Arc::clone(&primary_store), logger, runtime);

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

		// Immediate read from backup should fail
		let backup_read_cm = KVStoreSync::read(
			&*backup_store,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
		);
		assert!(backup_read_cm.is_err());

		// Primary not blocked by backup hence immediate read should succeed
		let primary_read_cm = KVStoreSync::read(
			&*primary_store,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
		);
		assert_eq!(primary_read_cm.unwrap(), data);

		// Delayed read  from backup should succeed
		thread::sleep(Duration::from_millis(50));
		let backup_read_cm = KVStoreSync::read(
			&*backup_store,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
		);
		assert_eq!(backup_read_cm.unwrap(), data);
	}

	#[test]
	fn lazy_removal() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path.clone(), Level::Trace).unwrap());
		let runtime = Arc::new(Runtime::new(Arc::clone(&logger)).unwrap());

		let _cleanup = CleanupDir(base_dir.clone());

		let primary_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStore::new(base_dir.join("primary"))));
		let mut tier =
			setup_tier_store(Arc::clone(&primary_store), Arc::clone(&logger), Arc::clone(&runtime));

		let backup_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStore::new(base_dir.join("backup"))));
		tier.set_backup_store(Arc::clone(&backup_store));

		let data = vec![42u8; 32];

		let key = CHANNEL_MANAGER_PERSISTENCE_KEY;
		let write_result = KVStoreSync::write(
			&tier,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			key,
			data.clone(),
		);
		assert!(write_result.is_ok(), "Write should succeed");

		thread::sleep(Duration::from_millis(100));

		assert_eq!(
			KVStoreSync::read(
				&*backup_store,
				CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
				key,
			)
			.unwrap(),
			data
		);

		KVStoreSync::remove(
			&tier,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			key,
			true,
		)
		.unwrap();

		thread::sleep(Duration::from_millis(10));

		let res = KVStoreSync::read(
			&*backup_store,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			key,
		);

		assert!(res.is_err());
	}
}
