// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

use std::path::PathBuf;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};
use config::{
	get_default_api_key_path, get_default_cert_path, get_default_config_path, load_config,
};
use hex_conservative::DisplayHex;
use ldk_server_client::client::LdkServerClient;
use ldk_server_client::error::LdkServerError;
use ldk_server_client::error::LdkServerErrorCode::{
	AuthError, InternalError, InternalServerError, InvalidRequestError, LightningError,
};
use ldk_server_client::ldk_server_protos::api::{
	Bolt11ReceiveRequest, Bolt11ReceiveResponse, Bolt11SendRequest, Bolt11SendResponse,
	Bolt12ReceiveRequest, Bolt12ReceiveResponse, Bolt12SendRequest, Bolt12SendResponse,
	CloseChannelRequest, CloseChannelResponse, ConnectPeerRequest, ConnectPeerResponse,
	ForceCloseChannelRequest, ForceCloseChannelResponse, GetBalancesRequest, GetBalancesResponse,
	GetNodeInfoRequest, GetNodeInfoResponse, GetPaymentDetailsRequest, GetPaymentDetailsResponse,
	ListChannelsRequest, ListChannelsResponse, ListForwardedPaymentsRequest, ListPaymentsRequest,
	OnchainReceiveRequest, OnchainReceiveResponse, OnchainSendRequest, OnchainSendResponse,
	OpenChannelRequest, OpenChannelResponse, SpliceInRequest, SpliceInResponse, SpliceOutRequest,
	SpliceOutResponse, UpdateChannelConfigRequest, UpdateChannelConfigResponse,
};
use ldk_server_client::ldk_server_protos::types::{
	bolt11_invoice_description, Bolt11InvoiceDescription, ChannelConfig, PageToken,
	RouteParametersConfig,
};
use serde::Serialize;
use types::{CliListForwardedPaymentsResponse, CliListPaymentsResponse, CliPaginatedResponse};

mod config;
mod types;

// Having these default values as constants in the Proto file and
// importing/reusing them here might be better, but Proto3 removed
// the ability to set default values.
const DEFAULT_MAX_TOTAL_CLTV_EXPIRY_DELTA: u32 = 1008;
const DEFAULT_MAX_PATH_COUNT: u32 = 10;
const DEFAULT_MAX_CHANNEL_SATURATION_POWER_OF_HALF: u32 = 2;
const DEFAULT_EXPIRY_SECS: u32 = 86_400;

#[derive(Parser, Debug)]
#[command(
	name = "ldk-server-cli",
	version,
	about = "CLI for interacting with an LDK Server node",
	override_usage = "ldk-server-cli [OPTIONS] <COMMAND>"
)]
struct Cli {
	#[arg(short, long, help = "Base URL of the server. If not provided, reads from config file")]
	base_url: Option<String>,

