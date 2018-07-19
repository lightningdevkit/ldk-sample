use rpc_client::*;

use utils::hex_to_vec;

use bitcoin;
use lightning;
use serde_json;
use tokio;

use futures::future;
use futures::future::Future;
use futures::{Stream, Sink};
use futures::sync::mpsc;

use lightning::chain::chaininterface;

use bitcoin::blockdata::block::Block;
use bitcoin::network::serialize::BitcoinHash;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::vec::Vec;
use std::time::{Instant, Duration};
use std;

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
	fn update_values(us: Arc<Self>, rpc_client: &RPCClient) -> impl Future<Item=(), Error=()> {
		let mut reqs: Vec<Box<Future<Item=(), Error=()> + Send>> = Vec::with_capacity(3);
		{
			let us = us.clone();
			reqs.push(Box::new(rpc_client.make_rpc_call("estimatesmartfee", &vec!["6", "\"CONSERVATIVE\""]).and_then(move |v| {
				if let Some(serde_json::Value::Number(hp_btc_per_kb)) = v.get("feerate") {
					us.high_prio_est.store((hp_btc_per_kb.as_f64().unwrap() * 100_000_000.0 / 1000.0) as usize + 3, Ordering::Release);
				}
				Ok(())
			})));
		}
		{
			let us = us.clone();
			reqs.push(Box::new(rpc_client.make_rpc_call("estimatesmartfee", &vec!["18", "\"ECONOMICAL\""]).and_then(move |v| {
				if let Some(serde_json::Value::Number(np_btc_per_kb)) = v.get("feerate") {
					us.normal_est.store((np_btc_per_kb.as_f64().unwrap() * 100_000_000.0 / 1000.0) as usize + 3, Ordering::Release);
				}
				Ok(())
			})));
		}
		{
			let us = us.clone();
			reqs.push(Box::new(rpc_client.make_rpc_call("estimatesmartfee", &vec!["144", "\"ECONOMICAL\""]).and_then(move |v| {
				if let Some(serde_json::Value::Number(bp_btc_per_kb)) = v.get("feerate") {
					us.background_est.store((bp_btc_per_kb.as_f64().unwrap() * 100_000_000.0 / 1000.0) as usize + 3, Ordering::Release);
				}
				Ok(())
			})));
		}
		future::join_all(reqs).then(|_| { Ok(()) })
	}
}
impl chaininterface::FeeEstimator for FeeEstimator {
	fn get_est_sat_per_vbyte(&self, conf_target: chaininterface::ConfirmationTarget) -> u64 {
		match conf_target {
			chaininterface::ConfirmationTarget::Background => self.background_est.load(Ordering::Acquire) as u64,
			chaininterface::ConfirmationTarget::Normal => self.normal_est.load(Ordering::Acquire) as u64,
			chaininterface::ConfirmationTarget::HighPriority => self.high_prio_est.load(Ordering::Acquire) as u64,
		}
	}
}

pub struct ChainInterface {
	util: chaininterface::ChainWatchInterfaceUtil,
	txn_to_broadcast: Mutex<HashMap<bitcoin::util::hash::Sha256dHash, bitcoin::blockdata::transaction::Transaction>>,
}
impl ChainInterface {
	pub fn new() -> Self {
		ChainInterface {
			util: chaininterface::ChainWatchInterfaceUtil::new(),
			txn_to_broadcast: Mutex::new(HashMap::new()),
		}
	}
}
impl chaininterface::ChainWatchInterface for ChainInterface {
	fn install_watch_script(&self, script: bitcoin::blockdata::script::Script) {
		self.util.install_watch_script(script);
	}

	fn install_watch_outpoint(&self, outpoint: (bitcoin::util::hash::Sha256dHash, u32)) {
		self.util.install_watch_outpoint(outpoint);
	}

	fn watch_all_txn(&self) {
		self.util.watch_all_txn();
	}

