// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in

use std::{
	collections::HashMap,
	future::Future,
	pin::Pin,
	sync::{
		atomic::{AtomicU64, Ordering},
		Arc, Mutex,
	},
};

use crate::{
	io::utils::check_namespace_key_validity, DynStoreTrait, DynStoreWrapper, SyncAndAsyncKVStore,
};

#[derive(Debug)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Error))]
pub enum IOError {
	NotFound,
	PermissionDenied,
	ConnectionRefused,
	ConnectionReset,
	ConnectionAborted,
	NotConnected,
	AddrInUse,
	AddrNotAvailable,
	BrokenPipe,
	AlreadyExists,
	WouldBlock,
	InvalidInput,
	InvalidData,
	TimedOut,
	WriteZero,
	Interrupted,
	UnexpectedEof,
	Other,
}

impl From<bitcoin::io::Error> for IOError {
	fn from(error: bitcoin::io::Error) -> Self {
		match error.kind() {
			bitcoin::io::ErrorKind::NotFound => IOError::NotFound,
			bitcoin::io::ErrorKind::PermissionDenied => IOError::PermissionDenied,
			bitcoin::io::ErrorKind::ConnectionRefused => IOError::ConnectionRefused,
			bitcoin::io::ErrorKind::ConnectionReset => IOError::ConnectionReset,
			bitcoin::io::ErrorKind::ConnectionAborted => IOError::ConnectionAborted,
			bitcoin::io::ErrorKind::NotConnected => IOError::NotConnected,
			bitcoin::io::ErrorKind::AddrInUse => IOError::AddrInUse,
			bitcoin::io::ErrorKind::AddrNotAvailable => IOError::AddrNotAvailable,
			bitcoin::io::ErrorKind::BrokenPipe => IOError::BrokenPipe,
			bitcoin::io::ErrorKind::AlreadyExists => IOError::AlreadyExists,
			bitcoin::io::ErrorKind::WouldBlock => IOError::WouldBlock,
			bitcoin::io::ErrorKind::InvalidInput => IOError::InvalidInput,
			bitcoin::io::ErrorKind::InvalidData => IOError::InvalidData,
			bitcoin::io::ErrorKind::TimedOut => IOError::TimedOut,
			bitcoin::io::ErrorKind::WriteZero => IOError::WriteZero,
			bitcoin::io::ErrorKind::Interrupted => IOError::Interrupted,
			bitcoin::io::ErrorKind::UnexpectedEof => IOError::UnexpectedEof,
			bitcoin::io::ErrorKind::Other => IOError::Other,
		}
	}
}

impl From<IOError> for bitcoin::io::Error {
	fn from(error: IOError) -> Self {
		match error {
			IOError::NotFound => bitcoin::io::ErrorKind::NotFound.into(),
			IOError::PermissionDenied => bitcoin::io::ErrorKind::PermissionDenied.into(),
			IOError::ConnectionRefused => bitcoin::io::ErrorKind::ConnectionRefused.into(),
			IOError::ConnectionReset => bitcoin::io::ErrorKind::ConnectionReset.into(),
			IOError::ConnectionAborted => bitcoin::io::ErrorKind::ConnectionAborted.into(),
			IOError::NotConnected => bitcoin::io::ErrorKind::NotConnected.into(),
			IOError::AddrInUse => bitcoin::io::ErrorKind::AddrInUse.into(),
			IOError::AddrNotAvailable => bitcoin::io::ErrorKind::AddrNotAvailable.into(),
			IOError::BrokenPipe => bitcoin::io::ErrorKind::BrokenPipe.into(),
			IOError::AlreadyExists => bitcoin::io::ErrorKind::AlreadyExists.into(),
			IOError::WouldBlock => bitcoin::io::ErrorKind::WouldBlock.into(),
			IOError::InvalidInput => bitcoin::io::ErrorKind::InvalidInput.into(),
			IOError::InvalidData => bitcoin::io::ErrorKind::InvalidData.into(),
			IOError::TimedOut => bitcoin::io::ErrorKind::TimedOut.into(),
			IOError::WriteZero => bitcoin::io::ErrorKind::WriteZero.into(),
			IOError::Interrupted => bitcoin::io::ErrorKind::Interrupted.into(),
			IOError::UnexpectedEof => bitcoin::io::ErrorKind::UnexpectedEof.into(),
			IOError::Other => bitcoin::io::ErrorKind::Other.into(),
		}
	}
}

