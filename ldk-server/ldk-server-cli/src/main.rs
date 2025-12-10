use clap::{Parser, Subcommand};
use ldk_server_client::client::LdkServerClient;
use ldk_server_client::error::LdkServerError;
use ldk_server_client::error::LdkServerErrorCode::{
	AuthError, InternalError, InternalServerError, InvalidRequestError, LightningError,
};
use ldk_server_client::ldk_server_protos::api::{
	Bolt11ReceiveRequest, Bolt11SendRequest, Bolt12ReceiveRequest, Bolt12SendRequest,
	CloseChannelRequest, ForceCloseChannelRequest, GetBalancesRequest, GetNodeInfoRequest,
	ListChannelsRequest, ListPaymentsRequest, OnchainReceiveRequest, OnchainSendRequest,
	OpenChannelRequest,
};
use ldk_server_client::ldk_server_protos::types::{
	bolt11_invoice_description, channel_config, Bolt11InvoiceDescription, ChannelConfig, PageToken,
	Payment,
};
use std::fmt::Debug;

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
	ListChannels,
	ListPayments {
		#[arg(short, long)]
		#[arg(
			help = "Minimum number of payments to return. If not provided, only the first page of the paginated list is returned."
		)]
		number_of_payments: Option<u64>,
	},
}

#[tokio::main]
async fn main() {
	let cli = Cli::parse();
	let client = LdkServerClient::new(cli.base_url);

	match cli.command {
		Commands::GetNodeInfo => {
			handle_response_result(client.get_node_info(GetNodeInfoRequest {}).await);
		},
		Commands::GetBalances => {
			handle_response_result(client.get_balances(GetBalancesRequest {}).await);
		},
		Commands::OnchainReceive => {
			handle_response_result(client.onchain_receive(OnchainReceiveRequest {}).await);
		},
		Commands::OnchainSend { address, amount_sats, send_all, fee_rate_sat_per_vb } => {
			handle_response_result(
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

			handle_response_result(client.bolt11_receive(request).await);
		},
		Commands::Bolt11Send { invoice, amount_msat } => {
			handle_response_result(
				client.bolt11_send(Bolt11SendRequest { invoice, amount_msat }).await,
			);
		},
		Commands::Bolt12Receive { description, amount_msat, expiry_secs, quantity } => {
			handle_response_result(
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
		Commands::Bolt12Send { offer, amount_msat, quantity, payer_note } => {
			handle_response_result(
				client
					.bolt12_send(Bolt12SendRequest { offer, amount_msat, quantity, payer_note })
					.await,
			);
		},
		Commands::CloseChannel { user_channel_id, counterparty_node_id } => {
			handle_response_result(
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
			handle_response_result(
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

			handle_response_result(
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
		Commands::ListChannels => {
			handle_response_result(client.list_channels(ListChannelsRequest {}).await);
		},
		Commands::ListPayments { number_of_payments } => {
			handle_response_result(list_n_payments(client, number_of_payments).await);
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

fn handle_response_result<Rs: Debug>(response: Result<Rs, LdkServerError>) {
	match response {
		Ok(response) => {
			println!("Success: {:?}", response);
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