	fn register_listener(&self, listener: std::sync::Weak<lightning::chain::chaininterface::ChainListener + 'static>) {
		self.util.register_listener(listener);
	}
}
impl chaininterface::BroadcasterInterface for ChainInterface {
	fn broadcast_transaction (&self, tx: &bitcoin::blockdata::transaction::Transaction) {
		self.txn_to_broadcast.lock().unwrap().insert(tx.bitcoin_hash(), tx.clone());
	}
}

enum ForkStep {
	DisconnectBlock(bitcoin::blockdata::block::BlockHeader),
	ConnectBlock((String, u32)),
}
fn find_fork_step(steps_tx: mpsc::Sender<ForkStep>, current_header: GetHeaderResponse, target_header_opt: Option<(String, GetHeaderResponse)>, rpc_client: Arc<RPCClient>) {
	if target_header_opt.is_some() && target_header_opt.as_ref().unwrap().0 == current_header.previousblockhash {
		// Target is the parent of current, we're done!
		return;
	} else if current_header.height == 1 {
		return;
	} else if target_header_opt.is_none() || target_header_opt.as_ref().unwrap().1.height < current_header.height {
		tokio::spawn(steps_tx.send(ForkStep::ConnectBlock((current_header.previousblockhash.clone(), current_header.height - 1))).then(move |send_res| {
			if let Ok(steps_tx) = send_res {
				future::Either::A(rpc_client.get_header(&current_header.previousblockhash).then(move |new_cur_header| {
					find_fork_step(steps_tx, new_cur_header.unwrap(), target_header_opt, rpc_client);
					Ok(())
				}))
			} else {
				// Caller droped the receiver, we should give up now
				future::Either::B(future::result(Ok(())))
			}
		}));
	} else {
		let target_header = target_header_opt.unwrap().1;
		// Everything below needs to disconnect target, so go ahead and do that now
		tokio::spawn(steps_tx.send(ForkStep::DisconnectBlock(target_header.to_block_header())).then(move |send_res| {
			if let Ok(steps_tx) = send_res {
				future::Either::A(if target_header.previousblockhash == current_header.previousblockhash {
					// Found the fork, also connect current and finish!
					future::Either::A(future::Either::A(
						steps_tx.send(ForkStep::ConnectBlock((current_header.previousblockhash.clone(), current_header.height - 1))).then(|_| { Ok(()) })))
				} else if target_header.height > current_header.height {
					// Target is higher, walk it back and recurse
					future::Either::B(rpc_client.get_header(&target_header.previousblockhash).then(move |new_target_header| {
						find_fork_step(steps_tx, current_header, Some((target_header.previousblockhash, new_target_header.unwrap())), rpc_client);
						Ok(())
					}))
				} else {
					// Target and current are at the same height, but we're not at fork yet, walk
					// both back and recurse
					future::Either::A(future::Either::B(
						steps_tx.send(ForkStep::ConnectBlock((current_header.previousblockhash.clone(), current_header.height - 1))).then(move |send_res| {
							if let Ok(steps_tx) = send_res {
								future::Either::A(rpc_client.get_header(&current_header.previousblockhash).then(move |new_cur_header| {
									rpc_client.get_header(&target_header.previousblockhash).then(move |new_target_header| {
										find_fork_step(steps_tx, new_cur_header.unwrap(), Some((target_header.previousblockhash, new_target_header.unwrap())), rpc_client);
										Ok(())
									})
								}))
							} else {
								// Caller droped the receiver, we should give up now
								future::Either::B(future::result(Ok(())))
							}
						})
					))
				})
			} else {
				// Caller droped the receiver, we should give up now
				future::Either::B(future::result(Ok(())))
			}
		}));
	}
}
/// Walks backwards from current_hash and target_hash finding the fork and sending ForkStep events
/// into the steps_tx Sender. There is no ordering guarantee between different ForkStep types, but
/// DisconnectBlock and ConnectBlock events are each in reverse, height-descending order.
fn find_fork(mut steps_tx: mpsc::Sender<ForkStep>, current_hash: String, target_hash: String, rpc_client: Arc<RPCClient>) {
	if current_hash == target_hash { return; }

	tokio::spawn(rpc_client.get_header(&current_hash).then(move |current_resp| {
		let current_header = current_resp.unwrap();
		assert!(steps_tx.start_send(ForkStep::ConnectBlock((current_hash, current_header.height - 1))).unwrap().is_ready());

		if current_header.previousblockhash == target_hash || current_header.height == 1 {
			// Fastpath one-new-block-connected or reached block 1
			future::Either::A(future::result(Ok(())))
		} else {
			future::Either::B(rpc_client.get_header(&target_hash).then(move |target_resp| {
				match target_resp {
					Ok(target_header) => find_fork_step(steps_tx, current_header, Some((target_hash, target_header)), rpc_client),
					Err(_) => {
						assert_eq!(target_hash, "");
						find_fork_step(steps_tx, current_header, None, rpc_client)
					},
				}
				Ok(())
			}))
		}
	}));
}

