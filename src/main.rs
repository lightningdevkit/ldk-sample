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

mod rpc_client;
use rpc_client::RPCClient;

use bytes::BufMut;

use futures::future;
use futures::future::Future;
use futures::{AsyncSink, Stream, Sink};
use futures::sync::mpsc;

use secp256k1::key::{PublicKey, SecretKey};
use secp256k1::Secp256k1;

use rand::{thread_rng, Rng};

use lightning::ln::{peer_handler, router, channelmanager, channelmonitor};
use lightning::ln::peer_handler::SocketDescriptor as LnSocketTrait;
use lightning::chain::chaininterface;
use lightning::util::events::EventsProvider;

use bitcoin::network::constants;
use bitcoin::network::serialize::BitcoinHash;

use std::collections::HashMap;
use std::{env, mem};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::vec::Vec;
use std::hash::Hash;
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
					us.high_prio_est.store((hp_btc_per_kb.as_f64().unwrap() * 100_000_000.0 / 1000.0) as usize, Ordering::Release);
				}
				Ok(())
			})));
		}
		{
			let us = us.clone();
			reqs.push(Box::new(rpc_client.make_rpc_call("estimatesmartfee", &vec!["18", "\"ECONOMICAL\""]).and_then(move |v| {
				if let Some(serde_json::Value::Number(np_btc_per_kb)) = v.get("feerate") {
					us.normal_est.store((np_btc_per_kb.as_f64().unwrap() * 100_000_000.0 / 1000.0) as usize, Ordering::Release);
				}
				Ok(())
			})));
		}
		{
			let us = us.clone();
			reqs.push(Box::new(rpc_client.make_rpc_call("estimatesmartfee", &vec!["144", "\"ECONOMICAL\""]).and_then(move |v| {
				if let Some(serde_json::Value::Number(bp_btc_per_kb)) = v.get("feerate") {
					us.background_est.store((bp_btc_per_kb.as_f64().unwrap() * 100_000_000.0 / 1000.0) as usize, Ordering::Release);
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

struct Connection {
	writer: Option<mpsc::Sender<bytes::Bytes>>,
	event_notify: mpsc::UnboundedSender<()>,
	pending_read: Vec<u8>,
	read_blocker: Option<futures::sync::oneshot::Sender<Result<(), std::io::Error>>>,
	read_paused: bool,
	need_disconnect: bool,
	id: u64,
}
impl Connection {
	fn schedule_read(peer_manager: Arc<peer_handler::PeerManager<SocketDescriptor>>, us: Arc<Mutex<Self>>, reader: futures::stream::SplitStream<tokio_codec::Framed<tokio::net::TcpStream, tokio_codec::BytesCodec>>) {
		let us_ref = us.clone();
		let us_close_ref = us.clone();
		let peer_manager_ref = peer_manager.clone();
		tokio::spawn(reader.for_each(move |b| {
			let unwrapper = |res: Result<Result<(), std::io::Error>, futures::sync::oneshot::Canceled>| { res.unwrap() };
			let pending_read = b.to_vec();
			{
				let mut lock = us_ref.lock().unwrap();
				assert!(lock.pending_read.is_empty());
				if lock.read_paused {
					lock.pending_read = pending_read;
					let (sender, blocker) = futures::sync::oneshot::channel();
					lock.read_blocker = Some(sender);
					return blocker.then(unwrapper);
				}
			}
			match peer_manager.read_event(&mut SocketDescriptor::new(us_ref.clone(), peer_manager.clone()), pending_read) {
				Ok(pause_read) => {
					if pause_read {
						let mut lock = us_ref.lock().unwrap();
						lock.read_paused = true;
					}
				},
				Err(e) => {
					us_ref.lock().unwrap().need_disconnect = false;
					//TODO: This sucks, find a better way:
					let (sender, blocker) = futures::sync::oneshot::channel();
					sender.send(Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e))).unwrap();
					return blocker.then(unwrapper);
				}
			}

			let _ = us_ref.lock().unwrap().event_notify.start_send(());

			//TODO: This sucks, find a better way:
			let (sender, blocker) = futures::sync::oneshot::channel();
			sender.send(Ok(())).unwrap();
			blocker.then(unwrapper)
		}).then(move |_| {
			if us_close_ref.lock().unwrap().need_disconnect {
				peer_manager_ref.disconnect_event(&SocketDescriptor::new(us_close_ref, peer_manager_ref.clone()));
				println!("Peer disconnected!");
			} else {
				println!("We disconnected peer!");
			}
			Ok(())
		}));
	}

	fn new(event_notify: mpsc::UnboundedSender<()>, stream: tokio::net::TcpStream, id: u64) -> (futures::stream::SplitStream<tokio_codec::Framed<tokio::net::TcpStream, tokio_codec::BytesCodec>>, Arc<Mutex<Self>>) {
		let (writer, reader) = tokio_codec::Framed::new(stream, tokio_codec::BytesCodec::new()).split();
		let (send_sink, send_stream) = mpsc::channel(3);
		tokio::spawn(writer.send_all(send_stream.map_err(|_| -> std::io::Error {
			unreachable!();
		})).then(|_| {
			future::result(Ok(()))
		}));
		let us = Arc::new(Mutex::new(Self { writer: Some(send_sink), event_notify, pending_read: Vec::new(), read_blocker: None, read_paused: true, need_disconnect: true, id }));

		(reader, us)
	}

	fn setup_inbound(peer_manager: Arc<peer_handler::PeerManager<SocketDescriptor>>, event_notify: mpsc::UnboundedSender<()>, stream: tokio::net::TcpStream, id: u64) {
		let (reader, us) = Self::new(event_notify, stream, id);

		if let Ok(_) = peer_manager.new_inbound_connection(SocketDescriptor::new(us.clone(), peer_manager.clone())) {
			Self::schedule_read(peer_manager, us, reader);
		}
	}

	fn setup_outbound(peer_manager: Arc<peer_handler::PeerManager<SocketDescriptor>>, event_notify: mpsc::UnboundedSender<()>, their_node_id: PublicKey, stream: tokio::net::TcpStream, id: u64) {
		let (reader, us) = Self::new(event_notify, stream, id);

		if let Ok(initial_send) = peer_manager.new_outbound_connection(their_node_id, SocketDescriptor::new(us.clone(), peer_manager.clone())) {
			if SocketDescriptor::new(us.clone(), peer_manager.clone()).send_data(&initial_send, 0, true) == initial_send.len() {
				Self::schedule_read(peer_manager, us, reader);
			} else {
				println!("Failed to write first full message to socket!");
			}
		}
	}
}

#[derive(Clone)]
struct SocketDescriptor {
	conn: Arc<Mutex<Connection>>,
	id: u64,
	peer_manager: Arc<peer_handler::PeerManager<SocketDescriptor>>,
}
impl SocketDescriptor {
	fn new(conn: Arc<Mutex<Connection>>, peer_manager: Arc<peer_handler::PeerManager<SocketDescriptor>>) -> Self {
		let id = conn.lock().unwrap().id;
		Self { conn, id, peer_manager }
	}
}
impl peer_handler::SocketDescriptor for SocketDescriptor {
	fn send_data(&mut self, data: &Vec<u8>, write_offset: usize, resume_read: bool) -> usize {
		macro_rules! schedule_read {
			($us_ref: expr) => {
				tokio::spawn(future::lazy(move || -> Result<(), ()> {
					let mut read_data = Vec::new();
					{
						let mut us = $us_ref.conn.lock().unwrap();
						mem::swap(&mut read_data, &mut us.pending_read);
					}
					if !read_data.is_empty() {
						let mut us_clone = $us_ref.clone();
						match $us_ref.peer_manager.read_event(&mut us_clone, read_data) {
							Ok(pause_read) => {
								if pause_read { return Ok(()); }
							},
							Err(_) => {
								//TODO: Not actually sure how to do this
								return Ok(());
							}
						}
					}
					let mut us = $us_ref.conn.lock().unwrap();
					if let Some(sender) = us.read_blocker.take() {
						sender.send(Ok(())).unwrap();
					}
					us.read_paused = false;
					let _ = us.event_notify.start_send(());
					Ok(())
				}));
			}
		}

		let mut us = self.conn.lock().unwrap();
		if resume_read {
			let us_ref = self.clone();
			schedule_read!(us_ref);
		}
		if data.len() == write_offset { return 0; }
		if us.writer.is_none() {
			us.read_paused = true;
			return 0;
		}

		let mut bytes = bytes::BytesMut::with_capacity(data.len() - write_offset);
		bytes.put(&data[write_offset..]);
		let write_res = us.writer.as_mut().unwrap().start_send(bytes.freeze());
		match write_res {
			Ok(res) => {
				match res {
					AsyncSink::Ready => {
						data.len() - write_offset
					},
					AsyncSink::NotReady(_) => {
						us.read_paused = true;
						let us_ref = self.clone();
						tokio::spawn(us.writer.take().unwrap().flush().then(move |writer_res| -> Result<(), ()> {
							if let Ok(writer) = writer_res {
								{
									let mut us = us_ref.conn.lock().unwrap();
									us.writer = Some(writer);
								}
								schedule_read!(us_ref);
							} // we'll fire the disconnect event on the socket reader end
							Ok(())
						}));
						0
					}
				}
			},
			Err(_) => {
				// We'll fire the disconnected event on the socket reader end
				0
			},
		}
	}
}
impl Eq for SocketDescriptor {}
impl PartialEq for SocketDescriptor {
	fn eq(&self, o: &Self) -> bool {
		self.id == o.id
	}
}
impl Hash for SocketDescriptor {
	fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
		self.id.hash(state);
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

		let channel_manager = channelmanager::ChannelManager::new(our_node_secret, FEE_PROPORTIONAL_MILLIONTHS, ANNOUNCE_CHANNELS, network, fee_estimator.clone(), monitor, chain_monitor.clone(), chain_monitor).unwrap();
		let router = Arc::new(router::Router::new(PublicKey::from_secret_key(&secp_ctx, &our_node_secret).unwrap()));

		let peer_manager = Arc::new(peer_handler::PeerManager::new(peer_handler::MessageHandler {
			chan_handler: channel_manager,
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
			rpc_client.make_rpc_call("getblockchaininfo", &Vec::new()).and_then(move |v| {
				let new_block = v["bestblockhash"].as_str().unwrap();
				if new_block == *cur_block.lock().unwrap() { return future::Either::A(future::result(Ok(()))); }
				//TODO: Figure out the list of changes to apply and notify via the chaininterface.
				println!("NEW BEST BLOCK!");
				*cur_block.lock().unwrap() = String::from(new_block);
				future::Either::B(FeeEstimator::update_values(fee_estimator, &rpc_client))
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
