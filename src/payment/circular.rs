// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use lightning::ln::channel_state::ChannelDetails;
use lightning::ln::channelmanager::MIN_FINAL_CLTV_EXPIRY_DELTA;
use lightning::routing::router::{
	InFlightHtlcs, PaymentParameters, Route, RouteHint, RouteHintHop, RouteParameters,
	RouteParametersConfig, Router,
};
use lightning_types::features::Bolt11InvoiceFeatures;
use lightning_types::routing::RoutingFees;

use crate::{Error, UserChannelId};

/// A hop in a circular route quote. The final hop returns to the local node.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct CircularRouteHop {
	/// The node reached by this hop.
	pub node_id: PublicKey,
	/// The short channel ID or alias used by this hop.
	pub short_channel_id: u64,
	/// The forwarding fee in millisatoshis, or the delivered amount for the final hop.
	pub fee_msat: u64,
	/// The CLTV delta attributed to this hop.
	pub cltv_expiry_delta: u32,
}

/// One path in a circular route quote.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct CircularRoutePath {
	/// The route hops, including the final hop back to the local node.
	pub hops: Vec<CircularRouteHop>,
	/// The principal delivered back to the local node in millisatoshis.
	pub amount_msat: u64,
	/// The routing fee for this path in millisatoshis.
	pub fee_msat: u64,
}

/// A circular route estimate pinned to two distinct local channels.
///
/// A quote does not reserve liquidity or guarantee that a payment will succeed. It is a snapshot
/// of local channel state, the network graph, and scoring information. No invoice, probe, or
/// payment is created, and the payment store is not modified.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct CircularRouteQuote {
	/// The requested principal in millisatoshis.
	pub amount_msat: u64,
	/// The total routing fee in millisatoshis.
	pub total_routing_fee_msat: u64,
	/// The exact local channel selected for the first hop.
	pub first_hop_user_channel_id: UserChannelId,
	/// The SCID or alias used for the first hop.
	pub first_hop_short_channel_id: u64,
	/// The exact local channel selected for the final hop.
	pub last_hop_user_channel_id: UserChannelId,
	/// The inbound SCID or alias used for the final hop.
	pub last_hop_short_channel_id: u64,
	/// Candidate paths. Every path uses both selected local channels.
	pub paths: Vec<CircularRoutePath>,
}

fn fresh_synthetic_payee() -> Result<PublicKey, Error> {
	let secp_ctx = Secp256k1::new();
	loop {
		let mut secret_bytes = [0u8; 32];
		getrandom::fill(&mut secret_bytes).map_err(|_| Error::PaymentSendingFailed)?;
		if let Ok(secret_key) = SecretKey::from_slice(&secret_bytes) {
			return Ok(PublicKey::from_secret_key(&secp_ctx, &secret_key));
		}
	}
}

fn apply_configured_routing_fee_cap(
	route_params: &mut RouteParameters, route_parameters: &RouteParametersConfig,
) {
	if let Some(max_total_routing_fee_msat) = route_parameters.max_total_routing_fee_msat {
		route_params.max_total_routing_fee_msat = Some(max_total_routing_fee_msat);
	}
}

/// Replace the synthetic destination only after validating every path's exact endpoints.
fn finalize_circular_route(
	mut route: Route, first_hop_node_id: PublicKey, first_hop_scid: u64,
	synthetic_payee: PublicKey, last_hop_node_id: PublicKey, last_hop_scid: u64,
	local_node_id: PublicKey,
) -> Result<Route, Error> {
	if route.paths.is_empty()
		|| route.paths.iter().any(|path| {
			path.blinded_tail.is_some()
				|| path.hops.len() < 2
				|| !path.hops.first().is_some_and(|hop| {
					hop.pubkey == first_hop_node_id && hop.short_channel_id == first_hop_scid
				}) || !path.hops.last().is_some_and(|hop| {
				hop.pubkey == synthetic_payee && hop.short_channel_id == last_hop_scid
			}) || path.hops[path.hops.len() - 2].pubkey != last_hop_node_id
				|| path.hops.iter().any(|hop| hop.pubkey == local_node_id)
		}) {
		return Err(Error::PaymentSendingFailed);
	}
	for path in &mut route.paths {
		let last_hop = path.hops.last_mut().ok_or(Error::PaymentSendingFailed)?;
		last_hop.pubkey = local_node_id;
	}
	// These parameters name the synthetic destination and must not escape into a real send.
	route.route_params = None;
	Ok(route)
}

