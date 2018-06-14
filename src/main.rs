extern crate futures;
extern crate hyper;
extern crate serde_json;
extern crate lightning;
extern crate rand;
extern crate secp256k1;
extern crate bitcoin;
extern crate tokio;
extern crate tokio_io;
extern crate tokio_codec;
extern crate bytes;

use bytes::BufMut;

use hyper::{Client,StatusCode};
use futures::future;
use futures::future::Future;
use futures::{AsyncSink, Stream, Sink};

use secp256k1::key::{PublicKey, SecretKey};
use secp256k1::Secp256k1;

use rand::{thread_rng, Rng};

use lightning::ln::{peer_handler, router, channelmanager, channelmonitor};
use lightning::chain::chaininterface;

use bitcoin::network::constants;
use bitcoin::network::serialize::BitcoinHash;

use std::collections::HashMap;
use std::{env, mem};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::vec::Vec;
use std::os::unix::io::{RawFd, AsRawFd};
use std::hash::Hash;
use std::time::{Instant, Duration};

use tokio::io::AsyncRead;

const FEE_PROPORTIONAL_MILLIONTHS: u32 = 10;
const ANNOUNCE_CHANNELS: bool = false;

#[allow(dead_code, unreachable_code)]
fn _check_usize_is_64() {
	// We assume 64-bit usizes here. If your platform has 32-bit usizes, wtf are you doing?
	unsafe { mem::transmute::<*const usize, [u8; 8]>(panic!()); }
}

struct FeeEstimator {
	background_est: AtomicUsize,
	normal_est: AtomicUsize,
	high_prio_est: AtomicUsize,
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
	writer: Option<futures::stream::SplitSink<tokio_io::codec::Framed<tokio::net::TcpStream, tokio_codec::BytesCodec>>>,
	pending_read: Vec<u8>,
	read_blocker: Option<futures::sync::oneshot::Sender<Result<(), std::io::Error>>>,
	read_paused: bool,
	need_disconnect: bool,
	fd: RawFd,
}
impl Connection {
	fn setup(peer_manager: Arc<peer_handler::PeerManager<SocketDescriptor>>, stream: tokio::net::TcpStream) {
		let fd = stream.as_raw_fd();
		let (writer, reader) = stream.framed(tokio_codec::BytesCodec::new()).split();
		let us = Arc::new(Mutex::new(Self { writer: Some(writer), pending_read: Vec::new(), read_blocker: None, read_paused: true, need_disconnect: true, fd }));

		let peer_manager_ref = peer_manager.clone();
		let us_ref = us.clone();
		if let Ok(_) = peer_manager.new_inbound_connection(SocketDescriptor::new(us.clone(), peer_manager.clone())) {
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
				match peer_manager_ref.read_event(&mut SocketDescriptor::new(us_ref.clone(), peer_manager_ref.clone()), pending_read) {
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

				//TODO: This sucks, find a better way:
				let (sender, blocker) = futures::sync::oneshot::channel();
				sender.send(Ok(())).unwrap();
				blocker.then(unwrapper)
			}).then(move |_| {
				if us.lock().unwrap().need_disconnect {
					peer_manager.disconnect_event(&SocketDescriptor::new(us, peer_manager.clone()));
				}
				Ok(())
			}));
		}
	}
}

#[derive(Clone)]
struct SocketDescriptor {
	conn: Arc<Mutex<Connection>>,
	fd: RawFd,
	peer_manager: Arc<peer_handler::PeerManager<SocketDescriptor>>,
}
impl SocketDescriptor {
	fn new(conn: Arc<Mutex<Connection>>, peer_manager: Arc<peer_handler::PeerManager<SocketDescriptor>>) -> Self {
		let fd = conn.lock().unwrap().fd.clone();
		Self { conn, fd, peer_manager }
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
		self.fd == o.fd
	}
}
impl Hash for SocketDescriptor {
	fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
		self.fd.hash(state);
	}
}

