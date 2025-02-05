// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Objects for querying the network graph.

use crate::types::Graph;

use lightning::{routing::gossip::NodeId, util::ser::Writeable};

#[cfg(feature = "uniffi")]
use lightning::ln::msgs::SocketAddress;
#[cfg(feature = "uniffi")]
use lightning::routing::gossip::RoutingFees;

#[cfg(not(feature = "uniffi"))]
use lightning::routing::gossip::{ChannelInfo, NodeInfo};

use std::sync::Arc;

/// Represents the network as nodes and channels between them.
pub struct NetworkGraph {
	inner: Arc<Graph>,
}

impl NetworkGraph {
	pub(crate) fn new(inner: Arc<Graph>) -> Self {
		Self { inner }
	}

	/// Returns the list of channels in the graph
	pub fn list_channels(&self) -> Vec<u64> {
		self.inner.read_only().channels().unordered_keys().map(|c| *c).collect()
	}

	/// Returns information on a channel with the given id.
	pub fn channel(&self, short_channel_id: u64) -> Option<ChannelInfo> {
		self.inner.read_only().channels().get(&short_channel_id).cloned().map(|c| c.into())
	}

	/// Returns the list of nodes in the graph
	pub fn list_nodes(&self) -> Vec<NodeId> {
		self.inner.read_only().nodes().unordered_keys().map(|n| *n).collect()
	}

	/// Returns information on a node with the given id.
	pub fn node(&self, node_id: &NodeId) -> Option<NodeInfo> {
		self.inner.read_only().nodes().get(node_id).cloned().map(|n| n.into())
	}

	/// Encode the graph into a byte buffer.
	pub fn encode(&self) -> Vec<u8> {
		self.inner.encode()
	}
}

/// Details about a channel (both directions).
///
/// Received within a channel announcement.
///
/// This is a simplified version of LDK's `ChannelInfo` for bindings.
#[cfg(feature = "uniffi")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelInfo {
	/// Source node of the first direction of a channel
	pub node_one: NodeId,
	/// Details about the first direction of a channel
	pub one_to_two: Option<ChannelUpdateInfo>,
	/// Source node of the second direction of a channel
	pub node_two: NodeId,
	/// Details about the second direction of a channel
	pub two_to_one: Option<ChannelUpdateInfo>,
	/// The channel capacity as seen on-chain, if chain lookup is available.
	pub capacity_sats: Option<u64>,
}

#[cfg(feature = "uniffi")]
impl From<lightning::routing::gossip::ChannelInfo> for ChannelInfo {
	fn from(value: lightning::routing::gossip::ChannelInfo) -> Self {
		Self {
			node_one: value.node_one,
			one_to_two: value.one_to_two.map(|u| u.into()),
			node_two: value.node_two,
			two_to_one: value.two_to_one.map(|u| u.into()),
			capacity_sats: value.capacity_sats,
		}
	}
}

/// Details about one direction of a channel as received within a `ChannelUpdate`.
///
/// This is a simplified version of LDK's `ChannelUpdateInfo` for bindings.
#[cfg(feature = "uniffi")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelUpdateInfo {
	/// When the last update to the channel direction was issued.
	/// Value is opaque, as set in the announcement.
	pub last_update: u32,
	/// Whether the channel can be currently used for payments (in this one direction).
	pub enabled: bool,
	/// The difference in CLTV values that you must have when routing through this channel.
	pub cltv_expiry_delta: u16,
	/// The minimum value, which must be relayed to the next hop via the channel
	pub htlc_minimum_msat: u64,
	/// The maximum value which may be relayed to the next hop via the channel.
	pub htlc_maximum_msat: u64,
	/// Fees charged when the channel is used for routing
	pub fees: RoutingFees,
}

#[cfg(feature = "uniffi")]
impl From<lightning::routing::gossip::ChannelUpdateInfo> for ChannelUpdateInfo {
	fn from(value: lightning::routing::gossip::ChannelUpdateInfo) -> Self {
		Self {
			last_update: value.last_update,
			enabled: value.enabled,
			cltv_expiry_delta: value.cltv_expiry_delta,
			htlc_minimum_msat: value.htlc_minimum_msat,
			htlc_maximum_msat: value.htlc_maximum_msat,
			fees: value.fees,
		}
	}
}

/// Details about a node in the network, known from the network announcement.
///
/// This is a simplified version of LDK's `NodeInfo` for bindings.
#[cfg(feature = "uniffi")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeInfo {
	/// All valid channels a node has announced
	pub channels: Vec<u64>,
	/// More information about a node from node_announcement.
	/// Optional because we store a Node entry after learning about it from
	/// a channel announcement, but before receiving a node announcement.
	pub announcement_info: Option<NodeAnnouncementInfo>,
}

#[cfg(feature = "uniffi")]
impl From<lightning::routing::gossip::NodeInfo> for NodeInfo {
	fn from(value: lightning::routing::gossip::NodeInfo) -> Self {
		Self {
			channels: value.channels,
			announcement_info: value.announcement_info.map(|a| a.into()),
		}
	}
}

/// Information received in the latest node_announcement from this node.
///
/// This is a simplified version of LDK's `NodeAnnouncementInfo` for bindings.
#[cfg(feature = "uniffi")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeAnnouncementInfo {
	/// When the last known update to the node state was issued.
	/// Value is opaque, as set in the announcement.
	pub last_update: u32,
	/// Moniker assigned to the node.
	/// May be invalid or malicious (eg control chars),
	/// should not be exposed to the user.
	pub alias: String,
	/// List of addresses on which this node is reachable
	pub addresses: Vec<SocketAddress>,
}

#[cfg(feature = "uniffi")]
impl From<lightning::routing::gossip::NodeAnnouncementInfo> for NodeAnnouncementInfo {
	fn from(value: lightning::routing::gossip::NodeAnnouncementInfo) -> Self {
		Self {
			last_update: value.last_update(),
			alias: value.alias().to_string(),
			addresses: value.addresses().iter().cloned().collect(),
		}
	}
}
