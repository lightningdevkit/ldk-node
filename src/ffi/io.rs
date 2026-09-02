// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

#[cfg(feature = "storage-tier")]
use std::future::Future;
#[cfg(feature = "storage-tier")]
use std::pin::Pin;
use std::sync::Arc;

use lightning::util::persist::{KVStore, MigratableKVStore, PaginatedKVStore};
#[cfg(feature = "storage-tier")]
use lightning::util::persist::{
	PageToken as LdkPageToken, PaginatedListResponse as LdkPaginatedListResponse,
};

use crate::ffi::PageToken;
#[cfg(feature = "storage-tier")]
use crate::io::utils::check_namespace_key_validity;

/// An I/O error returned by a foreign key-value store.
#[derive(Debug, uniffi::Error)]
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
			bitcoin::io::ErrorKind::NotFound => Self::NotFound,
			bitcoin::io::ErrorKind::PermissionDenied => Self::PermissionDenied,
			bitcoin::io::ErrorKind::ConnectionRefused => Self::ConnectionRefused,
			bitcoin::io::ErrorKind::ConnectionReset => Self::ConnectionReset,
			bitcoin::io::ErrorKind::ConnectionAborted => Self::ConnectionAborted,
			bitcoin::io::ErrorKind::NotConnected => Self::NotConnected,
			bitcoin::io::ErrorKind::AddrInUse => Self::AddrInUse,
			bitcoin::io::ErrorKind::AddrNotAvailable => Self::AddrNotAvailable,
			bitcoin::io::ErrorKind::BrokenPipe => Self::BrokenPipe,
			bitcoin::io::ErrorKind::AlreadyExists => Self::AlreadyExists,
			bitcoin::io::ErrorKind::WouldBlock => Self::WouldBlock,
			bitcoin::io::ErrorKind::InvalidInput => Self::InvalidInput,
			bitcoin::io::ErrorKind::InvalidData => Self::InvalidData,
			bitcoin::io::ErrorKind::TimedOut => Self::TimedOut,
			bitcoin::io::ErrorKind::WriteZero => Self::WriteZero,
			bitcoin::io::ErrorKind::Interrupted => Self::Interrupted,
			bitcoin::io::ErrorKind::UnexpectedEof => Self::UnexpectedEof,
			bitcoin::io::ErrorKind::Other => Self::Other,
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
		write!(f, "{:?}", self)
	}
}

/// One fully-qualified key returned by [`DynStoreTrait::list_all_keys`].
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct KVStoreKey {
	pub primary_namespace: String,
	pub secondary_namespace: String,
	pub key: String,
}

/// A page of keys returned by [`DynStoreTrait::list_paginated`].
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct PaginatedListResponse {
	pub keys: Vec<String>,
	pub next_page_token: Option<Arc<PageToken>>,
}

/// An asynchronous key-value store implemented by a foreign-language caller.
///
/// Implementations must support namespace listing, paginated listing, and exhaustive key
/// enumeration in addition to ordinary key-value operations.
#[async_trait::async_trait]
pub trait DynStoreTrait: Send + Sync {
	/// Returns the data stored for the given `primary_namespace`, `secondary_namespace`, and
	/// `key`.
	///
	/// Returns [`IOError::NotFound`] if the key does not exist in the given namespaces.
	async fn read(
		&self, primary_namespace: String, secondary_namespace: String, key: String,
	) -> Result<Vec<u8>, IOError>;

	/// Persists the given data under the given `key`.
	///
	/// Creates the given `primary_namespace` and `secondary_namespace` if they do not already
	/// exist.
	async fn write(
		&self, primary_namespace: String, secondary_namespace: String, key: String, buf: Vec<u8>,
	) -> Result<(), IOError>;

	/// Removes any data previously persisted under the given `key`.
	///
	/// If `lazy` is `true`, the implementation may defer the removal, for example to batch several
	/// removals. Lazy removals may be lost if the process crashes before they are persisted and
	/// should therefore only be used when the removal can safely be replayed.
	///
	/// Returns successfully if the key is absent, whether or not it existed before this call.
	async fn remove(
		&self, primary_namespace: String, secondary_namespace: String, key: String, lazy: bool,
	) -> Result<(), IOError>;

