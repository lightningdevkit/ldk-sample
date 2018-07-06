extern crate futures;
extern crate hyper;
extern crate serde_json;
extern crate lightning;
extern crate rand;
extern crate secp256k1;
extern crate bitcoin;
extern crate tokio;
extern crate tokio_io;
extern crate tokio_fs;
extern crate tokio_codec;
extern crate bytes;
extern crate base64;

#[macro_use]
extern crate serde_derive;

mod rpc_client;
use rpc_client::*;

mod utils;
use utils::hex_to_vec;

mod net_manager;
use net_manager::{Connection, SocketDescriptor};

use futures::future;
use futures::future::Future;
use futures::{Stream, Sink};
use futures::sync::mpsc;

use secp256k1::key::{PublicKey, SecretKey};
use secp256k1::Secp256k1;

use rand::{thread_rng, Rng};

use lightning::ln::{peer_handler, router, channelmanager, channelmonitor};
use lightning::chain::chaininterface;
use lightning::util::events::EventsProvider;

use bitcoin::blockdata::block::Block;
use bitcoin::network::constants;
use bitcoin::network::serialize::BitcoinHash;

use std::collections::HashMap;
use std::{env, mem};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::vec::Vec;
use std::time::{Instant, Duration};
use std::io::Write;

const FEE_PROPORTIONAL_MILLIONTHS: u32 = 10;
const ANNOUNCE_CHANNELS: bool = false;

#[allow(dead_code, unreachable_code)]
fn _check_usize_is_64() {
	// We assume 64-bit usizes here. If your platform has 32-bit usizes, wtf are you doing?
	unsafe { mem::transmute::<*const usize, [u8; 8]>(panic!()); }
}

fn hex_to_compressed_pubkey(hex: &str) -> Option<PublicKey> {
	let mut b = 0;
	let mut data = Vec::with_capacity(33);
	for (idx, c) in hex.as_bytes().iter().enumerate() {
		if idx >= 33*2 { break; }
		b <<= 4;
		match *c {
			b'A'...b'F' => b |= c - b'A' + 10,
			b'a'...b'f' => b |= c - b'a' + 10,
			b'0'...b'9' => b |= c - b'0',
			_ => return None,
		}
		if (idx & 1) == 1 {
			data.push(b);
			b = 0;
		}
	}
	match PublicKey::from_slice(&Secp256k1::without_caps(), &data) {
		Ok(pk) => Some(pk),
		Err(_) => None,
	}
}

