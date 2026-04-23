// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::io::utils::check_namespace_key_validity;
use crate::logger::{LdkLogger, Logger};
use crate::types::DynStore;

use bitcoin::io::Read;
use lightning::ln::msgs::DecodeError;
use lightning::util::persist::{
	KVStore, KVStoreSync, NETWORK_GRAPH_PERSISTENCE_KEY,
	NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE, NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
	SCORER_PERSISTENCE_KEY, SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
};
use lightning::util::ser::{Readable, Writeable, Writer};
use lightning::{io, log_error};

use std::collections::HashMap;
use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

/// A 3-tiered [`KVStore`]/[`KVStoreSync`] implementation that routes data across
/// storage backends that may be local or remote:
/// - a primary store for durable, authoritative persistence,
/// - an optional backup store that maintains an additional durable copy of
///   primary-backed data, and
/// - an optional ephemeral store for non-critical, rebuildable cached data.
///
/// When a backup store is configured, writes and removals for primary-backed
/// data are issued to the primary and backup stores concurrently. Success
/// semantics depend on the configured [`BackupMode`].
///
/// Reads and lists do not consult the backup store during normal operation.
/// Ephemeral data is read from and written to the ephemeral store when configured.
///
/// Note that dual-store writes and removals are not atomic across the primary
/// and backup stores. One store may already reflect the change even if the
/// overall operation returns an error.
pub(crate) struct TierStore {
	inner: Arc<TierStoreInner>,
	logger: Arc<Logger>,
}

impl TierStore {
	pub fn new(primary_store: Arc<DynStore>, logger: Arc<Logger>) -> Self {
		let inner = Arc::new(TierStoreInner::new(primary_store, Arc::clone(&logger)));

		Self { inner, logger }
	}

	/// Configures a backup store for primary-backed data.
	///
	/// Once set, writes and removals targeting the primary tier are issued to the
	/// primary and backup stores concurrently. Success semantics depend on the
	/// configured [`BackupMode`].
	///
	/// Note: dual-store writes/removals are not atomic. An error may be returned
	/// after the primary store has already been updated if the requested backup
	/// guarantee could not be achieved.
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
			(Ok(()), Err(backup_err)) => {
				log_error!(
					self.logger,
					"Backup {} failed for key {}/{}/{}: {}",
					op,
					primary_namespace,
					secondary_namespace,
					key,
					backup_err
				);
				Ok(())
			},
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

// Backup retry constants.
const BACKUP_RETRY_QUEUE_PRIMARY_NAMESPACE: &str = "backup_retry";
const BACKUP_RETRY_QUEUE_SECONDARY_NAMESPACE: &str = "";
const BACKUP_RETRY_QUEUE_KEY: &str = "pending_ops";

// Backoff parameters for retry task.
const RETRY_INITIAL_BACKOFF_MS: u64 = 500;
const RETRY_MAX_BACKOFF_MS: u64 = 60_000;
const RETRY_BACKOFF_MULTIPLIER: u64 = 2;

/// Controls how TierStore treats backup-store failures for primary-backed writes and removals.
pub enum BackupMode {
	/// Writes must succeed on the primary store.
	///
	/// Backup writes/removals are attempted immediately, but if the backup
	/// store fails, the operation still succeeds as long as the primary store
	/// succeeds. Backup failures are logged.
	BestEffortBackup,

	/// Writes must succeed on the primary store.
	///
	/// Backup writes/removals are attempted immediately. If the backup store
	/// fails but the primary succeeds, the operation still succeeds only if the
	/// failed backup operation is durably persisted locally and enqueued for
	/// asynchronous retry.
	SemiSync,
}

/// A pending operation against the backup store that failed on the write path
/// and needs to be retried asynchronously.
///
/// Limitation:
/// `Write` carries the exact `buf` at enqueue time. We never re-read from
/// primary at retry time — if a `Remove` for the same key succeeds on primary
/// after this `Write` was enqueued, re-reading primary would find nothing and
/// potentially resurrect deleted data on the backup.
#[derive(Clone)]
pub(crate) enum PendingBackupOp {
	Write { buf: Vec<u8> },
	Remove { lazy: bool },
}

impl Writeable for PendingBackupOp {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), io::Error> {
		match self {
			PendingBackupOp::Write { buf } => {
				0u8.write(writer)?;
				(buf.len() as u32).write(writer)?;
				writer.write_all(buf)?;
			},
			PendingBackupOp::Remove { lazy } => {
				1u8.write(writer)?;
				lazy.write(writer)?;
			},
		}
		Ok(())
	}
}

