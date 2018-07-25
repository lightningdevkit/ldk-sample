extern crate futures;
extern crate hyper;
extern crate serde_json;
extern crate lightning;
extern crate lightning_invoice;
extern crate rand;
extern crate secp256k1;
extern crate bitcoin;
extern crate tokio;
extern crate tokio_io;
extern crate tokio_fs;
extern crate tokio_codec;
extern crate bytes;
extern crate base64;
extern crate bitcoin_bech32;
extern crate crypto;

#[macro_use]
extern crate serde_derive;

mod rpc_client;
use rpc_client::*;

mod utils;

mod chain_monitor;
use chain_monitor::*;

mod net_manager;
use net_manager::{Connection, SocketDescriptor};

use crypto::digest::Digest;
use crypto::sha2::Sha256;

use futures::future;
use futures::future::Future;
use futures::Stream;
use futures::sync::mpsc;

use secp256k1::key::{PublicKey, SecretKey};
use secp256k1::Secp256k1;

use rand::{thread_rng, Rng};

use lightning::chain;
use lightning::ln::{peer_handler, router, channelmanager, channelmonitor};
use lightning::ln::channelmonitor::ManyChannelMonitor;
use lightning::util::events::{Event, EventsProvider};

use bitcoin::blockdata;
use bitcoin::network::{constants, serialize};
use bitcoin::util::hash::Sha256dHash;

use std::{env, mem};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::vec::Vec;
use std::time::{Instant, Duration};
use std::io::Write;
use std::fs;

const FEE_PROPORTIONAL_MILLIONTHS: u32 = 10;
const ANNOUNCE_CHANNELS: bool = false;

#[allow(dead_code, unreachable_code)]
fn _check_usize_is_64() {
	// We assume 64-bit usizes here. If your platform has 32-bit usizes, wtf are you doing?
	unsafe { mem::transmute::<*const usize, [u8; 8]>(panic!()); }
}

