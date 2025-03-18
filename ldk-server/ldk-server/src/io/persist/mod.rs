pub(crate) mod paginated_kv_store;
pub(crate) mod sqlite_store;

/// The forwarded payments will be persisted under this prefix.
pub(crate) const FORWARDED_PAYMENTS_PERSISTENCE_PRIMARY_NAMESPACE: &str = "forwarded_payments";
pub(crate) const FORWARDED_PAYMENTS_PERSISTENCE_SECONDARY_NAMESPACE: &str = "";

/// The payments will be persisted under this prefix.
pub(crate) const PAYMENTS_PERSISTENCE_PRIMARY_NAMESPACE: &str = "payments";
pub(crate) const PAYMENTS_PERSISTENCE_SECONDARY_NAMESPACE: &str = "";
