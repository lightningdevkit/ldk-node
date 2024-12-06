pub(crate) mod paginated_kv_store;
pub(crate) mod sqlite_store;
pub(crate) mod utils;

/// The forwarded payments will be persisted under this prefix.
pub(crate) const FORWARDED_PAYMENTS_PERSISTENCE_PRIMARY_NAMESPACE: &str = "forwarded_payments";
pub(crate) const FORWARDED_PAYMENTS_PERSISTENCE_SECONDARY_NAMESPACE: &str = "";
