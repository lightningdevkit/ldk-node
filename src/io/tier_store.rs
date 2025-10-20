// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::io::utils::{check_namespace_key_validity, is_possibly_transient};
use crate::logger::{LdkLogger, Logger};
use crate::runtime::Runtime;
use crate::types::DynStore;

use lightning::util::persist::{
	KVStore, KVStoreSync, NETWORK_GRAPH_PERSISTENCE_KEY,
	NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE, NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
	SCORER_PERSISTENCE_KEY, SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
};
use lightning::{io, log_trace};
use lightning::{log_debug, log_error, log_info, log_warn};

use tokio::sync::mpsc::{self, error::TrySendError};

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// todo(enigbe): Uncertain about appropriate queue size and if this would need
// configuring.
#[cfg(not(test))]
const BACKUP_QUEUE_CAPACITY: usize = 100;
#[cfg(test)]
const BACKUP_QUEUE_CAPACITY: usize = 5;

const DEFAULT_INITIAL_RETRY_DELAY_MS: u16 = 10;
const DEFAULT_MAXIMUM_RETRY_DELAY_MS: u16 = 500;
const DEFAULT_BACKOFF_MULTIPLIER: f32 = 1.5;

/// Configuration for exponential backoff retry behavior.
#[derive(Debug, Copy, Clone)]
pub struct RetryConfig {
	/// The initial delay before the first retry attempt, in milliseconds.
	pub initial_retry_delay_ms: u16,
	/// The maximum delay between retry attempts, in milliseconds.
	pub maximum_delay_ms: u16,
	/// The multiplier applied to the delay after each retry attempt.
	///
	/// For example, a value of `2.0` doubles the delay after each failed retry.
	pub backoff_multiplier: f32,
}

impl Default for RetryConfig {
	fn default() -> Self {
		Self {
			initial_retry_delay_ms: DEFAULT_INITIAL_RETRY_DELAY_MS,
			maximum_delay_ms: DEFAULT_MAXIMUM_RETRY_DELAY_MS,
			backoff_multiplier: DEFAULT_BACKOFF_MULTIPLIER,
		}
	}
}

/// A 3-tiered [`KVStoreSync`] implementation that manages data across
/// three distinct storage locations, i.e. primary (preferably remote)
/// store for all critical data, optional ephemeral (local) store for
/// non-critical and easily rebuildable data, and backup (preferably
/// local) to lazily backup the primary store for disaster recovery
/// scenarios.
pub(crate) struct TierStore {
	inner: Arc<TierStoreInner>,
	next_version: AtomicU64,
	runtime: Arc<Runtime>,
	logger: Arc<Logger>,
}

