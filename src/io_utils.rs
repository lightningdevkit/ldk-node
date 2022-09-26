use crate::payment_store::{PaymentInfo, PAYMENT_INFO_PERSISTENCE_PREFIX};
use crate::{Config, FilesystemLogger, NetworkGraph, Scorer, WALLET_KEYS_SEED_LEN};

use lightning::routing::scoring::{ProbabilisticScorer, ProbabilisticScoringParameters};
use lightning::util::ser::{Readable, ReadableArgs};

use rand::{thread_rng, RngCore};

use std::fs;
use std::io::{BufReader, Write};
use std::path::Path;
use std::sync::Arc;

pub(crate) fn read_or_generate_seed_file(keys_seed_path: &str) -> [u8; WALLET_KEYS_SEED_LEN] {
	if Path::new(&keys_seed_path).exists() {
		let seed = fs::read(keys_seed_path).expect("Failed to read keys seed file");
		assert_eq!(
			seed.len(),
			WALLET_KEYS_SEED_LEN,
			"Failed to read keys seed file: unexpected length"
		);
		let mut key = [0; WALLET_KEYS_SEED_LEN];
		key.copy_from_slice(&seed);
		key
	} else {
		let mut key = [0; WALLET_KEYS_SEED_LEN];
		thread_rng().fill_bytes(&mut key);

		let mut f = fs::File::create(keys_seed_path).expect("Failed to create keys seed file");
		f.write_all(&key).expect("Failed to write node keys seed to disk");
		f.sync_all().expect("Failed to sync node keys seed to disk");
		key
	}
}

pub(crate) fn read_network_graph(config: &Config, logger: Arc<FilesystemLogger>) -> NetworkGraph {
	let ldk_data_dir = format!("{}/ldk", config.storage_dir_path);
	let network_graph_path = format!("{}/network_graph", ldk_data_dir);

	if let Ok(file) = fs::File::open(network_graph_path) {
		if let Ok(graph) = NetworkGraph::read(&mut BufReader::new(file), Arc::clone(&logger)) {
			return graph;
		}
	}

	NetworkGraph::new(config.network, logger)
}

pub(crate) fn read_scorer(
	config: &Config, network_graph: Arc<NetworkGraph>, logger: Arc<FilesystemLogger>,
) -> Scorer {
	let ldk_data_dir = format!("{}/ldk", config.storage_dir_path);
	let scorer_path = format!("{}/scorer", ldk_data_dir);

	let params = ProbabilisticScoringParameters::default();
	if let Ok(file) = fs::File::open(scorer_path) {
		let args = (params.clone(), Arc::clone(&network_graph), Arc::clone(&logger));
		if let Ok(scorer) = ProbabilisticScorer::read(&mut BufReader::new(file), args) {
			return scorer;
		}
	}
	ProbabilisticScorer::new(params, network_graph, logger)
}

pub(crate) fn read_payment_info(config: &Config) -> Vec<PaymentInfo> {
	let ldk_data_dir = format!("{}/ldk", config.storage_dir_path);
	let payment_store_path = format!("{}/{}", ldk_data_dir, PAYMENT_INFO_PERSISTENCE_PREFIX);
	let mut payments = Vec::new();

	if let Ok(res) = fs::read_dir(payment_store_path) {
		for entry in res {
			if let Ok(entry) = entry {
				if entry.path().is_file() {
					if let Ok(mut f) = fs::File::open(entry.path()) {
						if let Ok(payment_info) = PaymentInfo::read(&mut f) {
							payments.push(payment_info);
						}
					}
				}
			}
		}
	}

	payments
}