pub fn spawn_chain_monitor(fee_estimator: Arc<FeeEstimator>, rpc_client: Arc<RPCClient>, chain_monitor: Arc<ChainInterface>, event_notify: mpsc::UnboundedSender<()>) {
	tokio::spawn(FeeEstimator::update_values(fee_estimator.clone(), &rpc_client));
	let cur_block = Arc::new(Mutex::new(String::from("")));
	tokio::spawn(tokio::timer::Interval::new(Instant::now(), Duration::from_secs(1)).for_each(move |_| {
		let cur_block = cur_block.clone();
		let fee_estimator = fee_estimator.clone();
		let rpc_client = rpc_client.clone();
		let chain_monitor = chain_monitor.clone();
		let event_notify = event_notify.clone();
		rpc_client.make_rpc_call("getblockchaininfo", &Vec::new()).and_then(move |v| {
			let new_block = v["bestblockhash"].as_str().unwrap().to_string();
			let old_block = cur_block.lock().unwrap().clone();
			if new_block == old_block { return future::Either::A(future::result(Ok(()))); }

			*cur_block.lock().unwrap() = new_block.clone();
			if old_block == "" { return future::Either::A(future::result(Ok(()))); }

			let (events_tx, events_rx) = mpsc::channel(1);
			find_fork(events_tx, new_block, old_block, rpc_client.clone());
			println!("NEW BEST BLOCK!");
			future::Either::B(events_rx.collect().then(move |events_res| {
				let events = events_res.unwrap();
				for event in events.iter().rev() {
					if let &ForkStep::DisconnectBlock(ref header) = &event {
						println!("Disconnecting block {}", header.bitcoin_hash().be_hex_string());
						chain_monitor.util.block_disconnected(header);
					}
				}
				let mut connect_futures = Vec::with_capacity(events.len());
				for event in events.iter().rev() {
					if let &ForkStep::ConnectBlock((ref hash, height)) = &event {
						let block_height = *height;
						let chain_monitor = chain_monitor.clone();
						connect_futures.push(rpc_client.make_rpc_call("getblock", &[&("\"".to_string() + hash + "\""), "0"]).then(move |blockhex| {
							let block: Block = bitcoin::network::serialize::deserialize(&hex_to_vec(blockhex.unwrap().as_str().unwrap()).unwrap()).unwrap();
							println!("Connecting block {}", block.bitcoin_hash().be_hex_string());
							chain_monitor.util.block_connected_with_filtering(&block, block_height);
							Ok(())
						}));
					}
				}
				future::join_all(connect_futures)
					.then(move |_: Result<Vec<()>, ()>| { FeeEstimator::update_values(fee_estimator, &rpc_client) })
					.then(move |_| { event_notify.unbounded_send(()).unwrap(); Ok(()) })
			}))
		}).then(|_| { Ok(()) })
	}).then(|_| { Ok(()) }));
}