impl std::fmt::Display for IOError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			IOError::NotFound => write!(f, "NotFound"),
			IOError::PermissionDenied => write!(f, "PermissionDenied"),
			IOError::ConnectionRefused => write!(f, "ConnectionRefused"),
			IOError::ConnectionReset => write!(f, "ConnectionReset"),
			IOError::ConnectionAborted => write!(f, "ConnectionAborted"),
			IOError::NotConnected => write!(f, "NotConnected"),
			IOError::AddrInUse => write!(f, "AddrInUse"),
			IOError::AddrNotAvailable => write!(f, "AddrNotAvailable"),
			IOError::BrokenPipe => write!(f, "BrokenPipe"),
			IOError::AlreadyExists => write!(f, "AlreadyExists"),
			IOError::WouldBlock => write!(f, "WouldBlock"),
			IOError::InvalidInput => write!(f, "InvalidInput"),
			IOError::InvalidData => write!(f, "InvalidData"),
			IOError::TimedOut => write!(f, "TimedOut"),
			IOError::WriteZero => write!(f, "WriteZero"),
			IOError::Interrupted => write!(f, "Interrupted"),
			IOError::UnexpectedEof => write!(f, "UnexpectedEof"),
			IOError::Other => write!(f, "Other"),
		}
	}
}

/// FFI-safe version of [`DynStoreTrait`].
///
/// [`DynStoreTrait`]: crate::types::DynStoreTrait
#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait FfiDynStoreTrait: Send + Sync {
	async fn read_async(
		&self, primary_namespace: String, secondary_namespace: String, key: String,
	) -> Result<Vec<u8>, IOError>;
	async fn write_async(
		&self, primary_namespace: String, secondary_namespace: String, key: String, buf: Vec<u8>,
	) -> Result<(), IOError>;
	async fn remove_async(
		&self, primary_namespace: String, secondary_namespace: String, key: String, lazy: bool,
	) -> Result<(), IOError>;
	async fn list_async(
		&self, primary_namespace: String, secondary_namespace: String,
	) -> Result<Vec<String>, IOError>;

	fn read(
		&self, primary_namespace: String, secondary_namespace: String, key: String,
	) -> Result<Vec<u8>, IOError>;
	fn write(
		&self, primary_namespace: String, secondary_namespace: String, key: String, buf: Vec<u8>,
	) -> Result<(), IOError>;
	fn remove(
		&self, primary_namespace: String, secondary_namespace: String, key: String, lazy: bool,
	) -> Result<(), IOError>;
	fn list(
		&self, primary_namespace: String, secondary_namespace: String,
	) -> Result<Vec<String>, IOError>;
}

#[derive(Clone, uniffi::Object)]
pub struct FfiDynStore {
	inner: Arc<FfiDynStoreInner>,
	next_write_version: Arc<AtomicU64>,
}

#[uniffi::export]
impl FfiDynStore {
	#[uniffi::constructor]
	pub fn from_store(store: Arc<dyn FfiDynStoreTrait>) -> Self {
		let inner = Arc::new(FfiDynStoreInner::new(store));
		Self { inner, next_write_version: Arc::new(AtomicU64::new(1)) }
	}
}

impl FfiDynStore {
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
		let version = self.next_write_version.fetch_add(1, Ordering::Relaxed);
		if version == u64::MAX {
			panic!("FfiDynStore version counter overflowed");
		}

		let inner_lock_ref = self.inner.get_inner_lock_ref(locking_key);

		(inner_lock_ref, version)
	}
}

impl DynStoreTrait for FfiDynStore {
	fn read_async(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, bitcoin::io::Error>> + Send + 'static>> {
		let store = Arc::clone(&self.inner);
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();

		Box::pin(async move {
			store
				.read_internal_async(primary_namespace, secondary_namespace, key)
				.await
				.map_err(|e| e.into())
		})
	}