impl TierStore {
	pub fn new(
		primary_store: Arc<DynStore>, runtime: Arc<Runtime>, logger: Arc<Logger>,
		retry_config: RetryConfig,
	) -> Self {
		let inner = Arc::new(TierStoreInner::new(primary_store, Arc::clone(&logger), retry_config));

		Self { inner, next_version: AtomicU64::new(1), runtime, logger }
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
			match Self::apply_backup_operation(&op, &backup_store) {
				Ok(_) => {
					log_trace!(
						logger,
						"Backup succeeded for key {}/{}/{}",
						op.primary_namespace(),
						op.secondary_namespace(),
						op.key()
					);
				},
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

	fn apply_backup_operation(op: &BackupOp, store: &Arc<DynStore>) -> io::Result<()> {
		match op {
			BackupOp::Write { primary_namespace, secondary_namespace, key, data } => {
				KVStoreSync::write(
					store.as_ref(),
					primary_namespace,
					secondary_namespace,
					key,
					data.clone(),
				)
			},
			BackupOp::Remove { primary_namespace, secondary_namespace, key, lazy } => {
				KVStoreSync::remove(
					store.as_ref(),
					primary_namespace,
					secondary_namespace,
					key,
					*lazy,
				)
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

	fn build_locking_key(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> String {
		if primary_namespace.is_empty() {
			key.to_owned()
		} else {
			format!("{}#{}#{}", primary_namespace, secondary_namespace, key)
		}
	}

	fn get_new_version_and_lock_ref(
		&self, locking_key: String,
	) -> (Arc<tokio::sync::Mutex<u64>>, u64) {
		let version = self.next_version.fetch_add(1, Ordering::Relaxed);
		if version == u64::MAX {
			panic!("TierStore version counter overflowed");
		}

		// Get a reference to the inner lock. We do this early so that the arc can double as an in-flight counter for
		// cleaning up unused locks.
		let inner_lock_ref = self.inner.get_inner_lock_ref(locking_key);

		(inner_lock_ref, version)
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

		let locking_key = self.build_locking_key(primary_namespace, secondary_namespace, key);
		let (inner_lock_ref, version) = self.get_new_version_and_lock_ref(locking_key.clone());

		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();

		async move {
			inner
				.write_internal(
					inner_lock_ref,
					locking_key,
					version,
					primary_namespace,
					secondary_namespace,
					key,
					buf,
				)
				.await
		}
	}

	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
		let inner = Arc::clone(&self.inner);

		let locking_key = self.build_locking_key(primary_namespace, secondary_namespace, key);
		let (inner_lock_ref, version) = self.get_new_version_and_lock_ref(locking_key.clone());

		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();

		async move {
			inner
				.remove_internal(
					inner_lock_ref,
					locking_key,
					version,
					primary_namespace,
					secondary_namespace,
					key,
					lazy,
				)
				.await
		}
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
		let locking_key = self.build_locking_key(primary_namespace, secondary_namespace, key);
		let (inner_lock_ref, version) = self.get_new_version_and_lock_ref(locking_key.clone());

		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();

		self.runtime.block_on(self.inner.write_internal(
			inner_lock_ref,
			locking_key,
			version,
			primary_namespace,
			secondary_namespace,
			key,
			buf,
		))
	}

	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> io::Result<()> {
		let locking_key = self.build_locking_key(primary_namespace, secondary_namespace, key);
		let (inner_lock_ref, version) = self.get_new_version_and_lock_ref(locking_key.clone());

		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();

		self.runtime.block_on(self.inner.remove_internal(
			inner_lock_ref,
			locking_key,
			version,
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

pub struct TierStoreInner {
	/// For remote data.
	primary_store: Arc<DynStore>,
	/// For local non-critical/ephemeral data.
	ephemeral_store: Option<Arc<DynStore>>,
	/// For redundancy (disaster recovery).
	backup_store: Option<Arc<DynStore>>,
	backup_sender: Option<mpsc::Sender<BackupOp>>,
	logger: Arc<Logger>,
	retry_config: RetryConfig,
	/// Per-key locks for the available data tiers, i.e. (primary, backup, ephemeral),
	/// that ensures we don't have concurrent writes to the same namespace/key.
	locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<u64>>>>,
}

impl TierStoreInner {
	/// Creates a tier store with the primary (remote) data store.
	pub fn new(
		primary_store: Arc<DynStore>, logger: Arc<Logger>, retry_config: RetryConfig,
	) -> Self {
		Self {
			primary_store,
			ephemeral_store: None,
			backup_store: None,
			backup_sender: None,
			logger,
			retry_config,
			locks: Mutex::new(HashMap::new()),
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

	/// Reads data from the backup store (if configured).
	fn read_from_backup(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<Vec<u8>> {
		if let Some(backup) = self.backup_store.as_ref() {
			KVStoreSync::read(backup.as_ref(), primary_namespace, secondary_namespace, key)
		} else {
			Err(io::Error::new(io::ErrorKind::NotFound, "Backup store not previously configured."))
		}
	}

	/// Lists keys from the given primary and secondary namespace pair from the backup
	/// store (if configured).
	fn list_from_backup(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> io::Result<Vec<String>> {
		if let Some(backup) = &self.backup_store {
			KVStoreSync::list(backup.as_ref(), primary_namespace, secondary_namespace)
		} else {
			Err(io::Error::new(io::ErrorKind::NotFound, "Backup store not previously configured."))
		}
	}

	/// Reads from the primary data store with basic retry logic, or falls back to backup.
	///
	/// For transient errors, retries up to a maximum delay time with exponential
	/// backoff. For any error (transient after exhaustion or non-transient), falls
	/// to the backup store (if configured).
	async fn read_primary_or_backup(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<Vec<u8>> {
		let mut delay = Duration::from_millis(self.retry_config.initial_retry_delay_ms as u64);
		let maximum_delay = Duration::from_millis(self.retry_config.maximum_delay_ms as u64);
		let mut tries = 0_u16;

		loop {
			match KVStore::read(
				self.primary_store.as_ref(),
				primary_namespace,
				secondary_namespace,
				key,
			)
			.await
			{
				Ok(data) => {
					log_info!(
						self.logger,
						"Read succeeded after {} retries for key: {}/{}/{}",
						tries,
						primary_namespace,
						secondary_namespace,
						key
					);
					return Ok(data);
				},

				Err(e) if is_possibly_transient(&e) && (delay < maximum_delay) => {
					log_warn!(
						self.logger,
						"Possible transient error reading key {}/{}/{} (attempt {}): {}. Retrying...",
						primary_namespace,
						secondary_namespace,
						key,
						tries + 1,
						e
					);
					tries += 1;
					tokio::time::sleep(delay).await;
					delay = std::cmp::min(
						delay.mul_f32(self.retry_config.backoff_multiplier),
						maximum_delay,
					);
				},

				Err(e) => {
					log_error!(self.logger, "Failed to read from primary store for key {}/{}/{}: {}. Falling back to backup.", 
                          primary_namespace, secondary_namespace, key, e);
					return self.read_from_backup(primary_namespace, secondary_namespace, key);
				},
			}
		}
	}

	/// Lists keys from the primary data store with retry logic, or falls back to backup.
	///
	/// For transient errors, retries up to a maximum delay time with exponential
	/// backoff. For any error (transient after exhaustion or non-transient), falls
	/// back to the backup store (if configured) for disaster recovery.
	async fn list_primary_or_backup(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> io::Result<Vec<String>> {
		let mut delay = Duration::from_millis(self.retry_config.initial_retry_delay_ms as u64);
		let maximum_delay = Duration::from_millis(self.retry_config.maximum_delay_ms as u64);
		let mut tries = 0_u16;

		loop {
			match KVStore::list(self.primary_store.as_ref(), primary_namespace, secondary_namespace)
				.await
			{
				Ok(keys) => {
					log_info!(
						self.logger,
						"List succeeded after {} retries for namespace: {}/{}",
						tries,
						primary_namespace,
						secondary_namespace
					);
					return Ok(keys);
				},
				Err(e) if is_possibly_transient(&e) && (delay < maximum_delay) => {
					log_warn!(
                    self.logger,
                    "Possible transient error listing namespace {}/{} (attempt {}): {}. Retrying...",
                    primary_namespace,
                    secondary_namespace,
                    tries + 1,
                    e
                );
					tries += 1;
					tokio::time::sleep(delay).await;
					delay = std::cmp::min(
						delay.mul_f32(self.retry_config.backoff_multiplier),
						maximum_delay,
					);
				},
				Err(e) => {
					log_error!(self.logger, "Failed to list from primary store for namespace {}/{}: {}. Falling back to backup.", 
                          primary_namespace, secondary_namespace, e);
					return self.list_from_backup(primary_namespace, secondary_namespace);
				},
			}
		}
	}

	/// Writes data to the primary store with retry logic.
	///
	/// For transient errors, retries up to a maximum delay time with exponential
	/// backoff.
	async fn retry_write_with_backoff(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> io::Result<()> {
		let mut delay = Duration::from_millis(self.retry_config.initial_retry_delay_ms as u64);
		let maximum_delay = Duration::from_millis(self.retry_config.maximum_delay_ms as u64);
		let mut tries = 0_u16;

		loop {
			match KVStore::write(
				self.primary_store.as_ref(),
				primary_namespace,
				secondary_namespace,
				key,
				buf.clone(),
			)
			.await
			{
				Ok(res) => {
					log_info!(
						self.logger,
						"Write succeeded after {} retries for key: {}/{}/{}",
						tries,
						primary_namespace,
						secondary_namespace,
						key
					);
					return Ok(res);
				},
				Err(e) if is_possibly_transient(&e) && (delay < maximum_delay) => {
					log_warn!(
                    self.logger,
                    "Possible transient error writing key {}/{}/{} (attempt {}): {}. Retrying...",
                    primary_namespace,
                    secondary_namespace,
                    key,
                    tries + 1,
                    e
                );
					tries += 1;
					tokio::time::sleep(delay).await;
					delay = std::cmp::min(
						delay.mul_f32(self.retry_config.backoff_multiplier),
						maximum_delay,
					);
				},
				Err(e) => {
					log_error!(
						self.logger,
						"Failed to write to primary store for key {}/{}/{}: {}",
						primary_namespace,
						secondary_namespace,
						key,
						e
					);
					return Err(e);
				},
			}
		}
	}

	/// Removes data from the primary store with retry logic.
	///
	/// For transient errors, retries up to a maximum delay time with exponential
	/// backoff.
	async fn retry_remove_with_backoff(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> io::Result<()> {
		let mut delay = Duration::from_millis(self.retry_config.initial_retry_delay_ms as u64);
		let maximum_delay = Duration::from_millis(self.retry_config.maximum_delay_ms as u64);
		let mut tries = 0_u16;

		loop {
			match KVStore::remove(
				self.primary_store.as_ref(),
				primary_namespace,
				secondary_namespace,
				key,
				lazy,
			)
			.await
			{
				Ok(res) => {
					log_info!(
						self.logger,
						"Successfully removed data from primary store after {} retries for key: {}/{}/{}",
						tries,
						primary_namespace,
						secondary_namespace,
						key
					);
					return Ok(res);
				},
				Err(e) if is_possibly_transient(&e) && (delay < maximum_delay) => {
					log_warn!(
                    self.logger,
                    "Possible transient error removing key {}/{}/{} from primary store (attempt {}): {}. Retrying...",
                    primary_namespace,
                    secondary_namespace,
                    key,
                    tries + 1,
                    e
                );
					tries += 1;
					tokio::time::sleep(delay).await;
					delay = std::cmp::min(
						delay.mul_f32(self.retry_config.backoff_multiplier),
						maximum_delay,
					);
				},
				Err(e) => {
					log_error!(
						self.logger,
						"Failed to remove data from primary store for key {}/{}/{}: {}",
						primary_namespace,
						secondary_namespace,
						key,
						e
					);
					return Err(e);
				},
			}
		}
	}

	async fn primary_write_then_schedule_backup(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> io::Result<()> {
		let primary_write_res = match KVStore::write(
			self.primary_store.as_ref(),
			primary_namespace,
			secondary_namespace,
			key,
			buf.clone(),
		)
		.await
		{
			Ok(res) => Ok(res),
			Err(e) if is_possibly_transient(&e) => {
				self.retry_write_with_backoff(
					primary_namespace,
					secondary_namespace,
					key,
					buf.clone(),
				)
				.await
			},
			Err(e) => Err(e),
		};

		match primary_write_res {
			Ok(res) => {
				// We enqueue for backup only what we successfully write to primary. In doing
				// this we avoid data inconsistencies across stores.
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

				Ok(res)
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
		let primary_remove_res = match KVStore::remove(
			self.primary_store.as_ref(),
			primary_namespace,
			secondary_namespace,
			key,
			lazy,
		)
		.await
		{
			Ok(res) => Ok(res),
			Err(e) if is_possibly_transient(&e) => {
				self.retry_remove_with_backoff(primary_namespace, secondary_namespace, key, lazy)
					.await
			},
			Err(e) => Err(e),
		};

		match primary_remove_res {
			Ok(res) => {
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

				Ok(res)
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

		match (primary_namespace.as_str(), secondary_namespace.as_str(), key.as_str()) {
			(NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE, _, NETWORK_GRAPH_PERSISTENCE_KEY)
			| (SCORER_PERSISTENCE_PRIMARY_NAMESPACE, _, SCORER_PERSISTENCE_KEY) => {
				if let Some(eph_store) = self.ephemeral_store.as_ref() {
					// We only try once here (without retry logic) because local failure might be indicative
					// of a more serious issue (e.g. full memory, memory corruption, permissions change) that
					// do not self-resolve such that retrying would negate the latency benefits.

					// The following questions remain:
					// 1. Are there situations where local transient errors may warrant a retry?
					// 2. Can we reliably identify/detect these transient errors?
					// 3. Should we fall back to the primary or backup stores in the event of any error?
					KVStoreSync::read(
						eph_store.as_ref(),
						&primary_namespace,
						&secondary_namespace,
						&key,
					)
				} else {
					log_debug!(self.logger, "Ephemeral store not configured. Reading non-critical data from primary or backup stores.");
					self.read_primary_or_backup(&primary_namespace, &secondary_namespace, &key)
						.await
				}
			},
			_ => self.read_primary_or_backup(&primary_namespace, &secondary_namespace, &key).await,
		}
	}

	async fn write_internal(
		&self, inner_lock_ref: Arc<tokio::sync::Mutex<u64>>, locking_key: String, version: u64,
		primary_namespace: String, secondary_namespace: String, key: String, buf: Vec<u8>,
	) -> io::Result<()> {
		check_namespace_key_validity(
			primary_namespace.as_str(),
			secondary_namespace.as_str(),
			Some(key.as_str()),
			"write",
		)?;

		match (primary_namespace.as_str(), secondary_namespace.as_str(), key.as_str()) {
			(NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE, _, NETWORK_GRAPH_PERSISTENCE_KEY)
			| (SCORER_PERSISTENCE_PRIMARY_NAMESPACE, _, SCORER_PERSISTENCE_KEY) => {
				if let Some(eph_store) = &self.ephemeral_store {
					self.execute_locked_write(
						inner_lock_ref,
						locking_key,
						version,
						async move || {
							KVStoreSync::write(
								eph_store.as_ref(),
								primary_namespace.as_str(),
								secondary_namespace.as_str(),
								key.as_str(),
								buf,
							)
						},
					)
					.await
				} else {
					log_debug!(self.logger, "Ephemeral store not configured. Writing non-critical data to primary and backup stores.");

					self.execute_locked_write(
						inner_lock_ref,
						locking_key,
						version,
						async move || {
							self.primary_write_then_schedule_backup(
								primary_namespace.as_str(),
								secondary_namespace.as_str(),
								key.as_str(),
								buf,
							)
							.await
						},
					)
					.await
				}
			},
			_ => {
				self.execute_locked_write(inner_lock_ref, locking_key, version, async move || {
					self.primary_write_then_schedule_backup(
						primary_namespace.as_str(),
						secondary_namespace.as_str(),
						key.as_str(),
						buf,
					)
					.await
				})
				.await
			},
		}
	}

	async fn remove_internal(
		&self, inner_lock_ref: Arc<tokio::sync::Mutex<u64>>, locking_key: String, version: u64,
		primary_namespace: String, secondary_namespace: String, key: String, lazy: bool,
	) -> io::Result<()> {
		check_namespace_key_validity(
			primary_namespace.as_str(),
			secondary_namespace.as_str(),
			Some(key.as_str()),
			"remove",
		)?;

		match (primary_namespace.as_str(), secondary_namespace.as_str(), key.as_str()) {
			(NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE, _, NETWORK_GRAPH_PERSISTENCE_KEY)
			| (SCORER_PERSISTENCE_PRIMARY_NAMESPACE, _, SCORER_PERSISTENCE_KEY) => {
				if let Some(eph_store) = &self.ephemeral_store {
					self.execute_locked_write(
						inner_lock_ref,
						locking_key,
						version,
						async move || {
							KVStoreSync::remove(
								eph_store.as_ref(),
								primary_namespace.as_str(),
								secondary_namespace.as_str(),
								key.as_str(),
								lazy,
							)
						},
					)
					.await
				} else {
					log_debug!(self.logger, "Ephemeral store not configured. Removing non-critical data from primary and backup stores.");

					self.execute_locked_write(
						inner_lock_ref,
						locking_key,
						version,
						async move || {
							self.primary_remove_then_schedule_backup(
								primary_namespace.as_str(),
								secondary_namespace.as_str(),
								key.as_str(),
								lazy,
							)
							.await
						},
					)
					.await
				}
			},
			_ => {
				self.execute_locked_write(inner_lock_ref, locking_key, version, async move || {
					self.primary_remove_then_schedule_backup(
						primary_namespace.as_str(),
						secondary_namespace.as_str(),
						key.as_str(),
						lazy,
					)
					.await
				})
				.await
			},
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
					KVStoreSync::list(eph_store.as_ref(), &primary_namespace, &secondary_namespace)
				} else {
					log_debug!(
						self.logger,
						"Ephemeral store not configured. Listing from primary and backup stores."
					);
					self.list_primary_or_backup(&primary_namespace, &secondary_namespace).await
				}
			},
			_ => self.list_primary_or_backup(&primary_namespace, &secondary_namespace).await,
		}
	}

	fn get_inner_lock_ref(&self, locking_key: String) -> Arc<tokio::sync::Mutex<u64>> {
		let mut outer_lock = self.locks.lock().unwrap();
		Arc::clone(&outer_lock.entry(locking_key).or_default())
	}

	async fn execute_locked_write<
		F: Future<Output = Result<(), lightning::io::Error>>,
		FN: FnOnce() -> F,
	>(
		&self, inner_lock_ref: Arc<tokio::sync::Mutex<u64>>, locking_key: String, version: u64,
		callback: FN,
	) -> Result<(), lightning::io::Error> {
		let res = {
			let mut last_written_version = inner_lock_ref.lock().await;

			// Check if we already have a newer version written. This ensures eventual consistency.
			let is_stale_version = version <= *last_written_version;

			if is_stale_version {
				Ok(())
			} else {
				callback().await.map(|_| {
					*last_written_version = version;
				})
			}
		};

		self.clean_locks(&inner_lock_ref, locking_key);
		res
	}

	fn clean_locks(&self, inner_lock_ref: &Arc<tokio::sync::Mutex<u64>>, locking_key: String) {
		// If there no arcs in use elsewhere, this means that there are no in-flight writes. We can remove the map entry
		// to prevent leaking memory. The two arcs that are expected are the one in the map and the one held here in
		// inner_lock_ref. The outer lock is obtained first, to avoid a new arc being cloned after we've already
		// counted.
		let mut outer_lock = self.locks.lock().unwrap();

		let strong_count = Arc::strong_count(&inner_lock_ref);
		debug_assert!(strong_count >= 2, "Unexpected TierStore strong count");

		if strong_count == 2 {
			outer_lock.remove(&locking_key);
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

#[cfg(test)]
mod tests {
	use std::panic::RefUnwindSafe;
	use std::path::PathBuf;
	use std::sync::Arc;
	use std::thread;

	use lightning::util::logger::Level;
	use lightning::util::persist::{
		CHANNEL_MANAGER_PERSISTENCE_KEY, CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
		CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
	};
	use lightning_persister::fs_store::FilesystemStore;

	use crate::io::test_utils::{
		do_read_write_remove_list_persist, random_storage_path, DelayedStore,
	};
	use crate::io::tier_store::{RetryConfig, TierStore};
	use crate::logger::Logger;
	use crate::runtime::Runtime;
	#[cfg(not(feature = "uniffi"))]
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
		let retry_config = RetryConfig::default();
		TierStore::new(primary_store, runtime, logger, retry_config)
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
	fn backup_overflow_doesnt_fail_writes() {
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
			Arc::new(DynStoreWrapper(DelayedStore::new(100, runtime)));
		tier.set_backup_store(Arc::clone(&backup_store));

		let data = vec![42u8; 32];

		let key = CHANNEL_MANAGER_PERSISTENCE_KEY;
		for i in 0..=10 {
			let result = KVStoreSync::write(
				&tier,
				CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
				&format!("{}_{}", key, i),
				data.clone(),
			);

			assert!(result.is_ok(), "Write {} should succeed", i);
		}

		// Check logs for backup queue overflow message
		let log_contents = std::fs::read_to_string(&log_path).unwrap();
		assert!(
			log_contents.contains("Backup queue is full"),
			"Logs should contain backup queue overflow message"
		);
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
			Arc::new(DynStoreWrapper(DelayedStore::new(100, runtime)));
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

		thread::sleep(Duration::from_millis(10));

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
