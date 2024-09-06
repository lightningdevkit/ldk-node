use clap::{Parser, Subcommand};
use client::client::LdkNodeServerClient;
use client::protos::OnchainReceiveRequest;

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
}

#[tokio::main]
async fn main() {
	let cli = Cli::parse();
	let client = LdkNodeServerClient::new(cli.base_url);

	match cli.command {
		Commands::OnchainReceive => {
			match client.onchain_receive(OnchainReceiveRequest {}).await {
				Ok(response) => {
					println!("New Bitcoin Address: {:?}", response);
				},
				Err(e) => {
					eprintln!("Error in OnchainReceive: {:?}", e);
				},
			};
		},
	}
}