	#[arg(
		short,
		long,
		help = "API key for authentication. Defaults by reading ~/.ldk-server/[network]/api_key"
	)]
	api_key: Option<String>,

	#[arg(
		short,
		long,
		help = "Path to the server's TLS certificate file (PEM format). Defaults to ~/.ldk-server/tls.crt"
	)]
	tls_cert: Option<String>,

	#[arg(short, long, help = "Path to config file. Defaults to ~/.ldk-server/config.toml")]
	config: Option<String>,

	#[command(subcommand)]
	command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
	#[command(about = "Retrieve the latest node info like node_id, current_best_block, etc")]
	GetNodeInfo,
	#[command(about = "Retrieve an overview of all known balances")]
	GetBalances,
	#[command(about = "Retrieve a new on-chain funding address")]
	OnchainReceive,
	#[command(about = "Send an on-chain payment to the given address")]
	OnchainSend {
		#[arg(short, long, help = "The address to send coins to")]
		address: String,
		#[arg(
			long,
			help = "The amount in satoshis to send. Will respect any on-chain reserve needed for anchor channels"
		)]
		amount_sats: Option<u64>,
		#[arg(
			long,
			help = "Send full balance to the address. Warning: will not retain on-chain reserves for anchor channels"
		)]
		send_all: Option<bool>,
		#[arg(
			long,
			help = "Fee rate in satoshis per virtual byte. If not set, a reasonable estimate will be used"
		)]
		fee_rate_sat_per_vb: Option<u64>,
	},
	#[command(about = "Create a BOLT11 invoice to receive a payment")]
	Bolt11Receive {
		#[arg(short, long, help = "Description to attach along with the invoice")]
		description: Option<String>,
		#[arg(
			long,
			help = "SHA-256 hash of the description (hex). Use instead of description for longer text"
		)]
		description_hash: Option<String>,
		#[arg(short, long, help = "Invoice expiry time in seconds (default: 86400)")]
		expiry_secs: Option<u32>,
		#[arg(
			long,
			help = "Amount in millisatoshis to request. If unset, a variable-amount invoice is returned"
		)]
		amount_msat: Option<u64>,
	},
	#[command(about = "Pay a BOLT11 invoice")]
	Bolt11Send {
		#[arg(short, long, help = "A BOLT11 invoice for a payment within the Lightning Network")]
		invoice: String,
		#[arg(long, help = "Amount in millisatoshis. Required when paying a zero-amount invoice")]
		amount_msat: Option<u64>,
		#[arg(
			long,
			help = "Maximum total fees in millisatoshis that may accrue during route finding. Defaults to 1% of payment + 50 sats"
		)]
		max_total_routing_fee_msat: Option<u64>,
		#[arg(long, help = "Maximum total CLTV delta we accept for the route (default: 1008)")]
		max_total_cltv_expiry_delta: Option<u32>,
		#[arg(
			long,
			help = "Maximum number of paths that may be used by MPP payments (default: 10)"
		)]
		max_path_count: Option<u32>,
		#[arg(
			long,
			help = "Maximum share of a channel's total capacity to send over a channel, as a power of 1/2 (default: 2)"
		)]
		max_channel_saturation_power_of_half: Option<u32>,
	},
	#[command(about = "Return a BOLT12 offer for receiving payments")]
	Bolt12Receive {
		#[arg(short, long, help = "Description to attach along with the offer")]
		description: String,
		#[arg(
			long,
			help = "Amount in millisatoshis to request. If unset, a variable-amount offer is returned"
		)]
		amount_msat: Option<u64>,
		#[arg(long, help = "Offer expiry time in seconds")]
		expiry_secs: Option<u32>,
		#[arg(long, help = "Number of items requested. Can only be set for fixed-amount offers")]
		quantity: Option<u64>,
	},
	#[command(about = "Send a payment for a BOLT12 offer")]
	Bolt12Send {
		#[arg(short, long, help = "A BOLT12 offer for a payment within the Lightning Network")]
		offer: String,
		#[arg(long, help = "Amount in millisatoshis. Required when paying a zero-amount offer")]
		amount_msat: Option<u64>,
		#[arg(short, long, help = "Number of items requested")]
		quantity: Option<u64>,
		#[arg(
			short,
			long,
			help = "Note to include for the payee. Will be seen by recipient and reflected back in the invoice"
		)]
		payer_note: Option<String>,
		#[arg(
			long,
			help = "Maximum total fees, in millisatoshi, that may accrue during route finding, Defaults to 1% of the payment amount + 50 sats"
		)]
		max_total_routing_fee_msat: Option<u64>,
		#[arg(long, help = "Maximum total CLTV delta we accept for the route (default: 1008)")]
		max_total_cltv_expiry_delta: Option<u32>,
		#[arg(
			long,
			help = "Maximum number of paths that may be used by MPP payments (default: 10)"
		)]
		max_path_count: Option<u32>,
		#[arg(
			long,
			help = "Maximum share of a channel's total capacity to send over a channel, as a power of 1/2 (default: 2)"
		)]
		max_channel_saturation_power_of_half: Option<u32>,
	},
	#[command(about = "Cooperatively close the channel specified by the given channel ID")]
	CloseChannel {
		#[arg(short, long, help = "The local user_channel_id of this channel")]
		user_channel_id: String,
		#[arg(
			short,
			long,
			help = "The hex-encoded public key of the node to close a channel with"
		)]
		counterparty_node_id: String,
	},
	#[command(about = "Force close the channel specified by the given channel ID")]
	ForceCloseChannel {
		#[arg(short, long, help = "The local user_channel_id of this channel")]
		user_channel_id: String,
		#[arg(
			short,
			long,
			help = "The hex-encoded public key of the node to close a channel with"
		)]
		counterparty_node_id: String,
		#[arg(long, help = "The reason for force-closing, defaults to \"\"")]
		force_close_reason: Option<String>,
	},
	#[command(about = "Create a new outbound channel to the given remote node")]
	OpenChannel {
		#[arg(short, long, help = "The hex-encoded public key of the node to open a channel with")]
		node_pubkey: String,
		#[arg(
			short,
			long,
			help = "Address to connect to remote peer (IPv4:port, IPv6:port, OnionV3:port, or hostname:port)"
		)]
		address: String,
		#[arg(long, help = "The amount of satoshis to commit to the channel")]
		channel_amount_sats: u64,
		#[arg(
			long,
			help = "Amount of satoshis to push to the remote side as part of the initial commitment state"
		)]
		push_to_counterparty_msat: Option<u64>,
		#[arg(long, help = "Whether the channel should be public")]
		announce_channel: bool,
		// Channel config options
		#[arg(
			long,
			help = "Amount (in millionths of a satoshi) charged per satoshi for payments forwarded outbound over the channel. This can be updated by using update-channel-config."
		)]
		forwarding_fee_proportional_millionths: Option<u32>,
		#[arg(
			long,
			help = "Amount (in milli-satoshi) charged for payments forwarded outbound over the channel, in excess of forwarding_fee_proportional_millionths. This can be updated by using update-channel-config."
		)]
		forwarding_fee_base_msat: Option<u32>,
		#[arg(
			long,
			help = "The difference in the CLTV value between incoming HTLCs and an outbound HTLC forwarded over the channel. This can be updated by using update-channel-config."
		)]
		cltv_expiry_delta: Option<u32>,
	},
	#[command(
		about = "Increase the channel balance by the given amount, funds will come from the node's on-chain wallet"
	)]
	SpliceIn {
		#[arg(short, long, help = "The local user_channel_id of the channel")]
		user_channel_id: String,
		#[arg(short, long, help = "The hex-encoded public key of the channel's counterparty node")]
		counterparty_node_id: String,
		#[arg(long, help = "The amount of sats to splice into the channel")]
		splice_amount_sats: u64,
	},
	#[command(about = "Decrease the channel balance by the given amount")]
	SpliceOut {
		#[arg(short, long, help = "The local user_channel_id of this channel")]
		user_channel_id: String,
		#[arg(short, long, help = "The hex-encoded public key of the channel's counterparty node")]
		counterparty_node_id: String,
		#[arg(long, help = "The amount of sats to splice out of the channel")]
		splice_amount_sats: u64,
		#[arg(
			short,
			long,
			help = "Bitcoin address to send the spliced-out funds. If not set, uses the node's on-chain wallet"
		)]
		address: Option<String>,
	},
	#[command(about = "Return a list of known channels")]
	ListChannels,
	#[command(about = "Retrieve list of all payments")]
	ListPayments {
		#[arg(short, long)]
		#[arg(
			help = "Fetch at least this many payments by iterating through multiple pages. Returns combined results with the last page token. If not provided, returns only a single page."
		)]
		number_of_payments: Option<u64>,
		#[arg(long)]
		#[arg(help = "Page token to continue from a previous page (format: token:index)")]
		page_token: Option<String>,
	},
	#[command(about = "Get details of a specific payment by its payment ID")]
	GetPaymentDetails {
		#[arg(short, long, help = "The payment ID in hex-encoded form")]
		payment_id: String,
	},
	#[command(about = "Retrieves list of all forwarded payments")]
	ListForwardedPayments {
		#[arg(
			short,
			long,
			help = "Fetch at least this many forwarded payments by iterating through multiple pages. Returns combined results with the last page token. If not provided, returns only a single page."
		)]
		number_of_payments: Option<u64>,
		#[arg(long, help = "Page token to continue from a previous page (format: token:index)")]
		page_token: Option<String>,
	},
	UpdateChannelConfig {
		#[arg(short, long, help = "The local user_channel_id of this channel")]
		user_channel_id: String,
		#[arg(
			short,
			long,
			help = "The hex-encoded public key of the counterparty node to update channel config with"
		)]
		counterparty_node_id: String,
		#[arg(
			long,
			help = "Amount (in millionths of a satoshi) charged per satoshi for payments forwarded outbound over the channel. This can be updated by using update-channel-config."
		)]
		forwarding_fee_proportional_millionths: Option<u32>,
		#[arg(
			long,
			help = "Amount (in milli-satoshi) charged for payments forwarded outbound over the channel, in excess of forwarding_fee_proportional_millionths. This can be updated by using update-channel-config."
		)]
		forwarding_fee_base_msat: Option<u32>,
		#[arg(
			long,
			help = "The difference in the CLTV value between incoming HTLCs and an outbound HTLC forwarded over the channel."
		)]
		cltv_expiry_delta: Option<u32>,
	},
	#[command(about = "Connect to a peer on the Lightning Network without opening a channel")]
	ConnectPeer {
		#[arg(short, long, help = "The hex-encoded public key of the node to connect to")]
		node_pubkey: String,
		#[arg(
			short,
			long,
			help = "Address to connect to remote peer (IPv4:port, IPv6:port, OnionV3:port, or hostname:port)"
		)]
		address: String,
		#[arg(
			long,
			default_value_t = false,
			help = "Whether to persist the connection for automatic reconnection on restart"
		)]
		persist: bool,
	},
	#[command(about = "Generate shell completions for the CLI")]
	Completions {
		#[arg(
			value_enum,
			help = "The shell to generate completions for (bash, zsh, fish, powershell, elvish)"
		)]
		shell: Shell,
	},
}

