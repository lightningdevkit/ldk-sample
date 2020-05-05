use crate::rpc_client::*;

use bitcoin;
use serde_json;
use tokio;

use lightning::chain::chaininterface;
use lightning::chain::chaininterface::{BlockNotifierArc, ChainError};
use lightning::util::logger::Logger;

use lightning_block_sync::{AChainListener, BlockSource, dns_headers, http_clients, MicroSPVClient};

use bitcoin::blockdata::block::Block;
use bitcoin::blockdata::transaction::Transaction;
use bitcoin::consensus::encode;
use bitcoin::network::constants::Network;
use bitcoin::hash_types::{BlockHash, Txid};

use futures_util::future;

use tokio::sync::mpsc;

use std::collections::HashMap;
use std::cmp;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::vec::Vec;
use std::time::Duration;

pub struct FeeEstimator {
	background_est: AtomicUsize,
	normal_est: AtomicUsize,
	high_prio_est: AtomicUsize,
}
impl FeeEstimator {
	pub fn new() -> Self {
		FeeEstimator {
			background_est: AtomicUsize::new(0),
			normal_est: AtomicUsize::new(0),
			high_prio_est: AtomicUsize::new(0),
		}
	}
	pub async fn update_values(&self, rpc_client: &RPCClient) {
		if let Ok(v) = rpc_client.make_rpc_call("estimatesmartfee", &vec!["6", "\"CONSERVATIVE\""], false).await {
			if let Some(serde_json::Value::Number(hp_btc_per_kb)) = v.get("feerate") {
				self.high_prio_est.store(
					// Because we often don't have have good fee estimates on testnet, and we
					// don't want to prevent opening channels with peers that have even worse
					// fee estimates on testnet (eg LND), add an absurd fudge factor here:
					10 * ((hp_btc_per_kb.as_f64().unwrap() * 100_000_000.0 / 250.0) as usize + 3),
					Ordering::Release);
			}
		}
		if let Ok(v) = rpc_client.make_rpc_call("estimatesmartfee", &vec!["18", "\"ECONOMICAL\""], false).await {
			if let Some(serde_json::Value::Number(np_btc_per_kb)) = v.get("feerate") {
				self.normal_est.store((np_btc_per_kb.as_f64().unwrap() * 100_000_000.0 / 250.0) as usize + 3, Ordering::Release);
			}
		}
		if let Ok(v) = rpc_client.make_rpc_call("estimatesmartfee", &vec!["144", "\"ECONOMICAL\""], false).await {
			if let Some(serde_json::Value::Number(bp_btc_per_kb)) = v.get("feerate") {
				self.background_est.store((bp_btc_per_kb.as_f64().unwrap() * 100_000_000.0 / 250.0) as usize + 3, Ordering::Release);
			}
		}
	}
}
impl chaininterface::FeeEstimator for FeeEstimator {
	fn get_est_sat_per_1000_weight(&self, conf_target: chaininterface::ConfirmationTarget) -> u64 {
		cmp::max(match conf_target {
			chaininterface::ConfirmationTarget::Background => self.background_est.load(Ordering::Acquire) as u64,
			chaininterface::ConfirmationTarget::Normal => self.normal_est.load(Ordering::Acquire) as u64,
			chaininterface::ConfirmationTarget::HighPriority => self.high_prio_est.load(Ordering::Acquire) as u64,
		}, 253)
	}
}

pub struct ChainInterface {
	util: chaininterface::ChainWatchInterfaceUtil,
	txn_to_broadcast: Mutex<HashMap<Txid, bitcoin::blockdata::transaction::Transaction>>,
	rpc_client: Arc<RPCClient>,
}
impl ChainInterface {
	pub fn new(rpc_client: Arc<RPCClient>, network: Network, logger: Arc<dyn Logger>) -> Self {
		ChainInterface {
			util: chaininterface::ChainWatchInterfaceUtil::new(network, logger),
			txn_to_broadcast: Mutex::new(HashMap::new()),
			rpc_client,
		}
	}

	async fn rebroadcast_txn(&self) {
		let mut send_futures = Vec::new();
		{
			let txn = self.txn_to_broadcast.lock().unwrap();
			for (_, tx) in txn.iter() {
				let tx_ser = "\"".to_string() + &encode::serialize_hex(tx) + "\"";
				let rpc_client = Arc::clone(&self.rpc_client);
				send_futures.push(tokio::spawn(async move {
					rpc_client.make_rpc_call("sendrawtransaction", &[&tx_ser], true).await
				}));
			}
		}
		future::join_all(send_futures).await;
	}
}
impl chaininterface::ChainWatchInterface for ChainInterface {
	fn install_watch_tx(&self, txid: &Txid, script: &bitcoin::blockdata::script::Script) {
		self.util.install_watch_tx(txid, script);
	}