	fn write_async(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> Pin<Box<dyn Future<Output = Result<(), bitcoin::io::Error>> + Send + 'static>> {
		let store = Arc::clone(&self.inner);
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();

		let locking_key = self.build_locking_key(&primary_namespace, &secondary_namespace, &key);
		let (inner_lock_ref, version) = self.get_new_version_and_lock_ref(locking_key.clone());

		Box::pin(async move {
			store
				.write_internal_async(
					inner_lock_ref,
					locking_key,
					version,
					primary_namespace,
					secondary_namespace,
					key,
					buf,
				)
				.await
				.map_err(|e| e.into())
		})
	}

	fn remove_async(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> Pin<Box<dyn Future<Output = Result<(), bitcoin::io::Error>> + Send + 'static>> {
		let store = Arc::clone(&self.inner);
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();

		let locking_key = self.build_locking_key(&primary_namespace, &secondary_namespace, &key);
		let (inner_lock_ref, version) = self.get_new_version_and_lock_ref(locking_key.clone());

		Box::pin(async move {
			store
				.remove_internal_async(
					inner_lock_ref,
					locking_key,
					version,
					primary_namespace,
					secondary_namespace,
					key,
					lazy,
				)
				.await
				.map_err(|e| e.into())
		})
	}

	fn list_async(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> Pin<Box<dyn Future<Output = Result<Vec<String>, bitcoin::io::Error>> + Send + 'static>> {
		let store = Arc::clone(&self.inner);
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();

		Box::pin(async move {
			store
				.list_internal_async(primary_namespace, secondary_namespace)
				.await
				.map_err(|e| e.into())
		})
	}

	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> Result<Vec<u8>, bitcoin::io::Error> {
		let store = Arc::clone(&self.inner);

		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();

		store.read_internal(primary_namespace, secondary_namespace, key)
	}

	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> Result<(), bitcoin::io::Error> {
		let store = Arc::clone(&self.inner);

		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();

		let locking_key = self.build_locking_key(&primary_namespace, &secondary_namespace, &key);
		let (inner_lock_ref, version) = self.get_new_version_and_lock_ref(locking_key.clone());

		store.write_internal(
			inner_lock_ref,
			locking_key,
			version,
			primary_namespace,
			secondary_namespace,
			key,
			buf,
		)
	}

	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> Result<(), bitcoin::io::Error> {
		let store = Arc::clone(&self.inner);

		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();

		let locking_key = self.build_locking_key(&primary_namespace, &secondary_namespace, &key);
		let (inner_lock_ref, version) = self.get_new_version_and_lock_ref(locking_key.clone());

		store.remove_internal(
			inner_lock_ref,
			locking_key,
			version,
			primary_namespace,
			secondary_namespace,
			key,
			lazy,
		)
	}

	fn list(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> Result<Vec<String>, bitcoin::io::Error> {
		let store = Arc::clone(&self.inner);

		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();

		store.list_internal(primary_namespace, secondary_namespace)
	}
}

struct FfiDynStoreInner {
	ffi_store: Arc<dyn FfiDynStoreTrait>,
	write_version_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<u64>>>>,
}

impl FfiDynStoreInner {
	fn new(ffi_store: Arc<dyn FfiDynStoreTrait>) -> Self {
		Self { ffi_store, write_version_locks: Mutex::new(HashMap::new()) }
	}

	fn get_inner_lock_ref(&self, locking_key: String) -> Arc<tokio::sync::Mutex<u64>> {
		let mut outer_lock = self.write_version_locks.lock().unwrap();
		Arc::clone(&outer_lock.entry(locking_key).or_default())
	}

	async fn read_internal_async(
		&self, primary_namespace: String, secondary_namespace: String, key: String,
	) -> bitcoin::io::Result<Vec<u8>> {
		check_namespace_key_validity(&primary_namespace, &secondary_namespace, Some(&key), "read")?;
		self.ffi_store
			.read_async(primary_namespace, secondary_namespace, key)
			.await
			.map_err(|e| e.into())
	}

	fn read_internal(
		&self, primary_namespace: String, secondary_namespace: String, key: String,
	) -> bitcoin::io::Result<Vec<u8>> {
		check_namespace_key_validity(&primary_namespace, &secondary_namespace, Some(&key), "read")?;
		self.ffi_store.read(primary_namespace, secondary_namespace, key).map_err(|e| e.into())
	}