#[tokio::main]
async fn main() {
	let cli = Cli::parse();

	// short-circuit if generating completions
	if let Commands::Completions { shell } = cli.command {
		generate(shell, &mut Cli::command(), "ldk-server-cli", &mut std::io::stdout());
		return;
	}

	let config_path = cli.config.map(PathBuf::from).or_else(get_default_config_path);
	let config = config_path.as_ref().and_then(|p| load_config(p).ok());

	// Get API key from argument, then from api_key file
	let api_key = cli
		.api_key
		.or_else(|| {
			// Try to read from api_key file based on network (file contains raw bytes)
			let network = config.as_ref().and_then(|c| c.network().ok()).unwrap_or("bitcoin".to_string());
			get_default_api_key_path(&network)
				.and_then(|path| std::fs::read(&path).ok())
				.map(|bytes| bytes.to_lower_hex_string())
		})
		.unwrap_or_else(|| {
			eprintln!("API key not provided. Use --api-key or ensure the api_key file exists at ~/.ldk-server/[network]/api_key");
			std::process::exit(1);
		});

	// Get base URL from argument then from config file
	let base_url =
		cli.base_url.or_else(|| config.as_ref().map(|c| c.node.rest_service_address.clone()))
			.unwrap_or_else(|| {
				eprintln!("Base URL not provided. Use --base-url or ensure config file exists at ~/.ldk-server/config.toml");
				std::process::exit(1);
			});

	// Get TLS cert path from argument, then from config file, then try default location
	let tls_cert_path = cli.tls_cert.map(PathBuf::from).or_else(|| {
		config
			.as_ref()
			.and_then(|c| c.tls.as_ref().and_then(|t| t.cert_path.as_ref().map(PathBuf::from)))
			.or_else(get_default_cert_path)
	})
		.unwrap_or_else(|| {
			eprintln!("TLS cert path not provided. Use --tls-cert or ensure config file exists at ~/.ldk-server/config.toml");
			std::process::exit(1);
		});

	let server_cert_pem = std::fs::read(&tls_cert_path).unwrap_or_else(|e| {
		eprintln!("Failed to read server certificate file '{}': {}", tls_cert_path.display(), e);
		std::process::exit(1);
	});

	let client = LdkServerClient::new(base_url, api_key, &server_cert_pem).unwrap_or_else(|e| {
		eprintln!("Failed to create client: {e}");
		std::process::exit(1);
	});

	match cli.command {
		Commands::GetNodeInfo => {
			handle_response_result::<_, GetNodeInfoResponse>(
				client.get_node_info(GetNodeInfoRequest {}).await,
			);
		},
		Commands::GetBalances => {
			handle_response_result::<_, GetBalancesResponse>(
				client.get_balances(GetBalancesRequest {}).await,
			);
		},
		Commands::OnchainReceive => {
			handle_response_result::<_, OnchainReceiveResponse>(
				client.onchain_receive(OnchainReceiveRequest {}).await,
			);
		},
		Commands::OnchainSend { address, amount_sats, send_all, fee_rate_sat_per_vb } => {
			handle_response_result::<_, OnchainSendResponse>(
				client
					.onchain_send(OnchainSendRequest {
						address,
						amount_sats,
						send_all,
						fee_rate_sat_per_vb,
					})
					.await,
			);
		},
		Commands::Bolt11Receive { description, description_hash, expiry_secs, amount_msat } => {
			let invoice_description = match (description, description_hash) {
				(Some(desc), None) => Some(Bolt11InvoiceDescription {
					kind: Some(bolt11_invoice_description::Kind::Direct(desc)),
				}),
				(None, Some(hash)) => Some(Bolt11InvoiceDescription {
					kind: Some(bolt11_invoice_description::Kind::Hash(hash)),
				}),
				(Some(_), Some(_)) => {
					handle_error(LdkServerError::new(
						InternalError,
						"Only one of description or description_hash can be set.".to_string(),
					));
				},
				(None, None) => None,
			};

			let expiry_secs = expiry_secs.unwrap_or(DEFAULT_EXPIRY_SECS);
			let request =
				Bolt11ReceiveRequest { description: invoice_description, expiry_secs, amount_msat };

			handle_response_result::<_, Bolt11ReceiveResponse>(
				client.bolt11_receive(request).await,
			);
		},
		Commands::Bolt11Send {
			invoice,
			amount_msat,
			max_total_routing_fee_msat,
			max_total_cltv_expiry_delta,
			max_path_count,
			max_channel_saturation_power_of_half,
		} => {
			let route_parameters = RouteParametersConfig {
				max_total_routing_fee_msat,
				max_total_cltv_expiry_delta: max_total_cltv_expiry_delta
					.unwrap_or(DEFAULT_MAX_TOTAL_CLTV_EXPIRY_DELTA),
				max_path_count: max_path_count.unwrap_or(DEFAULT_MAX_PATH_COUNT),
				max_channel_saturation_power_of_half: max_channel_saturation_power_of_half
					.unwrap_or(DEFAULT_MAX_CHANNEL_SATURATION_POWER_OF_HALF),
			};
			handle_response_result::<_, Bolt11SendResponse>(
				client
					.bolt11_send(Bolt11SendRequest {
						invoice,
						amount_msat,
						route_parameters: Some(route_parameters),
					})
					.await,
			);
		},
		Commands::Bolt12Receive { description, amount_msat, expiry_secs, quantity } => {
			handle_response_result::<_, Bolt12ReceiveResponse>(
				client
					.bolt12_receive(Bolt12ReceiveRequest {
						description,
						amount_msat,
						expiry_secs,
						quantity,
					})
					.await,
			);
		},
		Commands::Bolt12Send {
			offer,
			amount_msat,
			quantity,
			payer_note,
			max_total_routing_fee_msat,
			max_total_cltv_expiry_delta,
			max_path_count,
			max_channel_saturation_power_of_half,
		} => {
			let route_parameters = RouteParametersConfig {
				max_total_routing_fee_msat,
				max_total_cltv_expiry_delta: max_total_cltv_expiry_delta
					.unwrap_or(DEFAULT_MAX_TOTAL_CLTV_EXPIRY_DELTA),
				max_path_count: max_path_count.unwrap_or(DEFAULT_MAX_PATH_COUNT),
				max_channel_saturation_power_of_half: max_channel_saturation_power_of_half
					.unwrap_or(DEFAULT_MAX_CHANNEL_SATURATION_POWER_OF_HALF),
			};

			handle_response_result::<_, Bolt12SendResponse>(
				client
					.bolt12_send(Bolt12SendRequest {
						offer,
						amount_msat,
						quantity,
						payer_note,
						route_parameters: Some(route_parameters),
					})
					.await,
			);
		},
		Commands::CloseChannel { user_channel_id, counterparty_node_id } => {
			handle_response_result::<_, CloseChannelResponse>(
				client
					.close_channel(CloseChannelRequest { user_channel_id, counterparty_node_id })
					.await,
			);
		},
		Commands::ForceCloseChannel {
			user_channel_id,
			counterparty_node_id,
			force_close_reason,
		} => {
			handle_response_result::<_, ForceCloseChannelResponse>(
				client
					.force_close_channel(ForceCloseChannelRequest {
						user_channel_id,
						counterparty_node_id,
						force_close_reason,
					})
					.await,
			);
		},
		Commands::OpenChannel {
			node_pubkey,
			address,
			channel_amount_sats,
			push_to_counterparty_msat,
			announce_channel,
			forwarding_fee_proportional_millionths,
			forwarding_fee_base_msat,
			cltv_expiry_delta,
		} => {
			let channel_config = build_open_channel_config(
				forwarding_fee_proportional_millionths,
				forwarding_fee_base_msat,
				cltv_expiry_delta,
			);

			handle_response_result::<_, OpenChannelResponse>(
				client
					.open_channel(OpenChannelRequest {
						node_pubkey,
						address,
						channel_amount_sats,
						push_to_counterparty_msat,
						channel_config,
						announce_channel,
					})
					.await,
			);
		},
		Commands::SpliceIn { user_channel_id, counterparty_node_id, splice_amount_sats } => {
			handle_response_result::<_, SpliceInResponse>(
				client
					.splice_in(SpliceInRequest {
						user_channel_id,
						counterparty_node_id,
						splice_amount_sats,
					})
					.await,
			);
		},
		Commands::SpliceOut {
			user_channel_id,
			counterparty_node_id,
			address,
			splice_amount_sats,
		} => {
			handle_response_result::<_, SpliceOutResponse>(
				client
					.splice_out(SpliceOutRequest {
						user_channel_id,
						counterparty_node_id,
						address,
						splice_amount_sats,
					})
					.await,
			);
		},
		Commands::ListChannels => {
			handle_response_result::<_, ListChannelsResponse>(
				client.list_channels(ListChannelsRequest {}).await,
			);
		},
		Commands::ListPayments { number_of_payments, page_token } => {
			let page_token = page_token
				.map(|token_str| parse_page_token(&token_str).unwrap_or_else(|e| handle_error(e)));

			handle_response_result::<_, CliListPaymentsResponse>(
				fetch_paginated(
					number_of_payments,
					page_token,
					|pt| client.list_payments(ListPaymentsRequest { page_token: pt }),
					|r| (r.payments, r.next_page_token),
				)
				.await,
			);
		},
		Commands::GetPaymentDetails { payment_id } => {
			handle_response_result::<_, GetPaymentDetailsResponse>(
				client.get_payment_details(GetPaymentDetailsRequest { payment_id }).await,
			);
		},
		Commands::ListForwardedPayments { number_of_payments, page_token } => {
			let page_token = page_token
				.map(|token_str| parse_page_token(&token_str).unwrap_or_else(|e| handle_error(e)));

			handle_response_result::<_, CliListForwardedPaymentsResponse>(
				fetch_paginated(
					number_of_payments,
					page_token,
					|pt| {
						client.list_forwarded_payments(ListForwardedPaymentsRequest {
							page_token: pt,
						})
					},
					|r| (r.forwarded_payments, r.next_page_token),
				)
				.await,
			);
		},
		Commands::UpdateChannelConfig {
			user_channel_id,
			counterparty_node_id,
			forwarding_fee_proportional_millionths,
			forwarding_fee_base_msat,
			cltv_expiry_delta,
		} => {
			let channel_config = ChannelConfig {
				forwarding_fee_proportional_millionths,
				forwarding_fee_base_msat,
				cltv_expiry_delta,
				force_close_avoidance_max_fee_satoshis: None,
				accept_underpaying_htlcs: None,
				max_dust_htlc_exposure: None,
			};

			handle_response_result::<_, UpdateChannelConfigResponse>(
				client
					.update_channel_config(UpdateChannelConfigRequest {
						user_channel_id,
						counterparty_node_id,
						channel_config: Some(channel_config),
					})
					.await,
			);
		},
		Commands::ConnectPeer { node_pubkey, address, persist } => {
			handle_response_result::<_, ConnectPeerResponse>(
				client.connect_peer(ConnectPeerRequest { node_pubkey, address, persist }).await,
			);
		},
		Commands::Completions { .. } => unreachable!("Handled above"),
	}
}