struct FeeEstimator {
	background_est: AtomicUsize,
	normal_est: AtomicUsize,
	high_prio_est: AtomicUsize,
}
impl FeeEstimator {
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

struct ChainInterface {
	util: chaininterface::ChainWatchInterfaceUtil,
	txn_to_broadcast: Mutex<HashMap<bitcoin::util::hash::Sha256dHash, bitcoin::blockdata::transaction::Transaction>>,
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

struct EventHandler {
	peer_manager: Arc<peer_handler::PeerManager<SocketDescriptor>>,
}
impl EventHandler {
	fn setup(peer_manager: Arc<peer_handler::PeerManager<SocketDescriptor>>) -> mpsc::UnboundedSender<()> {
		let us = Arc::new(Self { peer_manager });
		let (sender, receiver) = mpsc::unbounded();
		tokio::spawn(receiver.for_each(move |_| {
			us.peer_manager.process_events();
			let events = us.peer_manager.get_and_clear_pending_events();
			for event in events {
				match event {
					_ => unimplemented!(),
				}
			}
			Ok(())
		}).then(|_| { Ok(()) }));
		sender
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

fn main() {
	println!("USAGE: rust-lightning-jsonrpc user:pass@rpc_host:port");
	if env::args().len() < 2 { return; }

	let rpc_client = {
		let path = env::args().skip(1).next().unwrap();
		let path_parts: Vec<&str> = path.split('@').collect();
		if path_parts.len() != 2 {
			println!("Bad RPC URL provided");
			return;
		}
		Arc::new(RPCClient::new(path_parts[0], path_parts[1]))
	};

	let mut network = constants::Network::Bitcoin;
	let cur_block = Arc::new(Mutex::new(String::from("")));
	let secp_ctx = Secp256k1::new();

	let fee_estimator = Arc::new(FeeEstimator {
		background_est: AtomicUsize::new(0),
		normal_est: AtomicUsize::new(0),
		high_prio_est: AtomicUsize::new(0),
	});

	{
		println!("Checking validity of RPC URL to bitcoind...");
		let mut thread_rt = tokio::runtime::current_thread::Runtime::new().unwrap();
		thread_rt.block_on(rpc_client.make_rpc_call("getblockchaininfo", &Vec::new()).and_then(|v| {
			assert!(v["verificationprogress"].as_f64().unwrap() > 0.99);
			assert_eq!(v["bip9_softforks"]["segwit"]["status"].as_str().unwrap(), "active");
			match v["chain"].as_str().unwrap() {
				"main" => network = constants::Network::Bitcoin,
				"test" => network = constants::Network::Testnet,
				_ => panic!("Unknown network type"),
			}
			*cur_block.lock().unwrap() = String::from(v["bestblockhash"].as_str().unwrap());
			FeeEstimator::update_values(fee_estimator.clone(), &rpc_client)
		})).unwrap();
		println!("Success! Starting up...");
	}

	let mut rt = tokio::runtime::Runtime::new().unwrap();
	rt.spawn(future::lazy(move || -> Result<(), ()> {
		let our_node_secret = {
			let mut key = [0; 32];
			thread_rng().fill_bytes(&mut key);
			SecretKey::from_slice(&secp_ctx, &key).unwrap()
		};

		let chain_monitor = Arc::new(ChainInterface {
			util: chaininterface::ChainWatchInterfaceUtil::new(),
			txn_to_broadcast: Mutex::new(HashMap::new()),
		});
		let monitor = channelmonitor::SimpleManyChannelMonitor::<lightning::chain::transaction::OutPoint>::new(chain_monitor.clone(), chain_monitor.clone());

		let channel_manager: Arc<_> = channelmanager::ChannelManager::new(our_node_secret, FEE_PROPORTIONAL_MILLIONTHS, ANNOUNCE_CHANNELS, network, fee_estimator.clone(), monitor, chain_monitor.clone(), chain_monitor.clone()).unwrap();
		let router = Arc::new(router::Router::new(PublicKey::from_secret_key(&secp_ctx, &our_node_secret).unwrap()));

		let peer_manager = Arc::new(peer_handler::PeerManager::new(peer_handler::MessageHandler {
			chan_handler: channel_manager.clone(),
			route_handler: router,
		}, our_node_secret));

		let event_notify = EventHandler::setup(peer_manager.clone());

		let listener = tokio::net::TcpListener::bind(&"0.0.0.0:9735".parse().unwrap()).unwrap();

		let peer_manager_listener = peer_manager.clone();
		let event_listener = event_notify.clone();
		let mut inbound_id = 0;
		tokio::spawn(listener.incoming().for_each(move |sock| {
			Connection::setup_inbound(peer_manager_listener.clone(), event_listener.clone(), sock, inbound_id);
			inbound_id += 2;
			Ok(())
		}).then(|_| { Ok(()) }));

		tokio::spawn(tokio::timer::Interval::new(Instant::now(), Duration::from_secs(1)).for_each(move |_| {
			let cur_block = cur_block.clone();
			let fee_estimator = fee_estimator.clone();
			let rpc_client = rpc_client.clone();
			let chain_monitor = chain_monitor.clone();
			rpc_client.make_rpc_call("getblockchaininfo", &Vec::new()).and_then(move |v| {
				let new_block = v["bestblockhash"].as_str().unwrap().to_string();
				let old_block = cur_block.lock().unwrap().clone();
				if new_block == old_block { return future::Either::A(future::result(Ok(()))); }

				let (events_tx, events_rx) = mpsc::channel(1);
				find_fork(events_tx, new_block.clone(), old_block, rpc_client.clone());
				println!("NEW BEST BLOCK!");
				*cur_block.lock().unwrap() = new_block;
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
							connect_futures.push(rpc_client.make_rpc_call("getblock", &vec![&("\"".to_string() + hash + "\""), "0"]).then(move |blockhex| {
								let block: Block = bitcoin::network::serialize::deserialize(&hex_to_vec(blockhex.unwrap().as_str().unwrap()).unwrap()).unwrap();
								println!("Connecting block {}", block.bitcoin_hash().be_hex_string());
								chain_monitor.util.block_connected_with_filtering(&block, block_height);
								Ok(())
							}));
						}
					}
					future::join_all(connect_futures)
						.then(move |_: Result<Vec<()>, ()>| { FeeEstimator::update_values(fee_estimator, &rpc_client) })
				}))
			}).then(|_| { Ok(()) })
		}).then(|_| { Ok(()) }));