impl Readable for PendingBackupOp {
	fn read<R: Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let tag: u8 = Readable::read(reader)?;
		match tag {
			0 => {
				let len: u32 = Readable::read(reader)?;
				let mut buf = vec![0u8; len as usize];
				reader.read_exact(&mut buf)?;
				Ok(PendingBackupOp::Write { buf })
			},
			1 => {
				let lazy: bool = Readable::read(reader)?;
				Ok(PendingBackupOp::Remove { lazy })
			},
			_ => Err(DecodeError::InvalidValue),
		}
	}
}

/// Deserialization wrapper for the pending ops map.
/// Decodes from a length-prefixed list of `(pn, sn, key, op_tag, [buf])` entries.
struct PendingOpsDeserWrapper(HashMap<(String, String, String), PendingBackupOp>);

impl Readable for PendingOpsDeserWrapper {
	fn read<R: Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let count: u32 = Readable::read(reader)?;
		let mut map = HashMap::with_capacity(count as usize);
		for _ in 0..count {
			let pn: String = Readable::read(reader)?;
			let sn: String = Readable::read(reader)?;
			let key: String = Readable::read(reader)?;
			let op: PendingBackupOp = Readable::read(reader)?;
			map.insert((pn, sn, key), op);
		}
		Ok(Self(map))
	}
}

/// Serialization wrapper for the pending ops map.
/// Encodes as a length-prefixed list of `(pn, sn, key, op_tag, [buf])` entries.
struct PendingOpsSerWrapper<'a>(&'a HashMap<(String, String, String), PendingBackupOp>);

impl Writeable for PendingOpsSerWrapper<'_> {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), io::Error> {
		(self.0.len() as u32).write(writer)?;
		let mut entries: Vec<_> = self.0.iter().collect();
		entries.sort_by(|a, b| a.0.cmp(b.0));

		for ((pn, sn, key), op) in entries {
			pn.write(writer)?;
			sn.write(writer)?;
			key.write(writer)?;
			op.write(writer)?;
		}

		Ok(())
	}
}

/// A durable, locally-persisted queue of backup operations that failed on the
/// write path and are pending async retry.
///
/// The queue is keyed by `(primary_namespace, secondary_namespace, key)` so
/// that a newer op for the same key always replaces an older one —
/// deduplication is structural. This means:
/// - `Write` followed by `Write` to same key: only latest `buf` is retried.
/// - `Write` followed by `Remove` to same key: only `Remove` is retried.
/// - `Remove` followed by `Write` to same key: only latest `Write` is retried.
///
/// The queue persists to a local `FilesystemStoreV2` at a dedicated namespace,
/// never through `TierStore`'s backup replication path, to avoid recursion
/// and to remain available even when primary is a remote store.
pub(crate) struct BackupRetryQueue<L: Deref>
where
	L::Target: LdkLogger,
{
	pending: Arc<Mutex<HashMap<(String, String, String), PendingBackupOp>>>,
	waker: Arc<Mutex<Option<Waker>>>,
	/// Always a local store — FilesystemStoreV2 at node data dir.
	/// Never the user's primary or backup store.
	local_store: Arc<DynStore>,
	logger: L,
}

