#[macro_use]
extern crate serde_derive;

mod rpc_client;
use rpc_client::*;

mod utils;
use utils::*;

mod chain_monitor;
use chain_monitor::*;

use lightning_net_tokio::Connection;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio::time::Instant;

use secp256k1::key::PublicKey;
use secp256k1::Secp256k1;

use rand::{thread_rng, Rng};

use lightning::chain;
use lightning::chain::chaininterface;
use lightning::chain::keysinterface::{KeysInterface, KeysManager, SpendableOutputDescriptor, InMemoryChannelKeys};
use lightning::ln::{peer_handler, router, channelmanager, channelmonitor};
use lightning::ln::channelmonitor::ManyChannelMonitor;
use lightning::ln::channelmanager::{PaymentHash, PaymentPreimage};
use lightning::util::events::{Event, EventsProvider};
use lightning::util::logger::{Logger, Record};
use lightning::util::ser::{ReadableArgs, Writeable};
use lightning::util::config;

use bitcoin::util::{bip32, bip143};
use bitcoin::blockdata;
use bitcoin::network::constants;
use bitcoin::consensus::encode;

use bitcoin_hashes::Hash;
use bitcoin_hashes::sha256d::Hash as Sha256dHash;
use bitcoin_hashes::hex::{ToHex, FromHex};

use std::{env, mem};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::vec::Vec;
use std::time::{Duration, SystemTime};
use std::io::{Cursor, Write};
use std::fs;

const FEE_PROPORTIONAL_MILLIONTHS: u32 = 10;
const ANNOUNCE_CHANNELS: bool = false;

// TODO: There are several dropped tokio::JoinHandle's in this file where we call tokio::spawn and then
// drop the result. In the future, this may break things, see https://github.com/tokio-rs/tokio/issues/1830.

#[allow(dead_code, unreachable_code)]
fn _check_usize_is_64() {
	// We assume 64-bit usizes here. If your platform has 32-bit usizes, wtf are you doing?
	unsafe { mem::transmute::<*const usize, [u8; 8]>(panic!()); }
}

struct EventHandler {
	secp_ctx: Secp256k1<secp256k1::All>,
	network: constants::Network,
	file_prefix: String,
	rpc_client: Arc<RPCClient>,
	peer_manager: peer_handler::SimpleArcPeerManager<lightning_net_tokio::SocketDescriptor, ChannelMonitor>,
	channel_manager: channelmanager::SimpleArcChannelManager<ChannelMonitor>,
	monitor: Arc<channelmonitor::SimpleManyChannelMonitor<chain::transaction::OutPoint, InMemoryChannelKeys>>,
	broadcaster: Arc<dyn chain::chaininterface::BroadcasterInterface>,
	txn_to_broadcast: Mutex<HashMap<chain::transaction::OutPoint, blockdata::transaction::Transaction>>,
	payment_preimages: Arc<Mutex<HashMap<PaymentHash, PaymentPreimage>>>,
}
impl EventHandler {
	async fn setup(network: constants::Network, file_prefix: String, rpc_client: Arc<RPCClient>, peer_manager: peer_handler::SimpleArcPeerManager<lightning_net_tokio::SocketDescriptor, ChannelMonitor>, monitor: Arc<channelmonitor::SimpleManyChannelMonitor<chain::transaction::OutPoint, InMemoryChannelKeys>>, channel_manager: channelmanager::SimpleArcChannelManager<ChannelMonitor>, broadcaster: Arc<dyn chain::chaininterface::BroadcasterInterface>, payment_preimages: Arc<Mutex<HashMap<PaymentHash, PaymentPreimage>>>) -> mpsc::Sender<()> {
		let us = Arc::new(Self { secp_ctx: Secp256k1::new(), network, file_prefix, rpc_client, peer_manager, channel_manager, monitor, broadcaster, txn_to_broadcast: Mutex::new(HashMap::new()), payment_preimages });
		let (sender, mut receiver) = mpsc::channel(2);
		let mut self_sender = sender.clone();
		tokio::spawn(async move {
			loop {
				receiver.recv().await.unwrap();
				Self::check_handle_event(&us, &mut self_sender).await;
			}
		});
		sender
	}

