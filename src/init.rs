/// This file contains the sample initialization logic.

#[macro_use]
mod utils;
use utils::{LdkConfig, LogPrinter};

use ldk::utils::RpcClient;

mod sampled;
use sampled::*;

use bitcoin::BlockHash;
use bitcoin::network::constants;
use bitcoin::secp256k1::key::PublicKey;
use bitcoin::secp256k1::Secp256k1;

use bitcoin::hashes::hex::FromHex;

use lightning_persister::FilesystemPersister;

use lightning::chain::keysinterface::{KeysManager, KeysInterface};
use lightning::util::events::Event;
use lightning::util::config;
use lightning::util::ser::Writer;

use rand::{thread_rng, Rng};

use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io::Read;
use std::process::exit;
use std::sync::Arc;
use std::str::FromStr;
use std::thread::sleep;
use std::time::{Duration, SystemTime};

use tokio::sync::mpsc;
use tokio::sync::mpsc::{Receiver, Sender};

const FEE_PROPORTIONAL_MILLIONTHS: u32 = 10;
const ANNOUNCE_CHANNELS: bool = false;

#[tokio::main]
async fn main() {

	// Step 1: Load data from canonical daemon directory
	//
	// A lightning node may persistent set of datas of different importance:
	//	- the channel monitor state (`/monitors`), a file critical to ensure funds safety
	//	- the channel state (`/channels`), a file critical to avoid channel force-closure
	//	- a configuration file (`/ldk-node.conf`), daemon-specific configurations options
	//	- a debug file (`/debug.log`)

	let daemon_dir = if env::args().len() > 2 {
		let path_dir = env::args().skip(2).next().unwrap();
		let path = if fs::metadata(&path_dir).unwrap().is_dir() {
			path_dir + "/ldk-node.conf"
		} else { exit_on!("Need daemon directory to exist and be a directory"); };
		path
	} else {
		let mut path = if let Some(path) = env::home_dir() {
			path
		} else { exit_on!("No home directory found"); };
		path.push(".ldk-node/ldk-node.conf");
		String::from_str(path.to_str().unwrap()).unwrap()
	};

	let ldk_config = LdkConfig::create(&daemon_dir);

	//TODO: think if default values make sense
	let mut config: config::UserConfig = Default::default();
	config.channel_options.fee_proportional_millionths = FEE_PROPORTIONAL_MILLIONTHS;
	config.channel_options.announced_channel = ANNOUNCE_CHANNELS;
	config.own_channel_config.minimum_depth = 1;

	let logger = Arc::new(LogPrinter::new("/home/user/.ldk-node/debug.log"));

	// Step 2: Start a JSON-RPC client to a Bitcoind instance
	//
	// A lightning node must always have access to diverse base layer functionalities:
	// 	- a lively view of the chain, either directly connected to a full-node or as lightclient
	// 	- a reliable tx-broadcast mechanism
	// 	- a honest fee estimation mechanism

	let bitcoind_client = Arc::new(RpcClient::new(&ldk_config.get_str("bitcoind_credentials"), &ldk_config.get_str("bitcoind_hostport")));

	let network = if let Ok(net) = ldk_config.get_network() {
		net
	} else { exit_on!("No network found in config file"); };

	log_sample!(logger, "Checking validity of RPC URL to bitcoind...");
	if let Ok(v) = bitcoind_client.make_rpc_call("getblockchaininfo", &[], false).await {
		assert!(v["verificationprogress"].as_f64().unwrap() > 0.99);
		assert!(
			v["bip9_softforks"]["segwit"]["status"].as_str() == Some("active") ||
			v["softforks"]["segwit"]["type"].as_str() == Some("buried"));
		let bitcoind_net = match v["chain"].as_str().unwrap() {
			"main" => constants::Network::Bitcoin,
			"test" => constants::Network::Testnet,
			"regtest" => constants::Network::Regtest,
			_ => panic!("Unknown network type"),
		};
		if !(network == bitcoind_net) { exit_on!("Divergent network between LDK node and bitcoind"); }
	} else { exit_on!("Failed to connect to bitcoind RPC server, check your `bitcoind_hostport`/`bitcoind_credentials` settings"); }

	//TODO: connect to bitcoind
	let fee_estimator = Arc::new(SampleFeeEstimator::new());

	//TODO: connect to bitcoind
	let tx_broadcaster = Arc::new(TxBroadcaster::new());

	//TODO: replace by daemon dir
	let data_path = String::from("/home/user/.ldk-node");
	if !fs::metadata(&data_path).unwrap().is_dir() {
		exit_on!("Need storage_directory_path to exist and be a directory (or symlink to one)");
	}
	let _ = fs::create_dir(data_path.clone() + "/monitors"); // If it already exists, ignore, hopefully perms are ok

	let persister = Arc::new(FilesystemPersister::new(data_path.clone() + "/monitors"));

	//TODO: if doesn't exist take best chain from bitcoind
	let starting_blockhash = if let Ok(mut blockhash_file) = OpenOptions::new().read(true).open(data_path.clone() + "/blockhash") {
		let mut buffer = Vec::new();
		let ret = if let Ok(_) = blockhash_file.read_to_end(&mut buffer) {
			let ret = if let Ok(hash) = BlockHash::from_hex(&(std::str::from_utf8(&buffer).unwrap())) {
				Some(hash)
			} else { None };
			ret
		} else { None };
		ret
	} else { None };
	if starting_blockhash.is_none() { exit_on!("Need `blockhash` in `./ldk-node` directory"); }

	let ln_port = if let Ok(port) = ldk_config.get_ln_port() {
		port
	} else { exit_on!("No ln port found in config file"); };

	let ldk_port = if let Ok(port) = ldk_config.get_ldk_port() {
		port
	} else { exit_on!("No ldk port found in config file"); };

	// Step 3: Initialize key material
	//
	// A lightning node must manage special key materials to cover special needs:
	//
	// 	- maintain a network-wise persistent identity, the node pubkey
	// 	- sign channel opening, updates and closing

	let secp_ctx = Secp256k1::new();

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
	let keys = Arc::new(KeysManager::new(&our_node_seed, network, cur.as_secs(), cur.subsec_nanos()));

	println!("Node Pubkey {}", PublicKey::from_secret_key(&secp_ctx, &keys.get_node_secret()));

	let mut join_handles = Vec::new();

	// Step 4: Create a ChainProvider
	//
	// A lightning chain backend must provide the following services:
	//
	//	- validate the chain, (cf. `chain::Watcher` documentation)
	//	- connect blocks to `ChannelManager`/`chain::Watch`
	//	- provide utxo filtering registration (`chain::Filter`)
	//	- provide utxo set access (`chain::Access`)
	// 
	// This sample node implementation relies on the default LDK block utilities (`lightning-block-sync`)
	// to communicate with a bitcoind instance through the HTTP interface. Block updates flow
	// to their final consumers (`ChannelManager/`chain::Watch`) through `ChainConnector.
	log_sample!(logger, "Starting chain backend thread...");

	let (outbound_blocks_chan_manager, inbound_blocks_chan_manager) = mpsc::channel(100);

	let buffer_blocks = Arc::new(ChainConnector::new());
	let chain_listener = buffer_blocks.clone();

	let (handle_1, handle_2) = setup_chain_backend(starting_blockhash.unwrap(), (ldk_config.get_str("bitcoind_credentials"), ldk_config.get_str("bitcoind_hostport")), buffer_blocks, chain_listener, outbound_blocks_chan_manager).await;
	join_handles.push(handle_1);
	join_handles.push(handle_2);

	// Step 5: Start chain processing.
	//
	// A lightning node must watch and process the chain lively to take the following actions:
	// 	
	// 	- claim HTLC-onchain
	// 	- punish the counterparty
	// 	- detect a preimage to settle an incoming HTLC on the previous channel link
	// 	- detect pay-to-us outputs (cf. `SpendableOutputDescriptors`)
	// 	- fee-bump any transactions generated due to the aforementioned actions (cf. `OnchainTxHandler`)
	//

	log_sample!(logger, "Starting chain processing...");

	let chain_source = Arc::new(ChainSource::new());
	let handle = setup_chain_processing(chain_source.clone(), tx_broadcaster.clone(), fee_estimator.clone(), logger.clone(), persister.clone()).await;
	join_handles.push(handle);

	// Step 6 : Start a event handler.
	//
	// A lightning node may gather event notifications for consumptions by user custom component
	// (e.g display an app notification, ...).

	log_sample!(logger, "Starting event handler...");

	let (outbound_event_peers, inbound_event_peers) = mpsc::channel(1);
	let (outbound_event_chan_manager, inbound_event_chan_manager) = mpsc::channel(1);

	let inbound_event_connector = InboundEventConnector::new(inbound_event_peers, inbound_event_chan_manager);

	let handle = setup_event_handler(inbound_event_connector).await;
	join_handles.push(handle);

	// Step 7 : Start channel manager.
	//
	// A lightning node core is the offchain state machine, accepting state update proposal from
	// counterparty or relaying user demand to update, and propagating change to the rest of the
	// implementation accordingly.
	//
	// TODO

	log_sample!(logger, "Starting channel manager...");

	let (outbound_chanman_rpccmd, inbound_chanman_rpccmd) = mpsc::channel(1);
	let (outbound_chanman_netmsg, inbound_chanman_netmsg) = mpsc::channel(1);
	let (outbound_chanman_msg_events, inbound_chanman_msg_events) = mpsc::channel(1);
	let (outbound_peerman_notify, inbound_peerman_notify) = mpsc::channel(1);

	let outbound_chan_manager_connector = OutboundChanManagerConnector::new(outbound_chanman_msg_events, outbound_peerman_notify);

	let chain_watchdog = Arc::new(ChainWatchdog::new());
	let (handle_1, handle_2) = setup_channel_manager(inbound_chanman_rpccmd, inbound_chanman_netmsg, inbound_blocks_chan_manager, outbound_chan_manager_connector, network, fee_estimator.clone(), chain_watchdog.clone(), tx_broadcaster.clone(), logger.clone(), keys.clone(), config, 0, bitcoind_client.clone()).await;
	join_handles.push(handle_1);
	join_handles.push(handle_2);

	// Step 8: Start node router
	//
	// The node router (`lightning::router::NetGraphMsgHandler`) is providing the following services:
	// 	- boostrap the initial network graph
	// 	- receive and validate network updates messages (BOLT-7's `node_announcement`/channel_announcement`/`channel_update`) from peers
	// 	- provide HTLC payment route (`lightning::router::get_route()`) to a given destination
	// 	- request and reply to extended queries for gossip synchronization (BOLT-7's `short_channel_ids`/`channel_range`)
	//
	// It requires a `chain::Access` reference to validate utxos part of channel announcement against the chain.

	log_sample!(logger, "Starting node router...");

	let (outbound_router_rpccmd, inbound_router_rpccmd) = mpsc::channel(1);
	let (outbound_router_rpcreply, inbound_router_rpcreply) = mpsc::channel(1);
	let (outbound_routing_msg, inbound_routing_msg): (Sender<Event>, Receiver<Event>) = mpsc::channel(1);

	let utxo_accessor = Arc::new(UtxoWatchdog::new());
	let outbound_router_connector = OutboundRouterConnector::new(outbound_router_rpcreply);
	let inbound_router_connector = InboundRouterConnector::new(inbound_router_rpccmd, inbound_router_rpcreply);
	let handle = setup_router(outbound_router_connector, inbound_router_connector, utxo_accessor.clone(), logger.clone()).await;
	join_handles.push(handle);

	// Step 9: Start a peer manager
	//
	// A lightning node must have access to the wider lightning network itself. A lightning 
	// network stack will offer the following services :
	// 	- relay messages to 
	// 	- handle peers management (e.g misbehaving, manual disconnection, ...)
	//
	// This sample node implementation relies on the default LDK networking stack (`lightning-net-tokio`).

	log_sample!(logger, "Starting peer manager...");

	let (outbound_peers_rpccmd, inbound_peers_rpccmd) = mpsc::channel(1);
	let (outbound_socket_events, inbound_socket_events): (Sender<()>, Receiver<()>) = mpsc::channel(1);

	let outbound_peer_manager_connector = Arc::new(OutboundPeerManagerConnector::new(outbound_event_peers)); //sender_routing_msg/sender_chan_msg
	let buffer_netmsg = Arc::new(BufferNetMsg::new(inbound_chanman_msg_events));
	let chan_handler = buffer_netmsg.clone();

	let mut ephemeral_data = [0; 32];
	rand::thread_rng().fill_bytes(&mut ephemeral_data);
	let (handle_1, handle_2, handle_3, peer_manager_arc) = setup_peer_manager(outbound_peer_manager_connector.clone(), outbound_chanman_netmsg, inbound_peers_rpccmd, inbound_peerman_notify, buffer_netmsg, chan_handler, outbound_peer_manager_connector.clone(), keys.get_node_secret(), ephemeral_data, logger.clone()).await;
	join_handles.push(handle_1);
	join_handles.push(handle_2);
	join_handles.push(handle_3);

	// Step 10: Start an inbound connections listener
	//
	// `lightning_net_tokio::setup_inbound` relays incoming messages to `ChannelManagerHandler`
	// and `RoutingMessageHandler` and feeds outgoing messages back to `SocketDescriptor` 
	// generated by accepting an incoming connection.

	log_sample!(logger, "Starting socket listener thread...");

	let outbound_socket_listener_connector = OutboundSocketListenerConnector::new(outbound_socket_events); // outbound_socket_events
	let handle = setup_socket_listener(peer_manager_arc, outbound_socket_listener_connector, ln_port).await;
	join_handles.push(handle);

	// Step X: Start a RPC server.
	//
	// Beyond peers messages and channel updates, a lightning node is also driven by user requests.
	//
	// This sample node implementation communicate with the LDK command-line binary (`ldk-cli`)
	// through a custom RPC protocol.

	log_sample!(logger, "Starting rpc server thread...");

	let outbound_rpc_server_connector = OutboundRPCServerConnector::new(outbound_peers_rpccmd, outbound_chanman_rpccmd, outbound_router_rpccmd);
	let handles = setup_rpc_server(outbound_rpc_server_connector, ldk_port).await;
	join_handles.push(handles);

	loop {
		let one_sec = Duration::from_millis(100);
		sleep(one_sec);
	}
}