impl<L: Deref> BackupRetryQueue<L>
where
	L::Target: LdkLogger,
{
	/// Creates a new queue, loading any previously persisted pending ops from
	/// `local_store` so that ops enqueued before a crash are not lost.
	pub(crate) fn new(local_store: Arc<DynStore>, logger: L) -> Self {
		let pending = Arc::new(Mutex::new(HashMap::new()));

		match KVStoreSync::read(
			local_store.as_ref(),
			BACKUP_RETRY_QUEUE_PRIMARY_NAMESPACE,
			BACKUP_RETRY_QUEUE_SECONDARY_NAMESPACE,
			BACKUP_RETRY_QUEUE_KEY,
		) {
			Ok(data) => match PendingOpsDeserWrapper::read(&mut io::Cursor::new(data)) {
				Ok(wrapper) => {
					*pending.lock().expect("lock") = wrapper.0;
				},
				Err(e) => {
					log_error!(
						logger,
						"Failed to decode persisted backup retry queue, \
                        starting fresh. Backup store may be missing recent \
                        writes. Error: {}",
						e
					);
				},
			},
			Err(e) if e.kind() == io::ErrorKind::NotFound => {
				// No persisted queue — normal on first startup.
			},
			Err(e) => {
				log_error!(
					logger,
					"Failed to read persisted backup retry queue: {}. \
                Starting fresh.",
					e
				);
			},
		}

		Self { pending, waker: Arc::new(Mutex::new(None)), local_store, logger }
	}

	/// Enqueues a failed backup operation for asynchronous retry.
	///
	/// If a pending op already exists for this key, it is replaced by the newer
	/// op. The updated queue is persisted synchronously to the local store before
	/// this function returns.
	///
	/// Returns an error if the retry intent could not be durably persisted locally.
	/// In that case, the op may still remain queued in memory for the current
	/// process, but it is not guaranteed to survive a restart.
	///
	/// This is the sync enqueue path, used from synchronous write/remove flows.
	pub(crate) fn enqueue_sync(
		&self, key: (String, String, String), op: PendingBackupOp,
	) -> io::Result<()> {
		let encoded = {
			let mut locked = self.pending.lock().expect("lock");
			locked.insert(key, op);
			PendingOpsSerWrapper(&*locked).encode()
		};

		KVStoreSync::write(
			self.local_store.as_ref(),
			BACKUP_RETRY_QUEUE_PRIMARY_NAMESPACE,
			BACKUP_RETRY_QUEUE_SECONDARY_NAMESPACE,
			BACKUP_RETRY_QUEUE_KEY,
			encoded,
		)?;

		if let Some(waker) = self.waker.lock().expect("lock").take() {
			waker.wake();
		}

		Ok(())
	}

	/// Enqueues a failed backup operation for asynchronous retry.
	///
	/// This is the async enqueue path, used from asynchronous write/remove flows.
	/// Internally it reuses the synchronous implementation because queue mutation
	/// and local queue persistence are both synchronous operations.
	///
	/// Returns an error if the retry intent could not be durably persisted locally.
	/// In that case, the op may still remain queued in memory for the current
	/// process, but it is not guaranteed to survive a restart.
	pub(crate) async fn enqueue_async(
		&self, key: (String, String, String), op: PendingBackupOp,
	) -> io::Result<()> {
		self.enqueue_sync(key, op)
	}

	/// Removes a successfully retried entry from the in-memory queue and
	/// best-effort persists the updated state.
	///
	/// Failure to persist this cleanup is logged but not returned. At that
	/// point the backup operation has already succeeded, so correctness relies
	/// on at-least-once replay semantics: if the process restarts before a
	/// later successful queue persistence, the completed op may be retried
	/// again from the on-disk snapshot.
	///
	/// This is acceptable because retried backup writes/removals are expected
	/// to be idempotent at the `KVStore` layer.
	pub(crate) fn remove_completed(&self, key: &(String, String, String)) {
		let encoded = {
			let mut locked = self.pending.lock().expect("lock");
			locked.remove(key);
			PendingOpsSerWrapper(&*locked).encode()
		};

		if let Err(e) = KVStoreSync::write(
			self.local_store.as_ref(),
			BACKUP_RETRY_QUEUE_PRIMARY_NAMESPACE,
			BACKUP_RETRY_QUEUE_SECONDARY_NAMESPACE,
			BACKUP_RETRY_QUEUE_KEY,
			encoded,
		) {
			// Cleanup persistence is best-effort only. The backup op has already
			// succeeded, so a later restart may replay it from the stale on-disk
			// queue snapshot. This is acceptable because backup ops are expected
			// to be idempotent.
			log_error!(
				self.logger,
				"Failed to persist backup retry queue after successful retry for key {:?}: {}",
				key,
				e
			);
		}
	}

	/// Returns a snapshot of all pending entries for the retry task to drain.
	///
	/// Returns cloned entries so the lock is not held during I/O.
	pub(crate) fn snapshot(&self) -> Vec<((String, String, String), PendingBackupOp)> {
		self.pending.lock().expect("lock").iter().map(|(k, v)| (k.clone(), v.clone())).collect()
	}

	/// Returns true if the queue has no pending entries.
	pub(crate) fn is_empty(&self) -> bool {
		self.pending.lock().expect("lock").is_empty()
	}

	/// Waits asynchronously until the queue has entries to process.
	pub(crate) async fn wait_for_entries(&self) {
		RetryQueueFuture { pending: Arc::clone(&self.pending), waker: Arc::clone(&self.waker) }
			.await
	}
}