fn main() {
	println!("USAGE: rust-lightning-jsonrpc URL");
	if env::args().len() < 2 { return; }

	let client = Client::new();

	let uri_base = env::args().skip(1).next().unwrap();

	let mut network = constants::Network::Bitcoin;
	let cur_block = Arc::new(Mutex::new(String::from("")));
	let secp_ctx = Secp256k1::new();

	let fee_estimator = Arc::new(FeeEstimator {
		background_est: AtomicUsize::new(0),
		normal_est: AtomicUsize::new(0),
		high_prio_est: AtomicUsize::new(0),
	});

	{
		println!("Checking validity of URL to a REST-running bitcoind...");
		let mut thread_rt = tokio::runtime::current_thread::Runtime::new().unwrap();
		thread_rt.block_on(client.get((uri_base.clone() + "/rest/chaininfo.json").parse().unwrap()).and_then(|res| {
			assert_eq!(res.status(), StatusCode::OK);

			res.into_body().concat2().and_then(|body| {
				let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
				assert!(v["verificationprogress"].as_f64().unwrap() > 0.99);
				assert_eq!(v["bip9_softforks"]["segwit"]["status"].as_str().unwrap(), "active");
				match v["chain"].as_str().unwrap() {
					"main" => network = constants::Network::Bitcoin,
					"test" => network = constants::Network::Testnet,
					_ => panic!("Unknown network type"),
				}
				*cur_block.lock().unwrap() = String::from(v["bestblockhash"].as_str().unwrap());
				Ok(())
			})
		})).unwrap();
		println!("Success! Starting up...");
		//TODO: Blocked on Bitcoin Core #11770: Fill in fee_estimator values!
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
		let monitor = channelmonitor::SimpleManyChannelMonitor::<(bitcoin::util::hash::Sha256dHash, u16)>::new(chain_monitor.clone(), chain_monitor.clone());

		let channel_manager = channelmanager::ChannelManager::new(our_node_secret, FEE_PROPORTIONAL_MILLIONTHS, ANNOUNCE_CHANNELS, network, fee_estimator, monitor, chain_monitor.clone(), chain_monitor).unwrap();
		let router = Arc::new(router::Router::new(PublicKey::from_secret_key(&secp_ctx, &our_node_secret).unwrap()));

		let peer_manager = Arc::new(peer_handler::PeerManager::new(peer_handler::MessageHandler {
			chan_handler: channel_manager,
			route_handler: router,
		}, our_node_secret));

		let listener = tokio::net::TcpListener::bind(&"0.0.0.0:9735".parse().unwrap()).unwrap();

		let peer_manager_listener = peer_manager.clone();
		tokio::spawn(listener.incoming().for_each(move |sock| {
			Connection::setup(peer_manager_listener.clone(), sock);
			Ok(())
		}).then(|_| { Ok(()) }));

		let req_running = Arc::new(AtomicBool::new(false));
		tokio::spawn(tokio::timer::Interval::new(Instant::now(), Duration::new(1, 0)).for_each(move |_| {
			if !req_running.swap(true, Ordering::AcqRel) {
				let req_ref = req_running.clone();
				let block_ref = cur_block.clone();
				tokio::spawn(client.get((uri_base.clone() + "/rest/chaininfo.json").parse().unwrap()).and_then(move |res| {
					assert_eq!(res.status(), StatusCode::OK);

					res.into_body().concat2().and_then(move |body| {
						let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
						let new_block = v["bestblockhash"].as_str().unwrap();
						if new_block == *block_ref.lock().unwrap() { return Ok(()); }
						//TODO: Figure out the list of changes to apply and notify via the chaininterface.
						println!("NEW BEST BLOCK!");
						//TODO: Blocked on Bitcoin Core #11770: Fill in fee_estimator values!
						*block_ref.lock().unwrap() = String::from(new_block);
						req_ref.store(false, Ordering::Release);
						Ok(())
					})
				}).then(|_| { Ok(()) }));
			}
			Ok(())
		}).then(|_| { Ok(()) }));

		tokio::spawn(tokio::timer::Interval::new(Instant::now(), Duration::new(1, 0)).for_each(move |_| {
			//TODO: Blocked on adding txn broadcasting to rest interface:
			//      Regularly poll chain_monitor.txn_to_broadcast and send them out
			Ok(())
		}).then(|_| { Ok(()) }));

		Ok(())
	}));
	rt.shutdown_on_idle().wait().unwrap();
}