pub(super) fn quote_circular_route<R: Router>(
	router: &R, local_node_id: PublicKey, invoice_features: Bolt11InvoiceFeatures,
	channels: &[ChannelDetails], inflight_htlcs: InFlightHtlcs, amount_msat: u64,
	first_hop_user_channel_id: UserChannelId, last_hop_user_channel_id: UserChannelId,
	route_parameters: RouteParametersConfig,
) -> Result<CircularRouteQuote, Error> {
	if amount_msat == 0 {
		return Err(Error::InvalidAmount);
	}
	if first_hop_user_channel_id == last_hop_user_channel_id {
		return Err(Error::InvalidChannelId);
	}
	let select_channel = |id: UserChannelId| {
		let mut matching = channels.iter().filter(|channel| channel.user_channel_id == id.0);
		match (matching.next(), matching.next()) {
			(Some(channel), None) if channel.is_usable => Ok(channel),
			_ => Err(Error::InvalidChannelId),
		}
	};
	let first_hop = select_channel(first_hop_user_channel_id)?;
	let last_hop = select_channel(last_hop_user_channel_id)?;
	if first_hop.outbound_capacity_msat < amount_msat
		|| last_hop.inbound_capacity_msat < amount_msat
	{
		return Err(Error::InsufficientFunds);
	}
	let first_hop_scid = first_hop.get_outbound_payment_scid().ok_or(Error::InvalidChannelId)?;
	let last_hop_scid = last_hop.get_inbound_payment_scid().ok_or(Error::InvalidChannelId)?;
	let forwarding_info =
		last_hop.counterparty.forwarding_info.as_ref().ok_or(Error::InvalidChannelId)?;
	// LDK does not pathfind from a node to itself. Search for a fresh synthetic recipient behind
	// the selected inbound channel, then replace only that terminal identity in the result.
	let synthetic_payee = fresh_synthetic_payee()?;
	if synthetic_payee == local_node_id {
		return Err(Error::InvalidNodeId);
	}
	let route_hint = RouteHint(vec![RouteHintHop {
		src_node_id: last_hop.counterparty.node_id,
		short_channel_id: last_hop_scid,
		fees: RoutingFees {
			base_msat: forwarding_info.fee_base_msat,
			proportional_millionths: forwarding_info.fee_proportional_millionths,
		},
		cltv_expiry_delta: forwarding_info.cltv_expiry_delta,
		htlc_minimum_msat: last_hop.inbound_htlc_minimum_msat,
		htlc_maximum_msat: last_hop.inbound_htlc_maximum_msat,
	}]);
	let mut payment_params =
		PaymentParameters::from_node_id(synthetic_payee, u32::from(MIN_FINAL_CLTV_EXPIRY_DELTA))
			.with_bolt11_features(invoice_features)
			.map_err(|_| Error::PaymentSendingFailed)?
			.with_route_hints(vec![route_hint])
			.map_err(|_| Error::PaymentSendingFailed)?;
	payment_params.max_total_cltv_expiry_delta = route_parameters.max_total_cltv_expiry_delta;
	payment_params.max_path_count = route_parameters.max_path_count;
	payment_params.max_channel_saturation_power_of_half =
		route_parameters.max_channel_saturation_power_of_half;
	let mut route_params =
		RouteParameters::from_payment_params_and_value(payment_params, amount_msat);
	apply_configured_routing_fee_cap(&mut route_params, &route_parameters);
	let first_hops = [first_hop];
	let route = router
		.find_route(&local_node_id, &route_params, Some(&first_hops), inflight_htlcs)
		.map_err(|_| Error::PaymentSendingFailed)?;
	let route = finalize_circular_route(
		route,
		first_hop.counterparty.node_id,
		first_hop_scid,
		synthetic_payee,
		last_hop.counterparty.node_id,
		last_hop_scid,
		local_node_id,
	)?;
	if route.get_total_amount() != amount_msat
		|| route_params.max_total_routing_fee_msat.is_some_and(|max| route.get_total_fees() > max)
		|| amount_msat
			.checked_add(route.get_total_fees())
			.is_none_or(|total| total > first_hop.outbound_capacity_msat)
	{
		return Err(Error::PaymentSendingFailed);
	}
	let paths = route
		.paths
		.iter()
		.map(|path| CircularRoutePath {
			hops: path
				.hops
				.iter()
				.map(|hop| CircularRouteHop {
					node_id: hop.pubkey,
					short_channel_id: hop.short_channel_id,
					fee_msat: hop.fee_msat,
					cltv_expiry_delta: hop.cltv_expiry_delta,
				})
				.collect(),
			amount_msat: path.final_value_msat(),
			fee_msat: path.fee_msat(),
		})
		.collect();
	Ok(CircularRouteQuote {
		amount_msat,
		total_routing_fee_msat: route.get_total_fees(),
		first_hop_user_channel_id,
		first_hop_short_channel_id: first_hop_scid,
		last_hop_user_channel_id,
		last_hop_short_channel_id: last_hop_scid,
		paths,
	})
}

