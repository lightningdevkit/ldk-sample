use crate::{InboundPaymentInfoStorage, NetworkGraph, OutboundPaymentInfoStorage};
use bitcoin::Network;
use chrono::Utc;
use lightning::routing::scoring::{ProbabilisticScorer, ProbabilisticScoringDecayParameters};
use lightning::util::hash_tables::new_hash_map;
use lightning::util::logger::{Level, Logger, Record};
use lightning::util::ser::{Readable, ReadableArgs};
use std::fs;
use std::fs::File;
use std::io::{BufReader, Write};
use std::path::Path;
use std::sync::Arc;

pub(crate) const INBOUND_PAYMENTS_FNAME: &str = "inbound_payments";
pub(crate) const OUTBOUND_PAYMENTS_FNAME: &str = "outbound_payments";

pub(crate) struct FilesystemLogger {
	data_dir: String,
}
impl FilesystemLogger {
	pub(crate) fn new(data_dir: String) -> Self {
		let logs_path = format!("{}/logs", data_dir);
		fs::create_dir_all(logs_path.clone()).unwrap();
		Self { data_dir: logs_path }
	}
}
impl Logger for FilesystemLogger {
	fn log(&self, record: Record) {
		if record.level == Level::Gossip {
			// Gossip-level logs are incredibly verbose, and thus we skip them by default.
			return;
		}
		let raw_log = record.args.to_string();
		let log = format!(
			"{} {:<5} [{}:{}] {}\n",
			// Note that a "real" lightning node almost certainly does *not* want subsecond
			// precision for message-receipt information as it makes log entries a target for
			// deanonymization attacks. For testing, however, its quite useful.
			Utc::now().format("%Y-%m-%d %H:%M:%S%.3f"),
			record.level.to_string(),
			record.module_path,
			record.line,
			raw_log
		);
		let logs_file_path = format!("{}/logs.txt", self.data_dir.clone());
		fs::OpenOptions::new()
			.create(true)
			.append(true)
			.open(logs_file_path)
			.unwrap()
			.write_all(log.as_bytes())
			.unwrap();
	}
}

pub(crate) fn read_network(
	path: &Path, network: Network, logger: Arc<FilesystemLogger>,
) -> NetworkGraph {
	if let Ok(file) = File::open(path) {
		if let Ok(graph) = NetworkGraph::read(&mut BufReader::new(file), logger.clone()) {
			return graph;
		}
	}
	NetworkGraph::new(network, logger)
}

pub(crate) fn read_inbound_payment_info(path: &Path) -> InboundPaymentInfoStorage {
	if let Ok(file) = File::open(path) {
		if let Ok(info) = InboundPaymentInfoStorage::read(&mut BufReader::new(file)) {
			return info;
		}
	}
	InboundPaymentInfoStorage { payments: new_hash_map() }
}

pub(crate) fn read_outbound_payment_info(path: &Path) -> OutboundPaymentInfoStorage {
	if let Ok(file) = File::open(path) {
		if let Ok(info) = OutboundPaymentInfoStorage::read(&mut BufReader::new(file)) {
			return info;
		}
	}
	OutboundPaymentInfoStorage { payments: new_hash_map() }
}

pub(crate) fn read_scorer(
	path: &Path, graph: Arc<NetworkGraph>, logger: Arc<FilesystemLogger>,
) -> ProbabilisticScorer<Arc<NetworkGraph>, Arc<FilesystemLogger>> {
	let params = ProbabilisticScoringDecayParameters::default();
	if let Ok(file) = File::open(path) {
		let args = (params.clone(), Arc::clone(&graph), Arc::clone(&logger));
		if let Ok(scorer) = ProbabilisticScorer::read(&mut BufReader::new(file), args) {
			return scorer;
		}
	}
	ProbabilisticScorer::new(params, graph, logger)
}