	async fn check_handle_event(us: &Arc<Self>, self_sender: &mut mpsc::Sender<()>) {
		us.peer_manager.process_events();
		let mut events = us.channel_manager.get_and_clear_pending_events();
		events.append(&mut us.monitor.get_and_clear_pending_events());
		for event in events {
			match event {
				Event::FundingGenerationReady { temporary_channel_id, channel_value_satoshis, output_script, .. } => {
					let addr = bitcoin_bech32::WitnessProgram::from_scriptpubkey(&output_script[..], match us.network {
							constants::Network::Bitcoin => bitcoin_bech32::constants::Network::Bitcoin,
							constants::Network::Testnet => bitcoin_bech32::constants::Network::Testnet,
							constants::Network::Regtest => bitcoin_bech32::constants::Network::Regtest,
						}
					).expect("LN funding tx should always be to a SegWit output").to_address();
					let mut self_sender = self_sender.clone();
					let outputs = format!("{{\"{}\": {}}}", addr, channel_value_satoshis as f64 / 1_000_000_00.0).to_string();
					if let Ok(tx_hex) = us.rpc_client.make_rpc_call("createrawtransaction", &["[]", &outputs], false).await {
						let rawtx = format!("\"{}\"", tx_hex.as_str().unwrap()).to_string();
						if let Ok(funded_tx) = us.rpc_client.make_rpc_call("fundrawtransaction", &[&rawtx], false).await {
							let changepos = funded_tx["changepos"].as_i64().unwrap();
							assert!(changepos == 0 || changepos == 1);
							let funded_tx = format!("\"{}\"", funded_tx["hex"].as_str().unwrap()).to_string();
							if let Ok(signed_tx) = us.rpc_client.make_rpc_call("signrawtransactionwithwallet", &[&funded_tx], false).await {
								assert_eq!(signed_tx["complete"].as_bool().unwrap(), true);
								let tx: blockdata::transaction::Transaction = encode::deserialize(&hex_to_vec(&signed_tx["hex"].as_str().unwrap()).unwrap()).unwrap();
								let outpoint = chain::transaction::OutPoint {
									txid: tx.txid(),
									index: if changepos == 0 { 1 } else { 0 },
								};
								us.channel_manager.funding_transaction_generated(&temporary_channel_id, outpoint);
								us.txn_to_broadcast.lock().unwrap().insert(outpoint, tx);
								let _ = self_sender.try_send(());
								println!("Generated funding tx!");
							}
						}
					}
				},
				Event::FundingBroadcastSafe { funding_txo, .. } => {
					let mut txn = us.txn_to_broadcast.lock().unwrap();
					let tx = txn.remove(&funding_txo).unwrap();
					us.broadcaster.broadcast_transaction(&tx);
					println!("Broadcast funding tx {}!", tx.txid());
				},
				Event::PaymentReceived { payment_hash, payment_secret, amt } => {
					println!("handling pr in 60 secs...");
					let mut self_sender = self_sender.clone();
					let images = us.payment_preimages.lock().unwrap();
					if let Some(payment_preimage) = images.get(&payment_hash) {
						if us.channel_manager.claim_funds(payment_preimage.clone(), &payment_secret, amt) { // Cheating by using amt here!
							println!("Moneymoney! {} id {}", amt, hex_str(&payment_hash.0));
						} else {
							us.channel_manager.fail_htlc_backwards(&payment_hash, &payment_secret);
							println!("Received payment but we didn't know the preimage :(");
						}
					}
					let _ = self_sender.try_send(());
				},
				Event::PaymentSent { payment_preimage } => {
					println!("Less money :(, proof: {}", hex_str(&payment_preimage.0));
				},
				Event::PaymentFailed { payment_hash, rejected_by_dest } => {
					println!("{} failed id {}!", if rejected_by_dest { "Send" } else { "Route" }, hex_str(&payment_hash.0));
				},
				Event::PendingHTLCsForwardable { time_forwardable } => {
					let us = us.clone();
					let mut self_sender = self_sender.clone();
					tokio::spawn(async move {
						tokio::time::delay_until(Instant::now() + time_forwardable).await;
						us.channel_manager.process_pending_htlc_forwards();
						let _ = self_sender.try_send(());
					});
				},
				Event::SpendableOutputs { mut outputs } => {
					for output in outputs.drain(..) {
						match output {
							SpendableOutputDescriptor:: StaticOutput { outpoint, .. } => {
								println!("Got on-chain output Bitcoin Core should know how to claim at {}:{}", hex_str(&outpoint.txid[..]), outpoint.vout);
							},
							SpendableOutputDescriptor::DynamicOutputP2WSH { outpoint, key, witness_script, to_self_delay, output } => {
								println!("Got on-chain output ({}:{}) to redeemScript {} spendable with key {} at time {}...", hex_str(&outpoint.txid[..]), outpoint.vout, hex_str(&witness_script[..]), hex_str(&key[..]), to_self_delay);
								let addr = us.rpc_client.make_rpc_call("getnewaddress",
									&["\"rust-lightning dynamic output p2wsh claim\"", "\"bech32\""], false).await.unwrap();
								let address = bitcoin::util::address::Address::from_str(addr.as_str().unwrap()).unwrap();
								let mut tx = bitcoin::Transaction {
									input: vec![bitcoin::TxIn {
										previous_output: outpoint,
										script_sig: bitcoin::Script::new(),
										sequence: to_self_delay as u32,
										witness: vec![vec![0], witness_script.to_bytes()],
									}],
									lock_time: 0,
									output: vec![bitcoin::TxOut {
										script_pubkey: address.script_pubkey(),
										value: output.value,
									}],
									version: 2,
								};
								let sighash = secp256k1::Message::from_slice(&bip143::SighashComponents::new(&tx).sighash_all(&tx.input[0], &witness_script, output.value)).unwrap();
								tx.input[0].witness.insert(0, us.secp_ctx.sign(&sighash, &key).serialize_der()[..].to_vec());
								println!("Spending it with {}", encode::serialize_hex(&tx));
								us.broadcaster.broadcast_transaction(&tx);
							},
							SpendableOutputDescriptor::DynamicOutputP2WPKH { outpoint, key, output: _ } => {
								println!("Got on-chain output ({}:{}) we should claim directly with key {}", hex_str(&outpoint.txid[..]), outpoint.vout, hex_str(&key[..]));
								us.rpc_client.make_rpc_call("importprivkey",
									&[&("\"".to_string() + &bitcoin::util::key::PrivateKey{ key, compressed: true, network: us.network}.to_wif() + "\""), "\"rust-lightning dynamic output p2wpkh\"", "false"], false).await.unwrap();
								let _ = us.rpc_client.make_rpc_call("rescanblockchain", &["610000"], false).await;
							},
						}
					}
				},
			}
		}

		let filename = format!("{}/manager_data", us.file_prefix);
		let tmp_filename = filename.clone() + ".tmp";

		{
			let mut f = fs::File::create(&tmp_filename).unwrap();
			us.channel_manager.write(&mut f).unwrap();
		}
		fs::rename(&tmp_filename, &filename).unwrap();
	}
}