fn build_open_channel_config(
	forwarding_fee_proportional_millionths: Option<u32>, forwarding_fee_base_msat: Option<u32>,
	cltv_expiry_delta: Option<u32>,
) -> Option<ChannelConfig> {
	// Only create a config if at least one field is set
	if forwarding_fee_proportional_millionths.is_none()
		&& forwarding_fee_base_msat.is_none()
		&& cltv_expiry_delta.is_none()
	{
		return None;
	}

	Some(ChannelConfig {
		forwarding_fee_proportional_millionths,
		forwarding_fee_base_msat,
		cltv_expiry_delta,
		force_close_avoidance_max_fee_satoshis: None,
		accept_underpaying_htlcs: None,
		max_dust_htlc_exposure: None,
	})
}

async fn fetch_paginated<T, R, Fut>(
	target_count: Option<u64>, initial_page_token: Option<PageToken>,
	fetch_page: impl Fn(Option<PageToken>) -> Fut,
	extract: impl Fn(R) -> (Vec<T>, Option<PageToken>),
) -> Result<CliPaginatedResponse<T>, LdkServerError>
where
	Fut: std::future::Future<Output = Result<R, LdkServerError>>,
{
	match target_count {
		Some(count) => {
			let mut items = Vec::with_capacity(count as usize);
			let mut page_token = initial_page_token;
			let mut next_page_token;

			loop {
				let response = fetch_page(page_token).await?;
				let (new_items, new_next_page_token) = extract(response);
				items.extend(new_items);
				next_page_token = new_next_page_token;

				if items.len() >= count as usize || next_page_token.is_none() {
					break;
				}
				page_token = next_page_token;
			}

			Ok(CliPaginatedResponse::new(items, next_page_token))
		},
		None => {
			let response = fetch_page(initial_page_token).await?;
			let (items, next_page_token) = extract(response);
			Ok(CliPaginatedResponse::new(items, next_page_token))
		},
	}
}