	async fn write_internal_async(
		&self, inner_lock_ref: Arc<tokio::sync::Mutex<u64>>, locking_key: String, version: u64,
		primary_namespace: String, secondary_namespace: String, key: String, buf: Vec<u8>,
	) -> bitcoin::io::Result<()> {
		check_namespace_key_validity(
			&primary_namespace,
			&secondary_namespace,
			Some(&key),
			"write",
		)?;

		let store = Arc::clone(&self.ffi_store);

		self.execute_locked_write(inner_lock_ref, locking_key, version, async move || {
			store
				.write_async(primary_namespace, secondary_namespace, key, buf)
				.await
				.map_err(|e| <IOError as Into<bitcoin::io::Error>>::into(e))?;

			Ok(())
		})
		.await
	}

	fn write_internal(
		&self, inner_lock_ref: Arc<tokio::sync::Mutex<u64>>, locking_key: String, version: u64,
		primary_namespace: String, secondary_namespace: String, key: String, buf: Vec<u8>,
	) -> bitcoin::io::Result<()> {
		check_namespace_key_validity(
			&primary_namespace,
			&secondary_namespace,
			Some(&key),
			"write",
		)?;

		let res = self.with_blocking_lock(&inner_lock_ref, |last_written_version| {
			if version <= *last_written_version {
				Ok(())
			} else {
				self.ffi_store
					.write(primary_namespace, secondary_namespace, key, buf)
					.map_err(|e| e.into())
					.map(|_| {
						*last_written_version = version;
					})
			}
		});

		self.clean_locks(&inner_lock_ref, locking_key);
		res
	}

	async fn remove_internal_async(
		&self, inner_lock_ref: Arc<tokio::sync::Mutex<u64>>, locking_key: String, version: u64,
		primary_namespace: String, secondary_namespace: String, key: String, lazy: bool,
	) -> bitcoin::io::Result<()> {
		check_namespace_key_validity(
			&primary_namespace,
			&secondary_namespace,
			Some(&key),
			"remove",
		)?;

		let store = Arc::clone(&self.ffi_store);

		self.execute_locked_write(inner_lock_ref, locking_key, version, async move || {
			store
				.remove_async(primary_namespace, secondary_namespace, key, lazy)
				.await
				.map_err(|e| <IOError as Into<bitcoin::io::Error>>::into(e))?;

			Ok(())
		})
		.await
	}

	fn remove_internal(
		&self, inner_lock_ref: Arc<tokio::sync::Mutex<u64>>, locking_key: String, version: u64,
		primary_namespace: String, secondary_namespace: String, key: String, lazy: bool,
	) -> bitcoin::io::Result<()> {
		check_namespace_key_validity(
			&primary_namespace,
			&secondary_namespace,
			Some(&key),
			"remove",
		)?;

		let res = self.with_blocking_lock(&inner_lock_ref, |last_written_version| {
			if version <= *last_written_version {
				Ok(())
			} else {
				self.ffi_store
					.remove(primary_namespace, secondary_namespace, key, lazy)
					.map_err(|e| <IOError as Into<bitcoin::io::Error>>::into(e))
					.map(|_| {
						*last_written_version = version;
					})
			}
		});

		self.clean_locks(&inner_lock_ref, locking_key);
		res
	}

	async fn list_internal_async(
		&self, primary_namespace: String, secondary_namespace: String,
	) -> bitcoin::io::Result<Vec<String>> {
		check_namespace_key_validity(&primary_namespace, &secondary_namespace, None, "list")?;
		self.ffi_store
			.list_async(primary_namespace, secondary_namespace)
			.await
			.map_err(|e| e.into())
	}

	fn list_internal(
		&self, primary_namespace: String, secondary_namespace: String,
	) -> bitcoin::io::Result<Vec<String>> {
		check_namespace_key_validity(&primary_namespace, &secondary_namespace, None, "list")?;
		self.ffi_store.list(primary_namespace, secondary_namespace).map_err(|e| e.into())
	}

