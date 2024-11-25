use std::io;

/// Provides an interface that allows storage and retrieval of persisted values that are associated
/// with given keys, with support for pagination with time-based ordering.
///
/// In order to avoid collisions, the key space is segmented based on the given `primary_namespace`s
/// and `secondary_namespace`s. Implementations of this trait are free to handle them in different
/// ways, as long as per-namespace key uniqueness is asserted.
///
/// Keys and namespaces are required to be valid ASCII strings in the range of
/// [`KVSTORE_NAMESPACE_KEY_ALPHABET`] and no longer than [`KVSTORE_NAMESPACE_KEY_MAX_LEN`]. Empty
/// primary namespaces and secondary namespaces (`""`) are considered valid; however, if
/// `primary_namespace` is empty, `secondary_namespace` must also be empty. This means that concerns
/// should always be separated by primary namespace first, before secondary namespaces are used.
/// While the number of primary namespaces will be relatively small and determined at compile time,
/// there may be many secondary namespaces per primary namespace. Note that per-namespace uniqueness
/// needs to also hold for keys *and* namespaces in any given namespace, i.e., conflicts between keys
/// and equally named primary or secondary namespaces must be avoided.
///
/// **Note:** This trait extends the functionality of [`KVStore`] by adding support for
/// paginated listing of keys based on a monotonic counter or logical timestamp. This is useful
/// when dealing with a large number of keys that cannot be efficiently retrieved all at once.
///
/// See also [`KVStore`].
///
/// [`KVStore`]: ldk_node::lightning::util::persist::KVStore
/// [`KVSTORE_NAMESPACE_KEY_ALPHABET`]: ldk_node::lightning::util::persist::KVSTORE_NAMESPACE_KEY_ALPHABET
/// [`KVSTORE_NAMESPACE_KEY_MAX_LEN`]: ldk_node::lightning::util::persist::KVSTORE_NAMESPACE_KEY_MAX_LEN
pub trait PaginatedKVStore {
	/// Returns the data stored for the given `primary_namespace`, `secondary_namespace`, and `key`.
	///
	/// Returns an [`ErrorKind::NotFound`] if the given `key` could not be found in the given
	/// `primary_namespace` and `secondary_namespace`.
	///
	/// [`ErrorKind::NotFound`]: io::ErrorKind::NotFound
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> Result<Vec<u8>, io::Error>;

	/// Persists the given data under the given `key` with an associated `time`.
	///
	/// The `time` parameter is a `i64` representing a monotonic counter or logical timestamp.
	/// It is used to track the order of keys for list operations. Implementations should store the
	/// `time` value and use it for ordering in the `list` method.
	///
	/// Will create the given `primary_namespace` and `secondary_namespace` if not already present
	/// in the store.
	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, time: i64, buf: &[u8],
	) -> Result<(), io::Error>;

	/// Removes any data that had previously been persisted under the given `key`.
	///
	/// If the `lazy` flag is set to `true`, the backend implementation might choose to lazily
	/// remove the given `key` at some point in time after the method returns, e.g., as part of an
	/// eventual batch deletion of multiple keys. As a consequence, subsequent calls to
	/// [`PaginatedKVStore::list`] might include the removed key until the changes are actually persisted.
	///
	/// Note that while setting the `lazy` flag reduces the I/O burden of multiple subsequent
	/// `remove` calls, it also influences the atomicity guarantees as lazy `remove`s could
	/// potentially get lost on crash after the method returns. Therefore, this flag should only be
	/// set for `remove` operations that can be safely replayed at a later time.
	///
	/// Returns successfully if no data will be stored for the given `primary_namespace`,
	/// `secondary_namespace`, and `key`, independently of whether it was present before its
	/// invocation or not.
	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> Result<(), io::Error>;

	/// Returns a paginated list of keys that are stored under the given `secondary_namespace` in
	/// `primary_namespace`, ordered in descending order of `time`.
	///
	/// The `list` method returns the latest records first, based on the `time` associated with each key.
	/// Pagination is controlled by the `next_page_token`, which is an `Option<String>`
	/// used to determine the starting point for the next page of results. If `next_page_token` is `None`,
	/// the listing starts from the most recent entry. The `next_page_token` in the returned
	/// [`ListResponse`] can be used to fetch the next page of results.
	///
	/// Implementations should ensure that keys are returned in descending order of `time` and that
	/// pagination tokens are correctly managed.
	///
	/// Returns an empty list if `primary_namespace` or `secondary_namespace` is unknown or if
	/// there are no more keys to return.
	///
	/// [`ListResponse`]: struct.ListResponse.html
	fn list(
		&self, primary_namespace: &str, secondary_namespace: &str, next_page_token: Option<String>,
	) -> Result<ListResponse, io::Error>;
}

/// Represents the response from a paginated `list` operation.
///
/// Contains the list of keys and an optional `next_page_token` that can be used to retrieve the
/// next set of keys.
pub struct ListResponse {
	/// A vector of keys, ordered in descending order of `time`.
	pub keys: Vec<String>,

	///  A token that can be used to retrieve the next set of keys.
	pub next_page_token: Option<String>,
}