fn handle_response_result<Rs, Js>(response: Result<Rs, LdkServerError>)
where
	Rs: Into<Js>,
	Js: Serialize + std::fmt::Debug,
{
	match response {
		Ok(response) => {
			let json_response: Js = response.into();
			match serde_json::to_string_pretty(&json_response) {
				Ok(json) => println!("{json}"),
				Err(e) => {
					eprintln!("Error serializing response ({json_response:?}) to JSON: {e}");
					std::process::exit(1);
				},
			}
		},
		Err(e) => {
			handle_error(e);
		},
	}
}

fn parse_page_token(token_str: &str) -> Result<PageToken, LdkServerError> {
	let parts: Vec<&str> = token_str.split(':').collect();
	if parts.len() != 2 {
		return Err(LdkServerError::new(
			InternalError,
			"Page token must be in format 'token:index'".to_string(),
		));
	}
	let index = parts[1]
		.parse::<i64>()
		.map_err(|_| LdkServerError::new(InternalError, "Invalid page token index".to_string()))?;
	Ok(PageToken { token: parts[0].to_string(), index })
}

fn handle_error(e: LdkServerError) -> ! {
	let error_type = match e.error_code {
		InvalidRequestError => "Invalid Request",
		AuthError => "Authentication Error",
		LightningError => "Lightning Error",
		InternalServerError => "Internal Server Error",
		InternalError => "Internal Error",
	};
	eprintln!("Error ({}): {}", error_type, e.message);
	std::process::exit(1); // Exit with status code 1 on error.
}