fn hex_to_vec(hex: &str) -> Option<Vec<u8>> {
	let mut b = 0;
	let mut data = Vec::with_capacity(hex.len() / 2);
	for (idx, c) in hex.as_bytes().iter().enumerate() {
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
	Some(data)
}

fn hex_to_compressed_pubkey(hex: &str) -> Option<PublicKey> {
	let data = match hex_to_vec(&hex[0..33*2]) {
		Some(bytes) => bytes,
		None => return None
	};
	match PublicKey::from_slice(&Secp256k1::without_caps(), &data) {
		Ok(pk) => Some(pk),
		Err(_) => None,
	}
}

#[inline]
fn hex_str(value: &[u8]) -> String {
	let mut res = String::with_capacity(64);
	for v in value {
		res += &format!("{:02x}", v);
	}
	res
}

#[inline]
pub fn slice_to_be64(v: &[u8]) -> u64 {
	((v[0] as u64) << 8*7) |
	((v[1] as u64) << 8*6) |
	((v[2] as u64) << 8*5) |
	((v[3] as u64) << 8*4) |
	((v[4] as u64) << 8*3) |
	((v[5] as u64) << 8*2) |
	((v[6] as u64) << 8*1) |
	((v[7] as u64) << 8*0)
}

struct EventHandler {
	network: constants::Network,
	rpc_client: Arc<RPCClient>,
	peer_manager: Arc<peer_handler::PeerManager<SocketDescriptor>>,
	channel_manager: Arc<channelmanager::ChannelManager>,
	broadcaster: Arc<chain::chaininterface::BroadcasterInterface>,
	txn_to_broadcast: Mutex<HashMap<chain::transaction::OutPoint, blockdata::transaction::Transaction>>,
	payment_preimages: Arc<Mutex<HashMap<[u8; 32], [u8; 32]>>>,
}
impl EventHandler {
	fn setup(network: constants::Network, rpc_client: Arc<RPCClient>, peer_manager: Arc<peer_handler::PeerManager<SocketDescriptor>>, channel_manager: Arc<channelmanager::ChannelManager>, broadcaster: Arc<chain::chaininterface::BroadcasterInterface>, payment_preimages: Arc<Mutex<HashMap<[u8; 32], [u8; 32]>>>) -> mpsc::UnboundedSender<()> {
		let us = Arc::new(Self { network, rpc_client, peer_manager, channel_manager, broadcaster, txn_to_broadcast: Mutex::new(HashMap::new()), payment_preimages });
		let (sender, receiver) = mpsc::unbounded();
		let self_sender = sender.clone();
		tokio::spawn(receiver.for_each(move |_| {
			us.peer_manager.process_events();
			let events = us.peer_manager.get_and_clear_pending_events();
			for event in events {
				match event {
					Event::FundingGenerationReady { temporary_channel_id, channel_value_satoshis, output_script, .. } => {
						let addr = bitcoin_bech32::WitnessProgram::from_scriptpubkey(&output_script[..], match us.network {
								constants::Network::Bitcoin => bitcoin_bech32::constants::Network::Bitcoin,
								constants::Network::Testnet => bitcoin_bech32::constants::Network::Testnet,
							}
						).expect("LN funding tx should always be to a SegWit output").to_address();
						let us = us.clone();
						let self_sender = self_sender.clone();
						return future::Either::A(us.rpc_client.make_rpc_call("createrawtransaction", &["[]", &format!("{{\"{}\": {}}}", addr, channel_value_satoshis as f64 / 1_000_000_00.0)], false).and_then(move |tx_hex| {
							us.rpc_client.make_rpc_call("fundrawtransaction", &[&format!("\"{}\"", tx_hex.as_str().unwrap())], false).and_then(move |funded_tx| {
								let changepos = funded_tx["changepos"].as_i64().unwrap();
								assert!(changepos == 0 || changepos == 1);
								us.rpc_client.make_rpc_call("signrawtransaction", &[&format!("\"{}\"", funded_tx["hex"].as_str().unwrap())], false).and_then(move |signed_tx| {
									assert_eq!(signed_tx["complete"].as_bool().unwrap(), true);
									let tx: blockdata::transaction::Transaction = serialize::deserialize(&hex_to_vec(&signed_tx["hex"].as_str().unwrap()).unwrap()).unwrap();
									let outpoint = chain::transaction::OutPoint {
										txid: tx.txid(),
										index: if changepos == 0 { 1 } else { 0 },
									};
									us.channel_manager.funding_transaction_generated(&temporary_channel_id, outpoint);
									us.txn_to_broadcast.lock().unwrap().insert(outpoint, tx);
									self_sender.unbounded_send(()).unwrap();
									println!("Generated funding tx!");
									Ok(())
								})
							})
						}));
					},
					Event::FundingBroadcastSafe { funding_txo, .. } => {
						let mut txn = us.txn_to_broadcast.lock().unwrap();
						let tx = txn.remove(&funding_txo).unwrap();
						us.broadcaster.broadcast_transaction(&tx);
						println!("Broadcast funding tx {}!", tx.txid());
					},
					Event::PaymentReceived { payment_hash, amt } => {
						let images = us.payment_preimages.lock().unwrap();
						if let Some(payment_preimage) = images.get(&payment_hash) {
							if us.channel_manager.claim_funds(payment_preimage.clone()) {
								println!("Moneymoney! {} id {}", amt, hex_str(&payment_hash));
							} else {
								println!("Failed to claim money we were told we had?");
							}
						} else {
							us.channel_manager.fail_htlc_backwards(&payment_hash);
							println!("Received payment but we didn't know the preimage :(");
						}
						self_sender.unbounded_send(()).unwrap();
					},
					Event::PaymentSent { payment_preimage } => {
						println!("Less money :(, proof: {}", hex_str(&payment_preimage));
					},
					Event::PaymentFailed { payment_hash } => {
						println!("Send failed id {}!", hex_str(&payment_hash));
					},
					Event::PendingHTLCsForwardable { time_forwardable } => {
						let us = us.clone();
						let self_sender = self_sender.clone();
						tokio::spawn(tokio::timer::Delay::new(time_forwardable).then(move |_| {
							us.channel_manager.process_pending_htlc_forwards();
							self_sender.unbounded_send(()).unwrap();
							Ok(())
						}));
					},
					_ => panic!(),
				}
			}
			future::Either::B(future::result(Ok(())))
		}).then(|_| { Ok(()) }));
		sender
	}
}

struct ChannelMonitor {
	monitor: Arc<channelmonitor::SimpleManyChannelMonitor<chain::transaction::OutPoint>>,
	file_prefix: String,
}
impl ChannelMonitor {
	fn load_from_disk(&self) {
		for file_option in fs::read_dir(&self.file_prefix).unwrap() {
			let mut loaded = false;
			let file = file_option.unwrap();
			if let Some(filename) = file.file_name().to_str() {
				if filename.is_ascii() && filename.len() > 65 {
					if let Ok(txid) = Sha256dHash::from_hex(filename.split_at(64).0) {
						if let Ok(index) = filename.split_at(65).1.split('.').next().unwrap().parse() {
							if let Ok(contents) = fs::read(&file.path()) {
								if let Some(loaded_monitor) = channelmonitor::ChannelMonitor::deserialize(&contents) {
									if let Ok(_) = self.monitor.add_update_monitor(chain::transaction::OutPoint { txid, index }, loaded_monitor) {
										loaded = true;
									}
								}
							}
						}
					}
				}
			}
			if !loaded {
				println!("WARNING: Failed to read one of the channel monitor storage files! Check perms!");
			}
		}
	}
}
#[cfg(any(target_os = "macos", target_os = "ios"))]
#[error "OSX creatively eats your data, using Lightning on OSX is unsafe"]
struct ERR {}

impl channelmonitor::ManyChannelMonitor for ChannelMonitor {
	fn add_update_monitor(&self, funding_txo: chain::transaction::OutPoint, monitor: channelmonitor::ChannelMonitor) -> Result<(), channelmonitor::ChannelMonitorUpdateErr> {
		macro_rules! try_fs {
			($res: expr) => {
				match $res {
					Ok(res) => res,
					Err(_) => return Err(channelmonitor::ChannelMonitorUpdateErr::TemporaryFailure),
				}
			}
		}
		// Do a crazy dance with lots of fsync()s to be overly cautious here...
		// We never want to end up in a state where we've lost the old data, or end up using the
		// old data on power loss after we've returned
		// Note that this actually *isn't* enough (at least on Linux)! We need to fsync an fd with
		// the containing dir, but Rust doesn't let us do that directly, sadly. TODO: Fix this with
		// the libc crate!
		let new_channel_data = monitor.serialize_for_disk();
		let filename = format!("{}/{}_{}", self.file_prefix, funding_txo.txid.be_hex_string(), funding_txo.index);
		let tmp_filename = filename.clone() + ".tmp";
		{
			let mut f = try_fs!(fs::File::create(&tmp_filename));
			try_fs!(f.write_all(&new_channel_data));
			try_fs!(f.sync_all());
		}
		// We don't need to create a backup if didn't already have the file, but in any other case
		// try to create the backup and expect failure on fs::copy() if eg there's a perms issue.
		let need_bk = match fs::metadata(&filename) {
			Ok(data) => {
				if !data.is_file() { return Err(channelmonitor::ChannelMonitorUpdateErr::TemporaryFailure); }
				true
			},
			Err(e) => match e.kind() {
				std::io::ErrorKind::NotFound => false,
				_ => true,
			}
		};
		let bk_filename = filename.clone() + ".bk";
		if need_bk {
			try_fs!(fs::copy(&filename, &bk_filename));
			{
				let f = try_fs!(fs::File::open(&bk_filename));
				try_fs!(f.sync_all());
			}
		}
		try_fs!(fs::rename(&tmp_filename, &filename));
		{
			let f = try_fs!(fs::File::open(&filename));
			try_fs!(f.sync_all());
		}
		if need_bk {
			try_fs!(fs::remove_file(&bk_filename));
		}
		self.monitor.add_update_monitor(funding_txo, monitor)
	}
}

fn main() {
	println!("USAGE: rust-lightning-jsonrpc user:pass@rpc_host:port storage_directory_path");
	if env::args().len() < 3 { return; }

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
	let secp_ctx = Secp256k1::new();

	let fee_estimator = Arc::new(FeeEstimator::new());

	{
		println!("Checking validity of RPC URL to bitcoind...");
		let mut thread_rt = tokio::runtime::current_thread::Runtime::new().unwrap();
		thread_rt.block_on(rpc_client.make_rpc_call("getblockchaininfo", &[], false).and_then(|v| {
			assert!(v["verificationprogress"].as_f64().unwrap() > 0.99);
			assert_eq!(v["bip9_softforks"]["segwit"]["status"].as_str().unwrap(), "active");
			match v["chain"].as_str().unwrap() {
				"main" => network = constants::Network::Bitcoin,
				"test" => network = constants::Network::Testnet,
				_ => panic!("Unknown network type"),
			}
			Ok(())
		})).unwrap();
		println!("Success! Starting up...");
	}

	if network == constants::Network::Bitcoin {
		panic!("LOL, you're insane");
	}

	let our_node_secret = {
		let mut key = [0; 32];
		thread_rng().fill_bytes(&mut key);
		SecretKey::from_slice(&secp_ctx, &key).unwrap()
	};

	let data_path = env::args().skip(2).next().unwrap();
	if !fs::metadata(&data_path).unwrap().is_dir() {
		println!("Need storage_directory_path to exist and be a directory (or symlink to one)");
		return;
	}
	let _ = fs::create_dir(data_path.clone() + "/monitors"); // If it already exists, ignore, hopefully perms are ok

	let chain_monitor = Arc::new(ChainInterface::new(rpc_client.clone()));
	let monitor = Arc::new(ChannelMonitor {
		monitor: channelmonitor::SimpleManyChannelMonitor::new(chain_monitor.clone(), chain_monitor.clone()),
		file_prefix: data_path + "/monitors",
	});
	monitor.load_from_disk();

	let channel_manager: Arc<_> = channelmanager::ChannelManager::new(our_node_secret, FEE_PROPORTIONAL_MILLIONTHS, ANNOUNCE_CHANNELS, network, fee_estimator.clone(), monitor, chain_monitor.clone(), chain_monitor.clone()).unwrap();
	let router = Arc::new(router::Router::new(PublicKey::from_secret_key(&secp_ctx, &our_node_secret).unwrap()));

	let peer_manager = Arc::new(peer_handler::PeerManager::new(peer_handler::MessageHandler {
		chan_handler: channel_manager.clone(),
		route_handler: router.clone(),
	}, our_node_secret));

	let mut rt = tokio::runtime::Runtime::new().unwrap();
	rt.spawn(future::lazy(move || -> Result<(), ()> {
		let payment_preimages = Arc::new(Mutex::new(HashMap::new()));
		let event_notify = EventHandler::setup(network, rpc_client.clone(), peer_manager.clone(), channel_manager.clone(), chain_monitor.clone(), payment_preimages.clone());

		let listener = tokio::net::TcpListener::bind(&"0.0.0.0:9735".parse().unwrap()).unwrap();

		let peer_manager_listener = peer_manager.clone();
		let event_listener = event_notify.clone();
		let mut inbound_id = 0;
		tokio::spawn(listener.incoming().for_each(move |sock| {
			println!("Got new inbound connection, waiting on them to start handshake...");
			Connection::setup_inbound(peer_manager_listener.clone(), event_listener.clone(), sock, inbound_id);
			inbound_id += 2;
			Ok(())
		}).then(|_| { Ok(()) }));

		spawn_chain_monitor(fee_estimator, rpc_client, chain_monitor, event_notify.clone());

		tokio::spawn(tokio::timer::Interval::new(Instant::now(), Duration::new(1, 0)).for_each(move |_| {
			//TODO: Blocked on adding txn broadcasting to rest interface:
			//      Regularly poll chain_monitor.txn_to_broadcast and send them out
			Ok(())
		}).then(|_| { Ok(()) }));

		let mut outbound_id = 1;
		println!("Bound on port 9735! Our node_id: {}", hex_str(&PublicKey::from_secret_key(&secp_ctx, &our_node_secret).unwrap().serialize()));
		println!("Started interactive shell! Commands:");
		println!("'c pubkey@host:port' Connect to given host+port, with given pubkey for auth");
		println!("'n pubkey value' Create a channel with the given connected node (by pubkey) and value in satoshis");
		println!("'k channel_id' Close a channel with the given id");
		println!("'l p' List the node_ids of all connected peers");
		println!("'l c' List details about all channels");
		//TODO: remove pubkey requirement but needs invoice upgrades
		//(see https://github.com/rust-bitcoin/rust-lightning-invoice/issues/6)
		println!("'s invoice pubkey [amt]' Send payment to an invoice, optionally with amount as whole msat if its not in the invoice");
		println!("'p' Gets a new payment_hash for receiving funds");
		print!("> "); std::io::stdout().flush().unwrap();
		tokio::spawn(tokio_codec::FramedRead::new(tokio_fs::stdin(), tokio_codec::LinesCodec::new()).for_each(move |line| {
			macro_rules! fail_return {
				() => {
					print!("> "); std::io::stdout().flush().unwrap();
					return Ok(());
				}
			}
			if line.len() > 2 && line.as_bytes()[1] == ' ' as u8 {
				match line.as_bytes()[0] {
					0x63 => { // 'c'
						match hex_to_compressed_pubkey(line.split_at(2).1) {
							Some(pk) => {
								if line.as_bytes()[2 + 33*2] == '@' as u8 {
									let parse_res: Result<std::net::SocketAddr, _> = line.split_at(2 + 33*2 + 1).1.parse();
									if let Ok(addr) = parse_res {
										print!("Attempting to connect to {}...", addr);
										match std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(10)) {
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
					0x6b => { // 'k'
						if line.len() == 64 + 2 {
							if let Some(chan_id_vec) = hex_to_vec(line.split_at(2).1) {
								let mut channel_id = [0; 32];
								channel_id.copy_from_slice(&chan_id_vec);
								match channel_manager.close_channel(&channel_id) {
									Ok(()) => {
										println!("Ok, channel closing!");
										event_notify.unbounded_send(()).unwrap();
									},
									Err(e) => println!("Failed to close channel: {:?}", e),
								}
							} else { println!("Bad channel_id hex"); }
						} else { println!("Bad channel_id hex"); }
					},
					0x6c => { // 'l'
						if line.as_bytes()[2] == 'p' as u8 {
							let mut nodes = String::new();
							for node_id in peer_manager.get_peer_node_ids() {
								nodes += &format!("{}, ", hex_str(&node_id.serialize()));
							}
							println!("Connected nodes: {}", nodes);
						} else if line.as_bytes()[2] == 'c' as u8 {
							println!("All channels:");
							for chan_info in channel_manager.list_channels() {
								if let Some(short_id) = chan_info.short_channel_id {
									println!("id: {}, short_id: {}, peer: {}, value: {} sat", hex_str(&chan_info.channel_id[..]), short_id, hex_str(&chan_info.remote_network_id.serialize()), chan_info.channel_value_satoshis);
								} else {
									println!("id: {}, not yet confirmed, peer: {}, value: {} sat", hex_str(&chan_info.channel_id[..]), hex_str(&chan_info.remote_network_id.serialize()), chan_info.channel_value_satoshis);
								}
							}
						} else {
							println!("Listing of non-peer/channel objects not yet implemented");
						}
					},
					0x73 => { // 's'
						let mut args = line.split_at(2).1.split(' ');
						match lightning_invoice::RawInvoice::from_str(args.next().unwrap()) {
							Ok(invoice) => {
								let target_node = Some(hex_to_compressed_pubkey(args.next().unwrap()).unwrap());

								if match invoice.hrp.currency {
									lightning_invoice::Currency::Bitcoin => constants::Network::Bitcoin,
									lightning_invoice::Currency::BitcoinTestnet => constants::Network::Testnet,
								} != network {
									println!("Wrong network on invoice");
								} else {
									let arg2 = args.next();
									let mut amt = if let Some(amt) = invoice.hrp.raw_amount {
										if arg2.is_some() {
											println!("Invoice contained an amount, can't specify it again");
											fail_return!();
										}
										amt * 1_0000_0000 * 1000
									} else {
										if arg2.is_none() {
											println!("Invoice was missing amount, you should specify one");
											fail_return!();
										}
										if invoice.hrp.si_prefix.is_some() {
											println!("Invoice had no amount but had a multiplier (ie was malformed)");
											fail_return!();
										}
										match arg2.unwrap().parse() {
											Ok(amt) => amt,
											Err(_) => {
												println!("Provided amount was garbage");
												fail_return!();
											}
										}
									};
									if let Some(mult) = invoice.hrp.si_prefix {
										amt /= match mult {
											// 10^-3
											lightning_invoice::SiPrefix::Milli => 1_000,
											// 10^-6
											lightning_invoice::SiPrefix::Micro => 1_000_000,
											// 10^-9
											lightning_invoice::SiPrefix::Nano =>  1_000_000_000,
											// 10^-12
											lightning_invoice::SiPrefix::Pico => {
												if amt % 10 != 0 {
													println!("Invoice had amount with non-round number of millisat (ie was malformed)");
													fail_return!();
												}
												1_000_000_000_000
											},
										};
									}
									let mut payment_hash = None;
									let mut final_cltv = None;
									let mut route_hint = None;
									for field in invoice.data.tagged_fields {
										match field {
											lightning_invoice::RawTaggedField::KnownTag(lightning_invoice::TaggedField::PaymentHash(invoice_hash)) => {
												if payment_hash.is_some() {
													println!("Invoice had duplicative payment hash (ie was malformed)");
													fail_return!();
												}
												payment_hash = Some(invoice_hash);
											},
											lightning_invoice::RawTaggedField::KnownTag(lightning_invoice::TaggedField::Description(_)) => {}
											lightning_invoice::RawTaggedField::KnownTag(lightning_invoice::TaggedField::PayeePubKey(invoice_target)) => {
												if *target_node.as_ref().unwrap() != invoice_target {
													println!("Invoice had wrong target node_id");
													fail_return!();
												}
											},
											lightning_invoice::RawTaggedField::KnownTag(lightning_invoice::TaggedField::DescriptionHash(_)) => {},
											lightning_invoice::RawTaggedField::KnownTag(lightning_invoice::TaggedField::ExpiryTime(_)) => {
												//TODO: Test against current system time
											},
											lightning_invoice::RawTaggedField::KnownTag(lightning_invoice::TaggedField::MinFinalCltvExpiry(invoice_cltv)) => {
												if final_cltv.is_some() {
													println!("Invoice had duplicative final cltv (ie was malformed)");
													fail_return!();
												}
												if invoice_cltv > std::u32::MAX as u64 {
													println!("Invoice had garbage final cltv");
													fail_return!();
												}
												final_cltv = Some(invoice_cltv as u32);
											},
											lightning_invoice::RawTaggedField::KnownTag(lightning_invoice::TaggedField::Fallback(_)) => {
												// We don't support fallbacks
											},
											lightning_invoice::RawTaggedField::KnownTag(lightning_invoice::TaggedField::Route(mut invoice_routes)) => {
												if route_hint.is_some() {
													println!("Invoice had duplicative target node_id (ie was malformed)");
													fail_return!();
												}
												let mut routes_converted = Vec::with_capacity(invoice_routes.len());
												for node in invoice_routes.drain(..) {
													routes_converted.push(router::RouteHint {
														src_node_id: node.pubkey,
														short_channel_id: slice_to_be64(&node.short_channel_id),
														fee_base_msat: node.fee_base_msat as u64,
														fee_proportional_millionths: node.fee_proportional_millionths,
														cltv_expiry_delta: node.cltv_expiry_delta,
														htlc_minimum_msat: 0,
													});
												}
												route_hint = Some(routes_converted);
											},
											lightning_invoice::RawTaggedField::UnknownTag(_, _) => {
												println!("Warning: invoice contained unknown fields");
											},
										}
									}
									if route_hint.is_none() { route_hint = Some(Vec::new()); }
									if payment_hash.is_none() || final_cltv.is_none() {
										println!("Invoice was missing some key bits");
										fail_return!();
									}
									match router.get_route(&target_node.unwrap(), Some(&channel_manager.list_usable_channels()), &route_hint.unwrap(), amt, final_cltv.unwrap()) {
										Ok(route) => {
											match channel_manager.send_payment(route, payment_hash.unwrap()) {
												Ok(()) => {
													println!("Sending {} msat", amt);
													event_notify.unbounded_send(()).unwrap();
												},
												Err(e) => {
													println!("Failed to send HTLC: {}", e.err);
												}
											}
										},
										Err(e) => {
											println!("Failed to find route: {}", e.err);
										}
									}
								}
							},
							Err(_) => {
								println!("Bad invoice");
							},
						}
					},
					0x70 => { // 'p'
						let mut payment_preimage = [0; 32];
						thread_rng().fill_bytes(&mut payment_preimage);
						let mut sha = Sha256::new();
						sha.input(&payment_preimage);
						let mut payment_hash = [0; 32];
						sha.result(&mut payment_hash);
						//TODO: Store this on disk somewhere!
						println!("payment_hash: {}", hex_str(&payment_hash));
						payment_preimages.lock().unwrap().insert(payment_hash, payment_preimage);
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
