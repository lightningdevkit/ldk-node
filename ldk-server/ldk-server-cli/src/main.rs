use clap::{Parser, Subcommand};
use ldk_server_client::client::LdkServerClient;
use ldk_server_client::error::LdkServerError;
use ldk_server_client::error::LdkServerErrorCode::{
	AuthError, InternalError, InternalServerError, InvalidRequestError, LightningError,
};
use ldk_server_client::ldk_server_protos::api::{
	Bolt11ReceiveRequest, Bolt11ReceiveResponse, Bolt11SendRequest, Bolt11SendResponse,
	Bolt12ReceiveRequest, Bolt12ReceiveResponse, Bolt12SendRequest, Bolt12SendResponse,
	CloseChannelRequest, CloseChannelResponse, ForceCloseChannelRequest, ForceCloseChannelResponse,
	GetBalancesRequest, GetBalancesResponse, GetNodeInfoRequest, GetNodeInfoResponse,
	ListChannelsRequest, ListChannelsResponse, ListPaymentsRequest, ListPaymentsResponse,
	OnchainReceiveRequest, OnchainReceiveResponse, OnchainSendRequest, OnchainSendResponse,
	OpenChannelRequest, OpenChannelResponse, SpliceInRequest, SpliceInResponse, SpliceOutRequest,
	SpliceOutResponse, UpdateChannelConfigRequest, UpdateChannelConfigResponse,
};
use ldk_server_client::ldk_server_protos::types::{
	bolt11_invoice_description, Bolt11InvoiceDescription, ChannelConfig, PageToken, Payment,
	RouteParametersConfig,
};
use serde::Serialize;

// Having these default values as constants in the Proto file and
// importing/reusing them here might be better, but Proto3 removed
// the ability to set default values.
const DEFAULT_MAX_TOTAL_CLTV_EXPIRY_DELTA: u32 = 1008;
const DEFAULT_MAX_PATH_COUNT: u32 = 10;
const DEFAULT_MAX_CHANNEL_SATURATION_POWER_OF_HALF: u32 = 2;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
	#[arg(short, long, default_value = "localhost:3000")]
	base_url: String,

	#[command(subcommand)]
	command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
	GetNodeInfo,
	GetBalances,
	OnchainReceive,
	OnchainSend {
		#[arg(short, long)]
		address: String,
		#[arg(long)]
		amount_sats: Option<u64>,
		#[arg(long)]
		send_all: Option<bool>,
		#[arg(long)]
		fee_rate_sat_per_vb: Option<u64>,
	},
	Bolt11Receive {
		#[arg(short, long)]
		description: Option<String>,
		#[arg(long)]
		description_hash: Option<String>,
		#[arg(short, long)]
		expiry_secs: u32,
		#[arg(long)]
		amount_msat: Option<u64>,
	},
	Bolt11Send {
		#[arg(short, long)]
		invoice: String,
		#[arg(long)]
		amount_msat: Option<u64>,
		#[arg(long)]
		max_total_routing_fee_msat: Option<u64>,
		#[arg(long)]
		max_total_cltv_expiry_delta: Option<u32>,
		#[arg(long)]
		max_path_count: Option<u32>,
		#[arg(long)]
		max_channel_saturation_power_of_half: Option<u32>,
	},
	Bolt12Receive {
		#[arg(short, long)]
		description: String,
		#[arg(long)]
		amount_msat: Option<u64>,
		#[arg(long)]
		expiry_secs: Option<u32>,
		#[arg(long)]
		quantity: Option<u64>,
	},
	Bolt12Send {
		#[arg(short, long)]
		offer: String,
		#[arg(long)]
		amount_msat: Option<u64>,
		#[arg(short, long)]
		quantity: Option<u64>,
		#[arg(short, long)]
		payer_note: Option<String>,
		#[arg(long)]
		max_total_routing_fee_msat: Option<u64>,
		#[arg(long)]
		max_total_cltv_expiry_delta: Option<u32>,
		#[arg(long)]
		max_path_count: Option<u32>,
		#[arg(long)]
		max_channel_saturation_power_of_half: Option<u32>,
	},
	CloseChannel {
		#[arg(short, long)]
		user_channel_id: String,
		#[arg(short, long)]
		counterparty_node_id: String,
	},
	ForceCloseChannel {
		#[arg(short, long)]
		user_channel_id: String,
		#[arg(short, long)]
		counterparty_node_id: String,
		#[arg(long)]
		force_close_reason: Option<String>,
	},
	OpenChannel {
		#[arg(short, long)]
		node_pubkey: String,
		#[arg(short, long)]
		address: String,
		#[arg(long)]
		channel_amount_sats: u64,
		#[arg(long)]
		push_to_counterparty_msat: Option<u64>,
		#[arg(long)]
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
	SpliceIn {
		#[arg(short, long)]
		user_channel_id: String,
		#[arg(short, long)]
		counterparty_node_id: String,
		#[arg(long)]
		splice_amount_sats: u64,
	},
	SpliceOut {
		#[arg(short, long)]
		user_channel_id: String,
		#[arg(short, long)]
		counterparty_node_id: String,
		#[arg(short, long)]
		address: Option<String>,
		#[arg(long)]
		splice_amount_sats: u64,
	},
	ListChannels,
	ListPayments {
		#[arg(short, long)]
		#[arg(
			help = "Minimum number of payments to return. If not provided, only the first page of the paginated list is returned."
		)]
		number_of_payments: Option<u64>,
	},
	UpdateChannelConfig {
		#[arg(short, long)]
		user_channel_id: String,
		#[arg(short, long)]
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
}

#[tokio::main]
async fn main() {
	let cli = Cli::parse();
	let client = LdkServerClient::new(cli.base_url);

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
		Commands::ListPayments { number_of_payments } => {
			handle_response_result::<_, ListPaymentsResponse>(
				list_n_payments(client, number_of_payments)
					.await
					// todo: handle pagination properly
					.map(|payments| ListPaymentsResponse { payments, next_page_token: None }),
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

async fn list_n_payments(
	client: LdkServerClient, number_of_payments: Option<u64>,
) -> Result<Vec<Payment>, LdkServerError> {
	let mut payments = Vec::new();
	let mut page_token: Option<PageToken> = None;
	// If no count is specified, just list the first page.
	let target_count = number_of_payments.unwrap_or(0);

	loop {
		let response = client.list_payments(ListPaymentsRequest { page_token }).await?;

		payments.extend(response.payments);
		if payments.len() >= target_count as usize || response.next_page_token.is_none() {
			break;
		}
		page_token = response.next_page_token;
	}
	Ok(payments)
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
