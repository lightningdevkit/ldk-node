use clap::{Parser, Subcommand};
use client::client::LdkNodeServerClient;
use client::error::LdkNodeServerError;
use client::protos::{
	Bolt11ReceiveRequest, Bolt11SendRequest, Bolt12ReceiveRequest, OnchainReceiveRequest,
	OnchainSendRequest,
};

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
	OnchainReceive,
	OnchainSend {
		#[arg(short, long)]
		address: String,
		#[arg(long)]
		amount_sats: Option<u64>,
		#[arg(long)]
		send_all: Option<bool>,
	},
	Bolt11Receive {
		#[arg(short, long)]
		description: String,
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
	},
}

#[tokio::main]
async fn main() {
	let cli = Cli::parse();
	let client = LdkNodeServerClient::new(cli.base_url);

	match cli.command {
		Commands::OnchainReceive => {
			handle_response(client.onchain_receive(OnchainReceiveRequest {}).await);
		},
		Commands::OnchainSend { address, amount_sats, send_all } => {
			handle_response(
				client.onchain_send(OnchainSendRequest { address, amount_sats, send_all }).await,
			);
		},
		Commands::Bolt11Receive { description, expiry_secs, amount_msat } => {
			handle_response(
				client
					.bolt11_receive(Bolt11ReceiveRequest { description, expiry_secs, amount_msat })
					.await,
			);
		},
		Commands::Bolt11Send { invoice, amount_msat } => {
			handle_response(client.bolt11_send(Bolt11SendRequest { invoice, amount_msat }).await);
		},
		Commands::Bolt12Receive { description, amount_msat } => {
			handle_response(
				client.bolt12_receive(Bolt12ReceiveRequest { description, amount_msat }).await,
			);
		},
	}
}

fn handle_response<Rs: ::prost::Message>(response: Result<Rs, LdkNodeServerError>) {
	match response {
		Ok(response) => {
			println!("{:?}", response);
		},
		Err(e) => {
			eprintln!("Error executing command: {:?}", e);
		},
	};
}
