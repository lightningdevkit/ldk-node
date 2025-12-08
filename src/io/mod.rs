// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Objects and traits for data persistence.

pub mod sqlite_store;
#[cfg(test)]
pub(crate) mod test_utils;
pub(crate) mod utils;
pub mod vss_store;

/// The event queue will be persisted under this key.
pub(crate) const EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE: &str = "";
pub(crate) const EVENT_QUEUE_PERSISTENCE_SECONDARY_NAMESPACE: &str = "";
pub(crate) const EVENT_QUEUE_PERSISTENCE_KEY: &str = "events";

/// The peer information will be persisted under this key.
pub(crate) const PEER_INFO_PERSISTENCE_PRIMARY_NAMESPACE: &str = "";
pub(crate) const PEER_INFO_PERSISTENCE_SECONDARY_NAMESPACE: &str = "";
pub(crate) const PEER_INFO_PERSISTENCE_KEY: &str = "peers";

/// The payment information will be persisted under this prefix.
pub(crate) const PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE: &str = "payments";
pub(crate) const PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE: &str = "";

/// The node metrics will be persisted under this key.
pub(crate) const NODE_METRICS_PRIMARY_NAMESPACE: &str = "";
pub(crate) const NODE_METRICS_SECONDARY_NAMESPACE: &str = "";
pub(crate) const NODE_METRICS_KEY: &str = "node_metrics";

/// The BDK wallet's [`ChangeSet::descriptor`] will be persisted under this key.
///
/// [`ChangeSet::descriptor`]: bdk_wallet::ChangeSet::descriptor
pub(crate) const BDK_WALLET_DESCRIPTOR_PRIMARY_NAMESPACE: &str = "bdk_wallet";
pub(crate) const BDK_WALLET_DESCRIPTOR_SECONDARY_NAMESPACE: &str = "";
pub(crate) const BDK_WALLET_DESCRIPTOR_KEY: &str = "descriptor";

/// The BDK wallet's [`ChangeSet::change_descriptor`] will be persisted under this key.
///
/// [`ChangeSet::change_descriptor`]: bdk_wallet::ChangeSet::change_descriptor
pub(crate) const BDK_WALLET_CHANGE_DESCRIPTOR_PRIMARY_NAMESPACE: &str = "bdk_wallet";
pub(crate) const BDK_WALLET_CHANGE_DESCRIPTOR_SECONDARY_NAMESPACE: &str = "";
pub(crate) const BDK_WALLET_CHANGE_DESCRIPTOR_KEY: &str = "change_descriptor";

/// The BDK wallet's [`ChangeSet::network`] will be persisted under this key.
///
/// [`ChangeSet::network`]: bdk_wallet::ChangeSet::network
pub(crate) const BDK_WALLET_NETWORK_PRIMARY_NAMESPACE: &str = "bdk_wallet";
pub(crate) const BDK_WALLET_NETWORK_SECONDARY_NAMESPACE: &str = "";
pub(crate) const BDK_WALLET_NETWORK_KEY: &str = "network";

/// The BDK wallet's [`ChangeSet::local_chain`] will be persisted under this key.
///
/// [`ChangeSet::local_chain`]: bdk_wallet::ChangeSet::local_chain
pub(crate) const BDK_WALLET_LOCAL_CHAIN_PRIMARY_NAMESPACE: &str = "bdk_wallet";
pub(crate) const BDK_WALLET_LOCAL_CHAIN_SECONDARY_NAMESPACE: &str = "";
pub(crate) const BDK_WALLET_LOCAL_CHAIN_KEY: &str = "local_chain";

/// The BDK wallet's [`ChangeSet::tx_graph`] will be persisted under this key.
///
/// [`ChangeSet::tx_graph`]: bdk_wallet::ChangeSet::tx_graph
pub(crate) const BDK_WALLET_TX_GRAPH_PRIMARY_NAMESPACE: &str = "bdk_wallet";
pub(crate) const BDK_WALLET_TX_GRAPH_SECONDARY_NAMESPACE: &str = "";
pub(crate) const BDK_WALLET_TX_GRAPH_KEY: &str = "tx_graph";

/// The BDK wallet's [`ChangeSet::indexer`] will be persisted under this key.
///
/// [`ChangeSet::indexer`]: bdk_wallet::ChangeSet::indexer
pub(crate) const BDK_WALLET_INDEXER_PRIMARY_NAMESPACE: &str = "bdk_wallet";
pub(crate) const BDK_WALLET_INDEXER_SECONDARY_NAMESPACE: &str = "";
pub(crate) const BDK_WALLET_INDEXER_KEY: &str = "indexer";

/// [`StaticInvoice`]s will be persisted under this key.
///
/// [`StaticInvoice`]: lightning::offers::static_invoice::StaticInvoice
pub(crate) const STATIC_INVOICE_STORE_PRIMARY_NAMESPACE: &str = "static_invoices";