struct ChannelMonitor {
	monitor: Arc<channelmonitor::SimpleManyChannelMonitor<chain::transaction::OutPoint, InMemoryChannelKeys>>,
	file_prefix: String,
}
impl ChannelMonitor {
	async fn load_from_disk(file_prefix: &String, cur_tip_hash: String, rpc_client: Arc<RPCClient>, broadcaster: Arc<ChainInterface>, feeest: Arc<FeeEstimator>) -> Vec<(chain::transaction::OutPoint, channelmonitor::ChannelMonitor<InMemoryChannelKeys>)> {
		let mut res = Vec::new();
		for file_option in fs::read_dir(file_prefix).unwrap() {
			let mut loaded = false;
			let file = file_option.unwrap();
			if let Some(filename) = file.file_name().to_str() {
				if filename.is_ascii() && filename.len() > 65 {
					if let Ok(txid) = Sha256dHash::from_hex(filename.split_at(64).0) {
						if let Ok(index) = filename.split_at(65).1.split('.').next().unwrap().parse() {
							if let Ok(contents) = fs::read(&file.path()) {
								if let Ok((last_block_hash, mut loaded_monitor)) = <(Sha256dHash, channelmonitor::ChannelMonitor<InMemoryChannelKeys>)>::read(&mut Cursor::new(&contents), Arc::new(LogPrinter{})) {
									let monitor_data = (&mut loaded_monitor, &*broadcaster, &*feeest);
									sync_chain_monitor(cur_tip_hash.clone(), format!("{:x}", last_block_hash), &rpc_client, monitor_data).await;

									res.push((chain::transaction::OutPoint { txid, index }, loaded_monitor));
									loaded = true;
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
		res
	}

	fn load_from_vec(&self, mut monitors: Vec<(chain::transaction::OutPoint, channelmonitor::ChannelMonitor<InMemoryChannelKeys>)>) {
		for (outpoint, monitor) in monitors.drain(..) {
			if let Err(_) = self.monitor.add_monitor(outpoint, monitor) {
				panic!("Failed to load monitor that deserialized");
			}
		}
	}

	fn write_monitor(&self, monitor: &channelmonitor::ChannelMonitor<InMemoryChannelKeys>) -> Result<(), channelmonitor::ChannelMonitorUpdateErr> {
		macro_rules! try_fs {
			($res: expr) => {
				match $res {
					Ok(res) => res,
					Err(_) => return Err(channelmonitor::ChannelMonitorUpdateErr::PermanentFailure),
				}
			}
		}
		// Do a crazy dance with lots of fsync()s to be overly cautious here...
		// We never want to end up in a state where we've lost the old data, or end up using the
		// old data on power loss after we've returned
		// Note that this actually *isn't* enough (at least on Linux)! We need to fsync an fd with
		// the containing dir, but Rust doesn't let us do that directly, sadly. TODO: Fix this with
		// the libc crate!
		let funding_txo = monitor.get_funding_txo().unwrap();
		let filename = format!("{}/{}_{}", self.file_prefix, funding_txo.txid.to_hex(), funding_txo.index);
		let tmp_filename = filename.clone() + ".tmp";

		{
			let mut f = try_fs!(fs::File::create(&tmp_filename));
			try_fs!(monitor.write_for_disk(&mut f));
			try_fs!(f.sync_all());
		}
		// We don't need to create a backup if didn't already have the file, but in any other case
		// try to create the backup and expect failure on fs::copy() if eg there's a perms issue.
		let need_bk = match fs::metadata(&filename) {
			Ok(data) => {
				if !data.is_file() { return Err(channelmonitor::ChannelMonitorUpdateErr::PermanentFailure); }
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
		Ok(())
	}
}
#[cfg(any(target_os = "macos", target_os = "ios"))]
#[error("OSX creatively eats your data, using Lightning on OSX is unsafe")]
struct ERR {}

impl channelmonitor::ManyChannelMonitor<InMemoryChannelKeys> for ChannelMonitor {
	fn add_monitor(&self, funding_txo: chain::transaction::OutPoint, monitor: channelmonitor::ChannelMonitor<InMemoryChannelKeys>) -> Result<(), channelmonitor::ChannelMonitorUpdateErr> {
		self.monitor.add_monitor(funding_txo, monitor)?;
		self.write_monitor(&self.monitor.get_monitor_ref_by_key(&funding_txo).unwrap())
	}

	fn update_monitor(&self, funding_txo: chain::transaction::OutPoint, update: channelmonitor::ChannelMonitorUpdate) -> Result<(), channelmonitor::ChannelMonitorUpdateErr> {
		self.monitor.update_monitor(funding_txo, update)?;
		self.write_monitor(&self.monitor.get_monitor_ref_by_key(&funding_txo).unwrap())
	}

	fn get_and_clear_pending_htlcs_updated(&self) -> Vec<channelmonitor::HTLCUpdate> {
		self.monitor.get_and_clear_pending_htlcs_updated()
	}
}

struct LogPrinter {}
impl Logger for LogPrinter {
	fn log(&self, record: &Record) {
		if !record.args.to_string().contains("Received message of type 258") && !record.args.to_string().contains("Received message of type 256") && !record.args.to_string().contains("Received message of type 257") {
			eprintln!("{:<5} [{} : {}, {}] {}", record.level.to_string(), record.module_path, record.file, record.line, record.args);
		}
	}
}

#[tokio::main]
async fn main() {
	println!("USAGE: rust-lightning-jsonrpc user:pass@rpc_host:port storage_directory_path [port]");
	if env::args().len() < 3 { return; }

	lightning_invoice::check_platform();

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

	println!("Checking validity of RPC URL to bitcoind...");
	if let Ok(v) = rpc_client.make_rpc_call("getblockchaininfo", &[], false).await {
		assert!(v["verificationprogress"].as_f64().unwrap() > 0.99);
		assert!(
			v["bip9_softforks"]["segwit"]["status"].as_str() == Some("active") ||
			v["softforks"]["segwit"]["type"].as_str() == Some("buried"));
		match v["chain"].as_str().unwrap() {
			"main" => network = constants::Network::Bitcoin,
			"test" => network = constants::Network::Testnet,
			"regtest" => network = constants::Network::Regtest,
			_ => panic!("Unknown network type"),
		}
	}
	println!("Success! Starting up...");

	if network == constants::Network::Bitcoin {
		panic!("LOL, you're insane");
	}

	let data_path = env::args().skip(2).next().unwrap();
	if !fs::metadata(&data_path).unwrap().is_dir() {
		println!("Need storage_directory_path to exist and be a directory (or symlink to one)");
		return;
	}
	let _ = fs::create_dir(data_path.clone() + "/monitors"); // If it already exists, ignore, hopefully perms are ok

	let port: u16 = match env::args().skip(3).next().map(|p| p.parse()) {
		Some(Ok(p)) => p,
		Some(Err(e)) => panic!(e),
		None => 9735,
	};

	let logger = Arc::new(LogPrinter {});

	let our_node_seed = if let Ok(seed) = fs::read(data_path.clone() + "/key_seed") {
		assert_eq!(seed.len(), 32);
		let mut key = [0; 32];
		key.copy_from_slice(&seed);
		key
	} else {
		let mut key = [0; 32];
		thread_rng().fill_bytes(&mut key);
		let mut f = fs::File::create(data_path.clone() + "/key_seed").unwrap();
		f.write_all(&key).expect("Failed to write seed to disk");
		f.sync_all().expect("Failed to sync seed to disk");
		key
	};
	let cur = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap();
	let keys = Arc::new(KeysManager::new(&our_node_seed, network, logger.clone(), cur.as_secs(), cur.subsec_nanos()));
	let (import_key_1, import_key_2) = bip32::ExtendedPrivKey::new_master(network, &our_node_seed).map(|extpriv| {
		(extpriv.ckd_priv(&secp_ctx, bip32::ChildNumber::from_hardened_idx(1).unwrap()).unwrap().private_key.key,
		 extpriv.ckd_priv(&secp_ctx, bip32::ChildNumber::from_hardened_idx(2).unwrap()).unwrap().private_key.key)
	}).unwrap();
	let chain_monitor = Arc::new(ChainInterface::new(rpc_client.clone(), network, logger.clone()));
	let block_notifier: chaininterface::BlockNotifierArc = Arc::new(chaininterface::BlockNotifier::new(chain_monitor.clone()));

	let mut join_handles = Vec::new();

	rpc_client.make_rpc_call("importprivkey",
			&[&("\"".to_string() + &bitcoin::util::key::PrivateKey{ key: import_key_1, compressed: true, network}.to_wif() + "\""), "\"rust-lightning ChannelMonitor claim\"", "false"], false).await.unwrap();
	rpc_client.make_rpc_call("importprivkey",
			&[&("\"".to_string() + &bitcoin::util::key::PrivateKey{ key: import_key_2, compressed: true, network}.to_wif() + "\""), "\"rust-lightning cooperative close\"", "false"], false).await.unwrap();

	fee_estimator.update_values(&rpc_client).await;

	let starting_blockhash = rpc_client.make_rpc_call("getblockchaininfo", &[], false).await.unwrap()["bestblockhash"].as_str().unwrap().to_string();

	let mut monitors_loaded = ChannelMonitor::load_from_disk(&(data_path.clone() + "/monitors"), starting_blockhash.clone(), rpc_client.clone(), chain_monitor.clone(), fee_estimator.clone()).await;
	let monitor = Arc::new(ChannelMonitor {
		monitor: Arc::new(channelmonitor::SimpleManyChannelMonitor::new(chain_monitor.clone(), chain_monitor.clone(), logger.clone(), fee_estimator.clone())),
		file_prefix: data_path.clone() + "/monitors",
	});
	block_notifier.register_listener(Arc::clone(&(monitor.monitor.clone() as Arc<dyn chaininterface::ChainListener>)));

	let mut config: config::UserConfig = Default::default();
	config.channel_options.fee_proportional_millionths = FEE_PROPORTIONAL_MILLIONTHS;
	config.channel_options.announced_channel = ANNOUNCE_CHANNELS;
	config.own_channel_config.minimum_depth = 1;

	let channel_manager = if let Ok(mut f) = fs::File::open(data_path.clone() + "/manager_data") {
		let (last_block_hash, manager) = {
			let mut monitors_refs = HashMap::new();
			for (outpoint, monitor) in monitors_loaded.iter_mut() {
				monitors_refs.insert(*outpoint, monitor);
			}
			<(Sha256dHash, channelmanager::SimpleArcChannelManager<ChannelMonitor>)>::read(&mut f, channelmanager::ChannelManagerReadArgs {
				keys_manager: keys.clone(),
				fee_estimator: fee_estimator.clone(),
				monitor: monitor.clone(),
				//chain_monitor: chain_monitor.clone(),
				tx_broadcaster: chain_monitor.clone(),
				logger: logger.clone(),
				default_config: config,
				channel_monitors: &mut monitors_refs,
			}).expect("Failed to deserialize channel manager")
		};
		monitor.load_from_vec(monitors_loaded);
		if format!("{:x}", last_block_hash) != starting_blockhash {
			sync_chain_monitor(starting_blockhash.clone(), format!("{:x}", last_block_hash), &rpc_client, &manager).await;
		}
		manager
	} else {
		if !monitors_loaded.is_empty() {
			panic!("Found some channel monitors but no channel state!");
		}
		Arc::new(channelmanager::ChannelManager::new(network, fee_estimator.clone(), monitor.clone(), chain_monitor.clone(), logger.clone(), keys.clone(), config, 0).unwrap()) //TODO: Get blockchain height
	};
	block_notifier.register_listener(Arc::clone(&(channel_manager.clone() as Arc<dyn chaininterface::ChainListener>)));
	let router = Arc::new(router::Router::new(PublicKey::from_secret_key(&secp_ctx, &keys.get_node_secret()), chain_monitor.clone(), logger.clone()));

	let mut ephemeral_data = [0; 32];
	rand::thread_rng().fill_bytes(&mut ephemeral_data);
	let peer_manager = Arc::new(peer_handler::PeerManager::new(peer_handler::MessageHandler {
		chan_handler: channel_manager.clone(),
		route_handler: router.clone(),
	}, keys.get_node_secret(), &ephemeral_data, logger.clone()));

	let payment_preimages = Arc::new(Mutex::new(HashMap::new()));
	let mut event_notify = EventHandler::setup(network, data_path, rpc_client.clone(), peer_manager.clone(), monitor.monitor.clone(), channel_manager.clone(), chain_monitor.clone(), payment_preimages.clone()).await;

	println!("Initial setup complete, binding port and running!");

	let mut listener = tokio::net::TcpListener::bind(("::".parse::<std::net::Ipv6Addr>().unwrap(), port)).await.unwrap();

	let peer_manager_listener = peer_manager.clone();
	let event_listener = event_notify.clone();
	join_handles.push(tokio::spawn(async move {
		loop {
			let sock = listener.accept().await.unwrap().0;
			println!("Got new inbound connection, waiting on them to start handshake...");
			let peer_manager_listener = peer_manager_listener.clone();
			let event_listener = event_listener.clone();
			tokio::spawn(async move {
				Connection::setup_inbound(peer_manager_listener, event_listener, sock).await;
			});
		}
	}));

	join_handles.push(tokio::spawn(
		spawn_chain_monitor(starting_blockhash, fee_estimator, rpc_client, chain_monitor, block_notifier, event_notify.clone())
	));

	join_handles.push(tokio::spawn(async move {
		let mut intvl = tokio::time::interval(Duration::from_secs(1));
		loop {
			intvl.tick().await;
			//TODO: Regularly poll chain_monitor.txn_to_broadcast and send them out
		}
	}));

	let peer_manager_timer = peer_manager.clone();
	let chan_manager_timer = channel_manager.clone();
	join_handles.push(tokio::spawn(async move {
		let mut intvl = tokio::time::interval(Duration::from_secs(60));
		loop {
			intvl.tick().await;
			//TODO: This seems to break our connections: peer_manager_timer.timer_tick_occured();
			chan_manager_timer.timer_chan_freshness_every_min();
		}
	}));

	println!("Bound on port 9735! Our node_id: {}", hex_str(&PublicKey::from_secret_key(&secp_ctx, &keys.get_node_secret()).serialize()));
	println!("Started interactive shell! Commands:");
	println!("'c pubkey@host:port' Connect to given host+port, with given pubkey for auth");
	println!("'n pubkey value push_value' Create a channel with the given connected node (by pubkey), value in satoshis, and push the given msat value");
	println!("'k channel_id' Close a channel with the given id");
	println!("'f all' Force close all channels, closing to chain");
	println!("'l p' List the node_ids of all connected peers");
	println!("'l c' List details about all channels");
	println!("'s invoice [amt]' Send payment to an invoice, optionally with amount as whole msat if its not in the invoice");
	println!("'p' Gets a new invoice for receiving funds");
	print!("> "); std::io::stdout().flush().unwrap();
	let mut lines = BufReader::new(tokio::io::stdin()).lines();
	while let Ok(Some(line)) = lines.next_line().await {
		macro_rules! fail_return {
			() => {
				print!("> "); std::io::stdout().flush().unwrap();
				continue;
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
											let peer_manager = peer_manager.clone();
											let event_notify = event_notify.clone();
											join_handles.push(tokio::spawn(async move {
												Connection::setup_outbound(peer_manager, event_notify, pk,
													tokio::net::TcpStream::from_std(stream).unwrap()).await;
											}));
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
								let mut args = line.split_at(2 + 33*2 + 1).1.split(' ');
								if let Some(value_str) = args.next() {
									if let Some(push_str) = args.next() {
										if let Ok(value) = value_str.parse() {
											if let Ok(push) = push_str.parse() {
												match channel_manager.create_channel(pk, value, push, 0) {
													Ok(_) => println!("Channel created, sending open_channel!"),
													Err(e) => println!("Failed to open channel: {:?}!", e),
												}
												let _ = event_notify.try_send(());
											} else { println!("Couldn't parse third argument into a push value"); }
										} else { println!("Couldn't parse second argument into a value"); }
									} else { println!("Couldn't read third argument"); }
								} else { println!("Couldn't read second argument"); }
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
									let _ = event_notify.try_send(());
								},
								Err(e) => println!("Failed to close channel: {:?}", e),
							}
						} else { println!("Bad channel_id hex"); }
					} else { println!("Bad channel_id hex"); }
				},
				0x66 => { // 'f'
					if line.len() == 5 && line.as_bytes()[2] == 'a' as u8 && line.as_bytes()[3] == 'l' as u8 && line.as_bytes()[4] == 'l' as u8 {
						channel_manager.force_close_all_channels();
					} else {
						println!("Single-channel force-close not yet implemented");
					}
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
								println!("id: {}, short_id: {}, peer: {}, value: {} sat, live: {}", hex_str(&chan_info.channel_id[..]), short_id, hex_str(&chan_info.remote_network_id.serialize()), chan_info.channel_value_satoshis, chan_info.is_live);
							} else {
								println!("id: {}, not yet confirmed, peer: {}, value: {} sat, live: {}", hex_str(&chan_info.channel_id[..]), hex_str(&chan_info.remote_network_id.serialize()), chan_info.channel_value_satoshis, chan_info.is_live);
							}
						}
					} else {
						println!("Listing of non-peer/channel objects not yet implemented");
					}
				},
				0x73 => { // 's'
					let mut args = line.split_at(2).1.split(' ');
					match lightning_invoice::Invoice::from_str(args.next().unwrap()) {
						Ok(invoice) => {
							if match invoice.currency() {
								lightning_invoice::Currency::Bitcoin => constants::Network::Bitcoin,
								lightning_invoice::Currency::BitcoinTestnet => constants::Network::Testnet,
								lightning_invoice::Currency::Regtest => constants::Network::Regtest,
							} != network {
								println!("Wrong network on invoice");
							} else {
								let arg2 = args.next();
								let amt = if let Some(amt) = invoice.amount_pico_btc().and_then(|amt| {
									if amt % 10 != 0 { None } else { Some(amt / 10) }
								}) {
									if arg2.is_some() {
										println!("Invoice had amount, you shouldn't specify one");
										fail_return!();
									}
									amt
								} else {
									if arg2.is_none() {
										println!("Invoice didn't have an amount, you should specify one");
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

								if let Some(pubkey) = invoice.payee_pub_key() {
									if *pubkey != invoice.recover_payee_pub_key() {
										println!("Invoice had non-equal duplicative target node_id (ie was malformed)");
										fail_return!();
									}
								}

								let mut route_hint = Vec::with_capacity(invoice.routes().len());
								for route in invoice.routes() {
									if route.len() != 1 {
										println!("Invoice contained multi-hop non-public route, ignoring as yet unsupported");
									} else {
										route_hint.push(router::RouteHint {
											src_node_id: route[0].pubkey,
											short_channel_id: slice_to_be64(&route[0].short_channel_id),
											fee_base_msat: route[0].fee_base_msat,
											fee_proportional_millionths: route[0].fee_proportional_millionths,
											cltv_expiry_delta: route[0].cltv_expiry_delta,
											htlc_minimum_msat: 0,
										});
									}
								}

								let final_cltv = invoice.min_final_cltv_expiry().unwrap_or(&9);
								if *final_cltv > std::u32::MAX as u64 {
									println!("Invoice had garbage final cltv");
									fail_return!();
								}
								match router.get_route(&invoice.recover_payee_pub_key(), Some(&channel_manager.list_usable_channels()), &route_hint, amt, *final_cltv as u32) {
									Ok(route) => {
										let mut payment_hash = PaymentHash([0; 32]);
										payment_hash.0.copy_from_slice(&invoice.payment_hash()[..]);
										match channel_manager.send_payment(route, payment_hash, None) {
											Ok(()) => {
												println!("Sending {} msat", amt);
												let _ = event_notify.try_send(());
											},
											Err(e) => {
												println!("Failed to send HTLC: {:?}", e);
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
					let payment_hash = bitcoin_hashes::sha256::Hash::hash(&payment_preimage);
					//TODO: Store this on disk somewhere!
					payment_preimages.lock().unwrap().insert(PaymentHash(payment_hash.into_inner()), PaymentPreimage(payment_preimage));
					println!("payment_hash: {}", hex_str(&payment_hash.into_inner()));

					let invoice_res = lightning_invoice::InvoiceBuilder::new(match network {
							constants::Network::Bitcoin => lightning_invoice::Currency::Bitcoin,
							constants::Network::Testnet => lightning_invoice::Currency::BitcoinTestnet,
							constants::Network::Regtest => lightning_invoice::Currency::BitcoinTestnet, //TODO
						}).payment_hash(payment_hash).description("rust-lightning-bitcoinrpc invoice".to_string())
						//.route(chans)
						.current_timestamp()
						.build_signed(|msg_hash| {
							secp_ctx.sign_recoverable(msg_hash, &keys.get_node_secret())
						});
					match invoice_res {
						Ok(invoice) => println!("Invoice: {}", invoice),
						Err(e) => println!("Error creating invoice: {:?}", e),
					}
				},
				_ => println!("Unknown command: {}", line.as_bytes()[0] as char),
			}
		} else {
			println!("Unknown command line: {}", line);
		}
		print!("> "); std::io::stdout().flush().unwrap();
	}
}