/// Future that resolves when the retry queue has pending entries.
/// Mirrors `EventFuture` in `EventQueue`.
struct RetryQueueFuture {
	pending: Arc<Mutex<HashMap<(String, String, String), PendingBackupOp>>>,
	waker: Arc<Mutex<Option<Waker>>>,
}

impl Future for RetryQueueFuture {
	type Output = ();

	fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		if !self.pending.lock().expect("lock").is_empty() {
			Poll::Ready(())
		} else {
			*self.waker.lock().expect("lock") = Some(cx.waker().clone());
			Poll::Pending
		}
	}
}

/// Runs the background retry loop for the given `BackupRetryQueue` against
/// `backup_store`.
///
/// Wakes when the queue has entries, iterates all pending ops, applies each
/// against the backup store, and removes successfully retried entries.
/// Uses exponential backoff with jitter on persistent failures.
/// Throttles error logging to avoid log spam.
///
/// This function runs for node lifetime and should be spawned via
/// `tokio::spawn` during `build()` when `BackupMode::SemiSync` is configured.
pub(crate) async fn run_backup_retry_task<L: Deref + Send + Sync + 'static>(
	queue: Arc<BackupRetryQueue<L>>, backup_store: Arc<DynStore>, logger: L,
) where
	L::Target: LdkLogger,
{
	let mut backoff_ms = RETRY_INITIAL_BACKOFF_MS;
	let mut consecutive_failures: usize = 0;

	loop {
		// Wait until there is something to do.
		queue.wait_for_entries().await;

		let entries = queue.snapshot();
		let mut all_succeeded = true;

		for (key, op) in &entries {
			let (pn, sn, k) = key;

			// Before applying, check whether this key is still in the queue
			// with the same op. A newer op may have replaced it since we
			// took the snapshot.
			{
				let locked = queue.pending.lock().expect("lock");
				match locked.get(key) {
					None => {
						// Entry was removed (completed by a concurrent retry).
						continue;
					},
					Some(current_op) => {
						// If the op in the queue differs from our snapshot,
						// a newer op replaced it. Skip — next iteration will
						// pick up the newer op.
						if !ops_match(current_op, op) {
							continue;
						}
					},
				}
			}

			let result = match op {
				PendingBackupOp::Write { buf } => {
					KVStoreSync::write(backup_store.as_ref(), pn, sn, k, buf.clone())
				},
				PendingBackupOp::Remove { lazy } => {
					KVStoreSync::remove(backup_store.as_ref(), pn, sn, k, *lazy)
				},
			};

			match result {
				Ok(()) => {
					queue.remove_completed(key);
					consecutive_failures = 0;
					backoff_ms = RETRY_INITIAL_BACKOFF_MS;
				},
				Err(e) => {
					all_succeeded = false;
					consecutive_failures += 1;
					// Throttle logging — only log every 8 failures to avoid
					// spam on persistent remote store outages.
					if consecutive_failures == 1 || consecutive_failures.is_power_of_two() {
						log_error!(
							logger,
							"Backup retry failed for key {}/{}/{} \
                            (attempt {}): {}",
							pn,
							sn,
							k,
							consecutive_failures,
							e
						);
					}
				},
			}
		}

		if !all_succeeded {
			// Exponential backoff with a small jitter.
			let jitter_ms = (backoff_ms / 10).max(1);
			let sleep_ms = backoff_ms + (jitter_ms % 17); // deterministic but offset
			tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
			backoff_ms = (backoff_ms * RETRY_BACKOFF_MULTIPLIER).min(RETRY_MAX_BACKOFF_MS);
		}
	}
}

/// Returns true if two `PendingBackupOp` values are the same variant with
/// equivalent data. Used to detect staleness before applying a retried op.
fn ops_match(a: &PendingBackupOp, b: &PendingBackupOp) -> bool {
	match (a, b) {
		(PendingBackupOp::Write { buf: ba }, PendingBackupOp::Write { buf: bb }) => ba == bb,
		(PendingBackupOp::Remove { lazy: la }, PendingBackupOp::Remove { lazy: lb }) => la == lb,
		_ => false,
	}
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