	/// Returns the keys stored under the given `secondary_namespace` in `primary_namespace`.
	///
	/// Keys may be returned in any order. Returns an empty list if either namespace is unknown.
	async fn list(
		&self, primary_namespace: String, secondary_namespace: String,
	) -> Result<Vec<String>, IOError>;

	/// Returns one page of keys from the requested namespace, ordered from most recently created
	/// to least recently created.
	///
	/// If `page_token` is provided, listing continues from where the previous page ended. If it is
	/// absent, listing begins with the most recently created key. The `next_page_token` in the
	/// response can be passed to a subsequent call to retrieve the next page.
	///
	/// Page tokens are scoped to a particular `(primary_namespace, secondary_namespace)` pair. If
	/// the key referenced by a token has been removed, listing should continue from the next valid
	/// position rather than fail.
	///
	/// Returns an empty page if either namespace is unknown or no more keys remain.
	async fn list_paginated(
		&self, primary_namespace: String, secondary_namespace: String,
		page_token: Option<Arc<PageToken>>,
	) -> Result<PaginatedListResponse, IOError>;

	/// Returns every key in the store together with its namespaces.
	///
	/// Exhaustive key enumeration is required for storage migrations. This includes backfilling a
	/// newly configured backup and resilvering a backup that became stale while the node operated
	/// without it.
	///
	/// The result must include every key known to the store so the destination is not left
	/// incomplete, but keys may be returned in any order.
	async fn list_all_keys(&self) -> Result<Vec<KVStoreKey>, IOError>;
}

/// Adapts a foreign [`DynStoreTrait`] implementation to ldk-node's internal store interface.
#[cfg(feature = "storage-tier")]
pub(crate) struct DynStore {
	inner: Arc<dyn DynStoreTrait>,
}

#[cfg(feature = "storage-tier")]
impl DynStore {
	pub(crate) fn new(inner: Arc<dyn DynStoreTrait>) -> Self {
		Self { inner }
	}
}

#[cfg(feature = "storage-tier")]
impl crate::types::DynStoreTrait for DynStore {
	fn read_async(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, bitcoin::io::Error>> + Send + 'static>> {
		let inner = Arc::clone(&self.inner);
		let primary_namespace = primary_namespace.to_owned();
		let secondary_namespace = secondary_namespace.to_owned();
		let key = key.to_owned();
		Box::pin(async move {
			check_namespace_key_validity(
				&primary_namespace,
				&secondary_namespace,
				Some(&key),
				"read",
			)?;
			inner.read(primary_namespace, secondary_namespace, key).await.map_err(Into::into)
		})
	}

	fn write_async(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> Pin<Box<dyn Future<Output = Result<(), bitcoin::io::Error>> + Send + 'static>> {
		let inner = Arc::clone(&self.inner);
		let primary_namespace = primary_namespace.to_owned();
		let secondary_namespace = secondary_namespace.to_owned();
		let key = key.to_owned();
		Box::pin(async move {
			check_namespace_key_validity(
				&primary_namespace,
				&secondary_namespace,
				Some(&key),
				"write",
			)?;
			inner.write(primary_namespace, secondary_namespace, key, buf).await.map_err(Into::into)
		})
	}

	fn remove_async(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> Pin<Box<dyn Future<Output = Result<(), bitcoin::io::Error>> + Send + 'static>> {
		let inner = Arc::clone(&self.inner);
		let primary_namespace = primary_namespace.to_owned();
		let secondary_namespace = secondary_namespace.to_owned();
		let key = key.to_owned();
		Box::pin(async move {
			check_namespace_key_validity(
				&primary_namespace,
				&secondary_namespace,
				Some(&key),
				"remove",
			)?;
			inner
				.remove(primary_namespace, secondary_namespace, key, lazy)
				.await
				.map_err(Into::into)
		})
	}

