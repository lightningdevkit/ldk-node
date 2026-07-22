// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

pub(crate) mod paginated_kv_store;
pub(crate) mod sqlite_store;

/// The forwarded payments will be persisted under this prefix.
pub(crate) const FORWARDED_PAYMENTS_PERSISTENCE_PRIMARY_NAMESPACE: &str = "forwarded_payments";
pub(crate) const FORWARDED_PAYMENTS_PERSISTENCE_SECONDARY_NAMESPACE: &str = "";

/// The payments will be persisted under this prefix.
pub(crate) const PAYMENTS_PERSISTENCE_PRIMARY_NAMESPACE: &str = "payments";
pub(crate) const PAYMENTS_PERSISTENCE_SECONDARY_NAMESPACE: &str = "";
