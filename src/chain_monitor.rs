use crate::rpc_client::*;
use crate::utils::hex_to_vec;

use bitcoin;
use serde_json;
use tokio;

use bitcoin_hashes::sha256d::Hash as Sha256dHash;
use bitcoin_hashes::hex::ToHex;

use lightning::chain::{chaininterface, keysinterface};
use lightning::chain::chaininterface::{BlockNotifierArc, ChainError, ChainListener};
use lightning::util::logger::Logger;
use lightning::ln::channelmonitor::{ChannelMonitor, ManyChannelMonitor};
use lightning::ln::channelmanager::ChannelManager;

use bitcoin::blockdata::block::{Block, BlockHeader};
use bitcoin::blockdata::transaction::Transaction;
use bitcoin::consensus::encode;
use bitcoin::network::constants::Network;
use bitcoin::util::hash::BitcoinHash;

use futures_util::future;
use futures_util::future::FutureExt;

use tokio::sync::mpsc;

use std::collections::HashMap;
use std::cmp;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::vec::Vec;
use std::time::Duration;
use std::pin::Pin;

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
	txn_to_broadcast: Mutex<HashMap<Sha256dHash, bitcoin::blockdata::transaction::Transaction>>,
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
	fn install_watch_tx(&self, txid: &bitcoin_hashes::sha256d::Hash, script: &bitcoin::blockdata::script::Script) {
		self.util.install_watch_tx(txid, script);
	}

	fn install_watch_outpoint(&self, outpoint: (bitcoin_hashes::sha256d::Hash, u32), script_pubkey: &bitcoin::blockdata::script::Script) {
		self.util.install_watch_outpoint(outpoint, script_pubkey);
	}

	fn watch_all_txn(&self) {
		self.util.watch_all_txn();
	}

	fn get_chain_utxo(&self, genesis_hash: bitcoin_hashes::sha256d::Hash, unspent_tx_output_identifier: u64) -> Result<(bitcoin::blockdata::script::Script, u64), ChainError> {
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
			rpc_client.make_rpc_call("sendrawtransaction", &[&tx_ser], true).await
		});
	}
}

enum ForkStep {
	DisconnectBlock((bitcoin::blockdata::block::BlockHeader, u32)),
	ConnectBlock((String, u32)),
}
fn find_fork_step<'a>(steps_tx: &'a mut Vec<ForkStep>, current_header: GetHeaderResponse, target_header_opt: Option<(String, GetHeaderResponse)>, rpc_client: Arc<RPCClient>) -> Pin<Box<dyn Future<Output=()> + Send + 'a>> {
	async move {
		if target_header_opt.is_some() && target_header_opt.as_ref().unwrap().0 == current_header.previousblockhash {
			// Target is the parent of current, we're done!
		} else if current_header.height == 1 {
		} else if target_header_opt.is_none() || target_header_opt.as_ref().unwrap().1.height < current_header.height {
			steps_tx.push(ForkStep::ConnectBlock((current_header.previousblockhash.clone(), current_header.height - 1)));
			let new_cur_header = rpc_client.get_header(&current_header.previousblockhash).await.unwrap();
			find_fork_step(steps_tx, new_cur_header, target_header_opt, rpc_client).await;
		} else {
			let target_header = target_header_opt.unwrap().1;
			// Everything below needs to disconnect target, so go ahead and do that now
			steps_tx.push(ForkStep::DisconnectBlock((target_header.to_block_header(), target_header.height)));
			if target_header.previousblockhash == current_header.previousblockhash {
				// Found the fork, also connect current and finish!
				steps_tx.push(ForkStep::ConnectBlock((current_header.previousblockhash.clone(), current_header.height - 1)));
			} else if target_header.height > current_header.height {
				// Target is higher, walk it back and recurse
				let new_target_header = rpc_client.get_header(&target_header.previousblockhash).await.unwrap();
				find_fork_step(steps_tx, current_header, Some((target_header.previousblockhash, new_target_header)), rpc_client).await;
			} else {
				// Target and current are at the same height, but we're not at fork yet, walk
				// both back and recurse
				steps_tx.push(ForkStep::ConnectBlock((current_header.previousblockhash.clone(), current_header.height - 1)));
				let new_cur_header = rpc_client.get_header(&current_header.previousblockhash).await.unwrap();
				let new_target_header = rpc_client.get_header(&target_header.previousblockhash).await.unwrap();
				find_fork_step(steps_tx, new_cur_header, Some((target_header.previousblockhash, new_target_header)), rpc_client).await;
			}
		}
	}.boxed()
}
/// Walks backwards from current_hash and target_hash finding the fork and sending ForkStep events
/// into the steps_tx Sender. There is no ordering guarantee between different ForkStep types, but
/// DisconnectBlock and ConnectBlock events are each in reverse, height-descending order.
async fn find_fork(steps_tx: &mut Vec<ForkStep>, current_hash: String, target_hash: String, rpc_client: Arc<RPCClient>) {
	if current_hash == target_hash { return; }

	let current_header = rpc_client.get_header(&current_hash).await.unwrap();
	steps_tx.push(ForkStep::ConnectBlock((current_hash, current_header.height)));

	if current_header.previousblockhash == target_hash || current_header.height == 1 {
		// Fastpath one-new-block-connected or reached block 1
		return;
	} else {
		match rpc_client.get_header(&target_hash).await {
			Ok(target_header) =>
				find_fork_step(steps_tx, current_header, Some((target_hash, target_header)), rpc_client).await,
			Err(_) => panic!(),
		}
	}
}