		tokio::spawn(tokio::timer::Interval::new(Instant::now(), Duration::new(1, 0)).for_each(move |_| {
			//TODO: Blocked on adding txn broadcasting to rest interface:
			//      Regularly poll chain_monitor.txn_to_broadcast and send them out
			Ok(())
		}).then(|_| { Ok(()) }));

		let mut outbound_id = 1;
		println!("Started interactive shell! Commands:");
		println!("'c pubkey@host:port' Connect to given host+port, with given pubkey for auth");
		println!("'n pubkey value' Create a channel with the given connected node (by pubkey) and value in satoshis");
		print!("> "); std::io::stdout().flush().unwrap();
		tokio::spawn(tokio_codec::FramedRead::new(tokio_fs::stdin(), tokio_codec::LinesCodec::new()).for_each(move |line| {
			if line.len() > 2 && line.as_bytes()[1] == ' ' as u8 {
				match line.as_bytes()[0] {
					0x63 => { // 'c'
						match hex_to_compressed_pubkey(line.split_at(2).1) {
							Some(pk) => {
								if line.as_bytes()[2 + 33*2] == '@' as u8 {
									let parse_res: Result<std::net::SocketAddr, _> = line.split_at(2 + 33*2 + 1).1.parse();
									if let Ok(addr) = parse_res {
										print!("Attempting to connect to {}...", addr);
										match std::net::TcpStream::connect(addr) {
											Ok(stream) => {
												println!("connected, initiating handshake!");
												Connection::setup_outbound(peer_manager.clone(), event_notify.clone(), pk, tokio::net::TcpStream::from_std(stream, &tokio::reactor::Handle::current()).unwrap(), outbound_id);
												outbound_id += 2;
											},
											Err(e) => {
												println!("connection failed {:?}!", e);
											}
										}
									} else { println!("Couldn't parse host:port into a socket address"); }
								} else { println!("Invalid line, should be c pubkey@host:port"); }
							},
							None => println!("Bad PubKey for remote node"),
						}
					},
					0x6e => { // 'n'
						match hex_to_compressed_pubkey(line.split_at(2).1) {
							Some(pk) => {
								if line.as_bytes()[2 + 33*2] == ' ' as u8 {
									let parse_res: Result<u64, _> = line.split_at(2 + 33*2 + 1).1.parse();
									if let Ok(value) = parse_res {
										match channel_manager.create_channel(pk, value, 0) {
											Ok(_) => println!("Channel created, sending open_channel!"),
											Err(e) => println!("Failed to open channel: {:?}!", e),
										}
										event_notify.unbounded_send(()).unwrap();
									} else { println!("Couldn't parse second argument into a value"); }
								} else { println!("Invalid line, should be n pubkey value"); }
							},
							None => println!("Bad PubKey for remote node"),
						}
					},
					_ => println!("Unknown command: {}", line.as_bytes()[0] as char),
				}
			} else {
				println!("Unknown command line: {}", line);
			}
			print!("> "); std::io::stdout().flush().unwrap();
			Ok(())
		}).then(|_| { Ok(()) }));

		Ok(())
	}));
	rt.shutdown_on_idle().wait().unwrap();
}