	async fn execute_locked_write<
		F: Future<Output = Result<(), bitcoin::io::Error>>,
		FN: FnOnce() -> F,
	>(
		&self, inner_lock_ref: Arc<tokio::sync::Mutex<u64>>, locking_key: String, version: u64,
		callback: FN,
	) -> Result<(), bitcoin::io::Error> {
		let res = {
			let mut last_written_version = inner_lock_ref.lock().await;

			// Check if we already have a newer version written/removed. This is used in async contexts to realize eventual
			// consistency.
			let is_stale_version = version <= *last_written_version;

			// If the version is not stale, we execute the callback. Otherwise we can and must skip writing.
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
		let mut outer_lock = self.write_version_locks.lock().unwrap();

		let strong_count = Arc::strong_count(&inner_lock_ref);
		debug_assert!(strong_count >= 2, "Unexpected FfiDynStore strong count");

		if strong_count == 2 {
			outer_lock.remove(&locking_key);
		}
	}

	fn with_blocking_lock<T, F: FnOnce(&mut u64) -> T>(
		&self, inner_lock_ref: &Arc<tokio::sync::Mutex<u64>>, f: F,
	) -> T {
		if tokio::runtime::Handle::try_current().is_ok() {
			tokio::task::block_in_place(|| {
				let mut last_written_version = inner_lock_ref.blocking_lock();
				f(&mut last_written_version)
			})
		} else {
			let mut last_written_version = inner_lock_ref.blocking_lock();
			f(&mut last_written_version)
		}
	}
}

#[async_trait::async_trait]
impl<T: SyncAndAsyncKVStore + Send + Sync + 'static> FfiDynStoreTrait for DynStoreWrapper<T> {
	async fn read_async(
		&self, primary_namespace: String, secondary_namespace: String, key: String,
	) -> Result<Vec<u8>, IOError> {
		DynStoreTrait::read_async(self, &primary_namespace, &secondary_namespace, &key)
			.await
			.map_err(IOError::from)
	}

	async fn write_async(
		&self, primary_namespace: String, secondary_namespace: String, key: String, buf: Vec<u8>,
	) -> Result<(), IOError> {
		DynStoreTrait::write_async(self, &primary_namespace, &secondary_namespace, &key, buf)
			.await
			.map_err(IOError::from)
	}

	async fn remove_async(
		&self, primary_namespace: String, secondary_namespace: String, key: String, lazy: bool,
	) -> Result<(), IOError> {
		DynStoreTrait::remove_async(self, &primary_namespace, &secondary_namespace, &key, lazy)
			.await
			.map_err(IOError::from)
	}

	async fn list_async(
		&self, primary_namespace: String, secondary_namespace: String,
	) -> Result<Vec<String>, IOError> {
		DynStoreTrait::list_async(self, &primary_namespace, &secondary_namespace)
			.await
			.map_err(IOError::from)
	}

	fn read(
		&self, primary_namespace: String, secondary_namespace: String, key: String,
	) -> Result<Vec<u8>, IOError> {
		DynStoreTrait::read(self, &primary_namespace, &secondary_namespace, &key)
			.map_err(IOError::from)
	}

	fn write(
		&self, primary_namespace: String, secondary_namespace: String, key: String, buf: Vec<u8>,
	) -> Result<(), IOError> {
		DynStoreTrait::write(self, &primary_namespace, &secondary_namespace, &key, buf)
			.map_err(IOError::from)
	}

	fn remove(
		&self, primary_namespace: String, secondary_namespace: String, key: String, lazy: bool,
	) -> Result<(), IOError> {
		DynStoreTrait::remove(self, &primary_namespace, &secondary_namespace, &key, lazy)
			.map_err(IOError::from)
	}

	fn list(
		&self, primary_namespace: String, secondary_namespace: String,
	) -> Result<Vec<String>, IOError> {
		DynStoreTrait::list(self, &primary_namespace, &secondary_namespace).map_err(IOError::from)
	}
}

impl<T: SyncAndAsyncKVStore + Send + Sync + 'static> From<T> for FfiDynStore {
	fn from(store: T) -> Self {
		Self::from_store(Arc::new(DynStoreWrapper(store)))
	}
}