pub trait AChainListener {
	fn a_block_connected(&mut self, block: &Block, height: u32);
	fn a_block_disconnected(&mut self, header: &BlockHeader, height: u32);
}

impl AChainListener for &BlockNotifierArc {
	fn a_block_connected(&mut self, block: &Block, height: u32) {
		self.block_connected(block, height);
	}
	fn a_block_disconnected(&mut self, header: &BlockHeader, height: u32) {
		self.block_disconnected(header, height);
	}
}

impl<M> AChainListener for &Arc<ChannelManager<keysinterface::InMemoryChannelKeys, Arc<M>>>
		where M: ManyChannelMonitor<keysinterface::InMemoryChannelKeys> {
	fn a_block_connected(&mut self, block: &Block, height: u32) {
		let mut txn = Vec::with_capacity(block.txdata.len());
		let mut idxn = Vec::with_capacity(block.txdata.len());
		for (i, tx) in block.txdata.iter().enumerate() {
			txn.push(tx);
			idxn.push(i as u32);
		}
		self.block_connected(&block.header, height, &txn, &idxn);
	}
	fn a_block_disconnected(&mut self, header: &BlockHeader, height: u32) {
		self.block_disconnected(header, height);
	}
}

impl<CS: keysinterface::ChannelKeys> AChainListener for (&mut ChannelMonitor<CS>, &ChainInterface, &FeeEstimator) {
	fn a_block_connected(&mut self, block: &Block, height: u32) {
		let mut txn = Vec::with_capacity(block.txdata.len());
		for tx in block.txdata.iter() {
			txn.push(tx);
		}
		self.0.block_connected(&txn, height, &block.bitcoin_hash(), self.1, self.2);
	}
	fn a_block_disconnected(&mut self, header: &BlockHeader, height: u32) {
		self.0.block_disconnected(height, &header.bitcoin_hash(), self.1, self.2);
	}
}

pub async fn sync_chain_monitor<CL : AChainListener + Sized>(new_block: String, old_block: String, rpc_client: &Arc<RPCClient>, mut chain_notifier: CL) {
	if old_block == "0000000000000000000000000000000000000000000000000000000000000000" { return; }

	let mut events = Vec::new();
	find_fork(&mut events, new_block, old_block, rpc_client.clone()).await;
	for event in events.iter().rev() {
		if let &ForkStep::DisconnectBlock((ref header, ref height)) = &event {
			println!("Disconnecting block {}", header.bitcoin_hash().to_hex());
			chain_notifier.a_block_disconnected(header, *height);
		}
	}
	for event in events.iter().rev() {
		if let &ForkStep::ConnectBlock((ref hash, block_height)) = &event {
			let blockhex = rpc_client.make_rpc_call("getblock", &[&("\"".to_string() + hash + "\""), "0"], false).await.unwrap();
			let block: Block = encode::deserialize(&hex_to_vec(blockhex.as_str().unwrap()).unwrap()).unwrap();
			println!("Connecting block {}", block.bitcoin_hash().to_hex());
			chain_notifier.a_block_connected(&block, *block_height);
		}
	}
}

pub async fn spawn_chain_monitor(starting_blockhash: String, fee_estimator: Arc<FeeEstimator>, rpc_client: Arc<RPCClient>, chain_interface: Arc<ChainInterface>, chain_notifier: BlockNotifierArc, mut event_notify: mpsc::Sender<()>) {
	tokio::spawn(async move {
		fee_estimator.update_values(&rpc_client).await;
		let mut interval = tokio::time::interval(Duration::from_secs(1));
		let mut cur_block = starting_blockhash;
		loop {
			interval.tick().await;
			if let Ok(v) = rpc_client.make_rpc_call("getblockchaininfo", &[], false).await {
				let new_block = v["bestblockhash"].as_str().unwrap().to_string();
				let old_block = cur_block.clone();

				if new_block == old_block { continue; }
				println!("NEW BEST BLOCK: {}!", new_block);
				cur_block = new_block.clone();

				sync_chain_monitor(new_block, old_block, &rpc_client, &chain_notifier).await;
				fee_estimator.update_values(&rpc_client).await;
				let _ = event_notify.try_send(());
				chain_interface.rebroadcast_txn().await;
			}
		}
	}).await.unwrap()
}
