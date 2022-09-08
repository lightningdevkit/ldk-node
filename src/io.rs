use crate::error::LdkLiteError as Error;

use crate::{FilesystemLogger, LdkLiteConfig, NetworkGraph, PeerInfo, Scorer};

use lightning::routing::scoring::{ProbabilisticScorer, ProbabilisticScoringParameters};
use lightning::util::ser::ReadableArgs;

use bitcoin::secp256k1::PublicKey;

use rand::{thread_rng, RngCore};

use std::collections::HashMap;
use std::convert::TryFrom;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::sync::Arc;

pub(crate) fn read_or_generate_seed_file(seed_path: String) -> Result<[u8; 64], Error> {
	let keys_seed = if let Ok(seed) = fs::read(seed_path.clone()) {
		assert_eq!(seed.len(), 64);
		let mut key = [0; 64];
		key.copy_from_slice(&seed);
		key
	} else {
		let mut key = [0; 64];
		thread_rng().fill_bytes(&mut key);

		let mut f = fs::File::create(seed_path.clone()).map_err(|e| Error::StdIo(e))?;
		f.write_all(&key).expect("Failed to write node keys seed to disk");
		f.sync_all().expect("Failed to sync node keys seed to disk");
		key
	};

	Ok(keys_seed)
}

pub(crate) fn read_network_graph(
	config: Arc<LdkLiteConfig>, logger: Arc<FilesystemLogger>,
) -> Result<NetworkGraph, Error> {
	let ldk_data_dir = format!("{}/ldk", &config.storage_dir_path.clone());
	let network_graph_path = format!("{}/network_graph", ldk_data_dir.clone());

	if let Ok(file) = fs::File::open(network_graph_path) {
		if let Ok(graph) = NetworkGraph::read(&mut BufReader::new(file), Arc::clone(&logger)) {
			return Ok(graph);
		}
	}

	let genesis_hash =
		bitcoin::blockdata::constants::genesis_block(config.network).header.block_hash();
	Ok(NetworkGraph::new(genesis_hash, logger))
}

pub(crate) fn read_scorer(
	config: Arc<LdkLiteConfig>, network_graph: Arc<NetworkGraph>, logger: Arc<FilesystemLogger>,
) -> Scorer {
	let ldk_data_dir = format!("{}/ldk", &config.storage_dir_path.clone());
	let scorer_path = format!("{}/scorer", ldk_data_dir.clone());

	let params = ProbabilisticScoringParameters::default();
	if let Ok(file) = fs::File::open(scorer_path) {
		let args = (params.clone(), Arc::clone(&network_graph), Arc::clone(&logger));
		if let Ok(scorer) = ProbabilisticScorer::read(&mut BufReader::new(file), args) {
			return scorer;
		}
	}
	ProbabilisticScorer::new(params, network_graph, logger)
}