#[cfg(test)]
mod tests {
	use lightning::ln::functional_test_utils::{
		create_announced_chan_between_nodes_with_value, create_chanmon_cfgs, create_network,
		create_node_cfgs, create_node_chanmgrs, create_unannounced_chan_between_nodes_with_value,
	};
	use lightning::routing::router::{Path, RouteHop};
	use lightning::types::features::{ChannelFeatures, NodeFeatures};
	use lightning::util::ser::Writeable;

	use super::*;

	fn node_id(byte: u8) -> PublicKey {
		PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[byte; 32]).unwrap())
	}

	fn hop(pubkey: PublicKey, short_channel_id: u64) -> RouteHop {
		RouteHop {
			pubkey,
			node_features: NodeFeatures::empty(),
			short_channel_id,
			channel_features: ChannelFeatures::empty(),
			fee_msat: 1_000,
			cltv_expiry_delta: 40,
			maybe_announced_channel: true,
		}
	}

	#[test]
	fn finalization_preserves_every_path_and_removes_synthetic_parameters() {
		let path =
			Path { hops: vec![hop(node_id(1), 41), hop(node_id(2), 99)], blinded_tail: None };
		let params = RouteParameters::from_payment_params_and_value(
			PaymentParameters::from_node_id(node_id(2), 40),
			2_000,
		);
		let original = Route { paths: vec![path.clone(), path], route_params: Some(params) };
		let result = finalize_circular_route(
			original.clone(),
			node_id(1),
			41,
			node_id(2),
			node_id(1),
			99,
			node_id(3),
		)
		.unwrap();
		let mut expected = original;
		for path in &mut expected.paths {
			path.hops.last_mut().unwrap().pubkey = node_id(3);
		}
		expected.route_params = None;
		assert_eq!(result, expected);
	}

	#[test]
	fn finalization_rejects_mismatched_endpoints_in_any_path() {
		let path = Path {
			hops: vec![hop(node_id(1), 41), hop(node_id(4), 70), hop(node_id(2), 99)],
			blinded_tail: None,
		};
		for mismatch in 0..6 {
			let mut route = Route { paths: vec![path.clone(), path.clone()], route_params: None };
			let bad = &mut route.paths[1].hops;
			match mismatch {
				0 => bad[0].short_channel_id = 42,
				1 => bad[0].pubkey = node_id(5),
				2 => bad[2].short_channel_id = 100,
				3 => bad[2].pubkey = node_id(5),
				4 => bad[1].pubkey = node_id(5),
				_ => bad.insert(1, hop(node_id(3), 80)),
			}
			assert_eq!(
				finalize_circular_route(
					route,
					node_id(1),
					41,
					node_id(2),
					node_id(4),
					99,
					node_id(3),
				),
				Err(Error::PaymentSendingFailed)
			);
		}
		for paths in [
			vec![],
			vec![Path { hops: vec![], blinded_tail: None }],
			vec![Path { hops: vec![hop(node_id(2), 99)], blinded_tail: None }],
		] {
			assert_eq!(
				finalize_circular_route(
					Route { paths, route_params: None },
					node_id(1),
					41,
					node_id(2),
					node_id(4),
					99,
					node_id(3),
				),
				Err(Error::PaymentSendingFailed)
			);
		}
	}

	#[test]
	fn absent_configured_fee_cap_preserves_ldk_default() {
		let mut params = RouteParameters::from_payment_params_and_value(
			PaymentParameters::from_node_id(node_id(1), u32::from(MIN_FINAL_CLTV_EXPIRY_DELTA)),
			1_000_000,
		);
		let default_cap = params.max_total_routing_fee_msat;
		apply_configured_routing_fee_cap(&mut params, &RouteParametersConfig::default());
		assert_eq!(params.max_total_routing_fee_msat, default_cap);
		for cap in [0, 777] {
			apply_configured_routing_fee_cap(
				&mut params,
				&RouteParametersConfig {
					max_total_routing_fee_msat: Some(cap),
					..Default::default()
				},
			);
			assert_eq!(params.max_total_routing_fee_msat, Some(cap));
		}
	}

	#[test]
	fn synthetic_payees_are_fresh() {
		assert_ne!(fresh_synthetic_payee().unwrap(), fresh_synthetic_payee().unwrap());
	}

	#[test]
	fn real_router_quotes_exact_parallel_channels_without_changing_payment_state() {
		let chanmon_cfgs = create_chanmon_cfgs(3);
		let node_cfgs = create_node_cfgs(3, &chanmon_cfgs);
		let node_chanmgrs = create_node_chanmgrs(3, &node_cfgs, &[None, None, None]);
		let nodes = create_network(3, &node_cfgs, &node_chanmgrs);
		let (_, _, first_id, _) =
			create_announced_chan_between_nodes_with_value(&nodes, 0, 1, 100_000, 0);
		create_announced_chan_between_nodes_with_value(&nodes, 0, 1, 100_000, 0);
		create_announced_chan_between_nodes_with_value(&nodes, 1, 2, 40_000, 0);
		create_announced_chan_between_nodes_with_value(&nodes, 1, 2, 40_000, 0);
		let (_, _, last_id, _) =
			create_announced_chan_between_nodes_with_value(&nodes, 2, 0, 100_000, 0);
		create_announced_chan_between_nodes_with_value(&nodes, 2, 0, 100_000, 0);
		let mut channels = nodes[0].node.list_channels();
		// LDK's channel creation helpers reuse user IDs; give the snapshot unique application IDs.
		for (i, channel) in channels.iter_mut().enumerate() {
			channel.user_channel_id = i as u128;
		}
		let first = channels.iter().find(|c| c.channel_id == first_id).unwrap();
		let last = channels.iter().find(|c| c.channel_id == last_id).unwrap();
		let before: Vec<_> = nodes.iter().map(|n| n.node.encode()).collect();
		let quote = quote_circular_route(
			&nodes[0].router.router,
			nodes[0].node.get_our_node_id(),
			nodes[0].node.init_features().to_context(),
			&channels,
			nodes[0].node.compute_inflight_htlcs(),
			5_000_000,
			UserChannelId(first.user_channel_id),
			UserChannelId(last.user_channel_id),
			RouteParametersConfig {
				max_total_routing_fee_msat: Some(50_000),
				..Default::default()
			},
		)
		.unwrap();
		assert_eq!(quote.amount_msat, 5_000_000);
		assert!(quote.total_routing_fee_msat > 0 && quote.total_routing_fee_msat <= 50_000);
		assert_eq!(quote.paths.iter().map(|p| p.amount_msat).sum::<u64>(), quote.amount_msat);
		assert_eq!(
			quote.paths.iter().map(|p| p.fee_msat).sum::<u64>(),
			quote.total_routing_fee_msat
		);
		for path in &quote.paths {
			assert_eq!(
				path.hops.first().unwrap().short_channel_id,
				first.get_outbound_payment_scid().unwrap()
			);
			assert_eq!(path.hops.first().unwrap().node_id, nodes[1].node.get_our_node_id());
			assert_eq!(
				path.hops.last().unwrap().short_channel_id,
				last.get_inbound_payment_scid().unwrap()
			);
			assert_eq!(path.hops.last().unwrap().node_id, nodes[0].node.get_our_node_id());
		}
		for (node, encoded) in nodes.iter().zip(before.iter()) {
			assert_eq!(&node.node.encode(), encoded);
		}

		// No single middle-channel HTLC can carry this amount; MPP must retain both local endpoints.
		let mpp = quote_circular_route(
			&nodes[0].router.router,
			nodes[0].node.get_our_node_id(),
			nodes[0].node.init_features().to_context(),
			&channels,
			nodes[0].node.compute_inflight_htlcs(),
			15_000_000,
			UserChannelId(first.user_channel_id),
			UserChannelId(last.user_channel_id),
			RouteParametersConfig {
				max_total_routing_fee_msat: Some(100_000),
				..Default::default()
			},
		)
		.unwrap();
		assert!(mpp.paths.len() > 1);
		assert_eq!(mpp.paths.iter().map(|p| p.amount_msat).sum::<u64>(), 15_000_000);
		for path in &mpp.paths {
			assert_eq!(
				path.hops.first().unwrap().short_channel_id,
				first.get_outbound_payment_scid().unwrap()
			);
			assert_eq!(
				path.hops.last().unwrap().short_channel_id,
				last.get_inbound_payment_scid().unwrap()
			);
		}
		assert_eq!(
			quote_circular_route(
				&nodes[0].router.router,
				nodes[0].node.get_our_node_id(),
				nodes[0].node.init_features().to_context(),
				&channels,
				nodes[0].node.compute_inflight_htlcs(),
				15_000_000,
				UserChannelId(first.user_channel_id),
				UserChannelId(last.user_channel_id),
				RouteParametersConfig { max_path_count: 1, ..Default::default() },
			),
			Err(Error::PaymentSendingFailed)
		);

		// An explicit zero-fee budget must not silently fall back to the default budget.
		assert_eq!(
			quote_circular_route(
				&nodes[0].router.router,
				nodes[0].node.get_our_node_id(),
				nodes[0].node.init_features().to_context(),
				&channels,
				nodes[0].node.compute_inflight_htlcs(),
				5_000_000,
				UserChannelId(first.user_channel_id),
				UserChannelId(last.user_channel_id),
				RouteParametersConfig { max_total_routing_fee_msat: Some(0), ..Default::default() },
			),
			Err(Error::PaymentSendingFailed)
		);
	}

	#[test]
	fn private_parallel_channels_use_the_selected_aliases_and_reject_invalid_selections() {
		let chanmon_cfgs = create_chanmon_cfgs(2);
		let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
		let node_chanmgrs = create_node_chanmgrs(2, &node_cfgs, &[None, None]);
		let nodes = create_network(2, &node_cfgs, &node_chanmgrs);
		create_unannounced_chan_between_nodes_with_value(&nodes, 0, 1, 100_000, 0);
		create_unannounced_chan_between_nodes_with_value(&nodes, 1, 0, 100_000, 0);
		let mut channels = nodes[0].node.list_channels();
		channels.sort_by_key(|c| !c.is_outbound);
		channels[0].user_channel_id = 1;
		channels[1].user_channel_id = 2;
		// Model an outbound channel without a confirmed SCID to exercise its alias.
		channels[0].short_channel_id = None;
		// The incoming alias takes precedence even when the confirmed SCID is known.
		assert!(channels[0].outbound_scid_alias.is_some());
		assert!(channels[1].inbound_scid_alias.is_some());
		let quote = |channels: &[ChannelDetails], amount, first, last| {
			quote_circular_route(
				&nodes[0].router.router,
				nodes[0].node.get_our_node_id(),
				nodes[0].node.init_features().to_context(),
				channels,
				nodes[0].node.compute_inflight_htlcs(),
				amount,
				UserChannelId(first),
				UserChannelId(last),
				RouteParametersConfig::default(),
			)
		};
		let result = quote(&channels, 5_000_000, 1, 2).unwrap();
		assert_eq!(result.first_hop_short_channel_id, channels[0].outbound_scid_alias.unwrap());
		assert_eq!(result.last_hop_short_channel_id, channels[1].inbound_scid_alias.unwrap());
		assert_eq!(result.paths.len(), 1);
		assert_eq!(result.paths[0].hops.len(), 2);
		assert_eq!(quote(&channels, 0, 1, 2), Err(Error::InvalidAmount));
		assert_eq!(quote(&channels, 5_000_000, 1, 1), Err(Error::InvalidChannelId));
		assert_eq!(quote(&channels, 5_000_000, 99, 2), Err(Error::InvalidChannelId));
		assert_eq!(quote(&channels, 5_000_000, 1, 99), Err(Error::InvalidChannelId));
		for index in 0..2 {
			let mut unusable = channels.clone();
			unusable[index].is_usable = false;
			assert_eq!(quote(&unusable, 5_000_000, 1, 2), Err(Error::InvalidChannelId));
		}
		let mut ambiguous = channels.clone();
		ambiguous.push(channels[0].clone());
		assert_eq!(quote(&ambiguous, 5_000_000, 1, 2), Err(Error::InvalidChannelId));
		let mut insufficient = channels.clone();
		insufficient[0].outbound_capacity_msat = 4_999_999;
		assert_eq!(quote(&insufficient, 5_000_000, 1, 2), Err(Error::InsufficientFunds));
		insufficient = channels.clone();
		insufficient[1].inbound_capacity_msat = 4_999_999;
		assert_eq!(quote(&insufficient, 5_000_000, 1, 2), Err(Error::InsufficientFunds));
		channels[1].counterparty.forwarding_info = None;
		assert_eq!(quote(&channels, 5_000_000, 1, 2), Err(Error::InvalidChannelId));
	}
}