	fn list_async(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> Pin<Box<dyn Future<Output = Result<Vec<String>, bitcoin::io::Error>> + Send + 'static>> {
		let inner = Arc::clone(&self.inner);
		let primary_namespace = primary_namespace.to_owned();
		let secondary_namespace = secondary_namespace.to_owned();
		Box::pin(async move {
			check_namespace_key_validity(&primary_namespace, &secondary_namespace, None, "list")?;
			inner.list(primary_namespace, secondary_namespace).await.map_err(Into::into)
		})
	}

	fn list_paginated_async(
		&self, primary_namespace: &str, secondary_namespace: &str, page_token: Option<LdkPageToken>,
	) -> Pin<
		Box<
			dyn Future<Output = Result<LdkPaginatedListResponse, bitcoin::io::Error>>
				+ Send
				+ 'static,
		>,
	> {
		let inner = Arc::clone(&self.inner);
		let primary_namespace = primary_namespace.to_owned();
		let secondary_namespace = secondary_namespace.to_owned();
		let page_token = page_token.map(|token| Arc::new(token.into()));
		Box::pin(async move {
			check_namespace_key_validity(&primary_namespace, &secondary_namespace, None, "list")?;
			let response = inner
				.list_paginated(primary_namespace, secondary_namespace, page_token)
				.await
				.map_err(bitcoin::io::Error::from)?;
			Ok(LdkPaginatedListResponse {
				keys: response.keys,
				next_page_token: response
					.next_page_token
					.map(|token| token.as_ref().as_ref().clone()),
			})
		})
	}

	fn list_all_keys_async(
		&self,
	) -> Pin<
		Box<
			dyn Future<Output = Result<Vec<(String, String, String)>, bitcoin::io::Error>>
				+ Send
				+ 'static,
		>,
	> {
		let inner = Arc::clone(&self.inner);
		Box::pin(async move {
			inner.list_all_keys().await.map_err(Into::into).map(|keys| {
				keys.into_iter()
					.map(|key| (key.primary_namespace, key.secondary_namespace, key.key))
					.collect()
			})
		})
	}
}

#[async_trait::async_trait]
impl<T> DynStoreTrait for T
where
	T: PaginatedKVStore + MigratableKVStore + Send + Sync + 'static,
{
	async fn read(
		&self, primary_namespace: String, secondary_namespace: String, key: String,
	) -> Result<Vec<u8>, IOError> {
		KVStore::read(self, &primary_namespace, &secondary_namespace, &key)
			.await
			.map_err(Into::into)
	}

	async fn write(
		&self, primary_namespace: String, secondary_namespace: String, key: String, buf: Vec<u8>,
	) -> Result<(), IOError> {
		KVStore::write(self, &primary_namespace, &secondary_namespace, &key, buf)
			.await
			.map_err(Into::into)
	}

	async fn remove(
		&self, primary_namespace: String, secondary_namespace: String, key: String, lazy: bool,
	) -> Result<(), IOError> {
		KVStore::remove(self, &primary_namespace, &secondary_namespace, &key, lazy)
			.await
			.map_err(Into::into)
	}

	async fn list(
		&self, primary_namespace: String, secondary_namespace: String,
	) -> Result<Vec<String>, IOError> {
		KVStore::list(self, &primary_namespace, &secondary_namespace).await.map_err(Into::into)
	}

	async fn list_paginated(
		&self, primary_namespace: String, secondary_namespace: String,
		page_token: Option<Arc<PageToken>>,
	) -> Result<PaginatedListResponse, IOError> {
		let page_token = page_token.map(|token| token.as_ref().as_ref().clone());
		PaginatedKVStore::list_paginated(self, &primary_namespace, &secondary_namespace, page_token)
			.await
			.map(|response| PaginatedListResponse {
				keys: response.keys,
				next_page_token: response.next_page_token.map(|token| Arc::new(token.into())),
			})
			.map_err(Into::into)
	}

	async fn list_all_keys(&self) -> Result<Vec<KVStoreKey>, IOError> {
		MigratableKVStore::list_all_keys(self)
			.await
			.map(|keys| {
				keys.into_iter()
					.map(|(primary_namespace, secondary_namespace, key)| KVStoreKey {
						primary_namespace,
						secondary_namespace,
						key,
					})
					.collect()
			})
			.map_err(Into::into)
	}
}
