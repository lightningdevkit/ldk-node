use clap::{Parser, Subcommand};
use client::client::LdkNodeServerClient;
use client::error::LdkNodeServerError;
use client::protos::{OnchainReceiveRequest, OnchainSendRequest};

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