	fn install_watch_outpoint(&self, outpoint: (Txid, u32), script_pubkey: &bitcoin::blockdata::script::Script) {
		self.util.install_watch_outpoint(outpoint, script_pubkey);
	}

	fn watch_all_txn(&self) {
		self.util.watch_all_txn();
	}

	fn get_chain_utxo(&self, genesis_hash: BlockHash, unspent_tx_output_identifier: u64) -> Result<(bitcoin::blockdata::script::Script, u64), ChainError> {
		self.util.get_chain_utxo(genesis_hash, unspent_tx_output_identifier)
	}

	fn filter_block<'a>(&self, block: &'a Block) -> (Vec<&'a Transaction>, Vec<u32>) {
		self.util.filter_block(block)
	}

	fn reentered(&self) -> usize {
		self.util.reentered()
	}
}
impl chaininterface::BroadcasterInterface for ChainInterface {
	fn broadcast_transaction (&self, tx: &bitcoin::blockdata::transaction::Transaction) {
		self.txn_to_broadcast.lock().unwrap().insert(tx.txid(), tx.clone());
		let tx_ser = "\"".to_string() + &encode::serialize_hex(tx) + "\"";
		let rpc_client = Arc::clone(&self.rpc_client);
		// TODO: New tokio has largely endeavored to do away with the concept of spawn-and-forget,
		// which was the primary method of executing in 0.1. Sadly, in a non-async context, there
		// is no way to block on a future completion. In some future version, this may break, see
		// https://github.com/tokio-rs/tokio/issues/1830.
		tokio::spawn(async move {
			if let Ok(txid) = rpc_client.make_rpc_call("sendrawtransaction", &[&tx_ser], true).await {
				println!("Broadcasted transaction {}", txid);
			}
		});
	}
}

pub async fn rebroadcast_and_update_fees(fee_estimator: Arc<FeeEstimator>, chain_interface: Arc<ChainInterface>, rpc_client: Arc<RPCClient>) {
	tokio::spawn(async move {
		let mut interval = tokio::time::interval(Duration::from_secs(5));
		interval.tick().await;
		let mut cur_block = rpc_client.make_rpc_call("getblockchaininfo", &[], false).await
			.unwrap()["bestblockhash"].as_str().unwrap().to_string();
		fee_estimator.update_values(&rpc_client).await;
		loop {
			interval.tick().await;
			if let Ok(chaininfo) = rpc_client.make_rpc_call("getblockchaininfo", &[], false).await {
				let new_block = chaininfo["bestblockhash"].as_str().unwrap().to_string();
				let old_block = cur_block.clone();

				if new_block == old_block { continue; }
				cur_block = new_block.clone();

				fee_estimator.update_values(&rpc_client).await;
				chain_interface.rebroadcast_txn().await;
			}
		}
	}).await.unwrap()
}

pub async fn init_sync_chain_monitor<CL : AChainListener + Sized>(new_block: BlockHash, old_block: BlockHash, block_source: (&str, &str), chain_notifier: CL) {
	let mut rpc_client = lightning_block_sync::http_clients::RPCClient::new(block_source.0, "http://".to_string() + block_source.1 + "/").unwrap();
	lightning_block_sync::init_sync_chain_monitor(new_block, old_block, &mut rpc_client, chain_notifier).await
}

pub fn spawn_chain_monitor(chain_tip_hash: BlockHash, block_source: (&str, &str), chain_notifier: BlockNotifierArc, mut event_notify: mpsc::Sender<()>, mainnet: bool) -> impl Future<Output = ()> {
	let mut rpc_client = lightning_block_sync::http_clients::RPCClient::new(block_source.0, "http://".to_string() + block_source.1 + "/").unwrap();
	async move {
		tokio::spawn(async move {
			let chain_tip = rpc_client.get_header(&chain_tip_hash, None).await.unwrap();
			let mut block_sources = vec![&mut rpc_client as &mut dyn BlockSource];
			let mut backup_sources = Vec::new();

			let headers = dns_headers::DNSHeadersClient::new("bitcoinheaders.net".to_string());
			let mut headers_source = dns_headers::CachingHeadersClient::new(headers, mainnet);
			let mut backup_source = http_clients::RESTClient::new("http://cloudflare.deanonymizingseed.com/rest/".to_string()).unwrap();
			if mainnet {
				block_sources.push(&mut headers_source as &mut dyn BlockSource);
				backup_sources.push(&mut backup_source as &mut dyn BlockSource);
			}
			let mut client = MicroSPVClient::init(chain_tip, block_sources, backup_sources, &chain_notifier, mainnet);
			let mut interval = tokio::time::interval(Duration::from_secs(1));
			loop {
				interval.tick().await;
				if client.poll_best_tip().await {
					let _ = event_notify.try_send(());
				}
			}
		}).await.unwrap()
	}
}
