mod bitcoind_client;
mod cli;
mod convert;
mod disk;
mod hex_utils;

use lightning_background_processor::BackgroundProcessor;
use bitcoin::BlockHash;
use bitcoin::blockdata::constants::genesis_block;
use bitcoin::blockdata::transaction::Transaction;
use bitcoin::consensus::encode;
use bitcoin::hashes::Hash;
use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::network::constants::Network;
use bitcoin::secp256k1::Secp256k1;
use bitcoin_bech32::WitnessProgram;
use crate::bitcoind_client::BitcoindClient;
use crate::disk::FilesystemLogger;
use lightning::chain;
use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning::chain::chainmonitor::ChainMonitor;
use lightning::chain::Filter;
use lightning::chain::keysinterface::{InMemorySigner, KeysInterface, KeysManager};
use lightning::chain::transaction::OutPoint;
use lightning::chain::Watch;
use lightning::ln::channelmanager;
use lightning::ln::channelmanager::{ChainParameters, ChannelManagerReadArgs, PaymentHash, PaymentPreimage,
                                    SimpleArcChannelManager};
use lightning::ln::peer_handler::{MessageHandler, SimpleArcPeerManager};
use lightning::util::config::UserConfig;
use lightning::util::events::{Event, EventsProvider};
use lightning::util::ser::ReadableArgs;
use lightning_block_sync::UnboundedCache;
use lightning_block_sync::SpvClient;
use lightning_block_sync::init;
use lightning_block_sync::poll;
use lightning_net_tokio::SocketDescriptor;
use lightning_persister::FilesystemPersister;
use rand::{thread_rng, Rng};
use lightning::routing::network_graph::NetGraphMsgHandler;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::fs::File;
use std::io;
use std::io:: Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

#[derive(PartialEq)]
pub(crate) enum HTLCDirection {
    Inbound,
    Outbound
}

pub(crate) enum HTLCStatus {
    Pending,
    Succeeded,
    Failed,
}

pub(crate) struct SatoshiAmount(Option<u64>);

impl fmt::Display for SatoshiAmount {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            Some(amt) => write!(f, "{}", amt),
            None => write!(f, "unknown")

        }
    }
}

pub(crate) type PaymentInfoStorage = Arc<Mutex<HashMap<PaymentHash, (Option<PaymentPreimage>,
                                                                     HTLCDirection, HTLCStatus,
                                                                     SatoshiAmount)>>>;

type ArcChainMonitor = ChainMonitor<InMemorySigner, Arc<dyn Filter>, Arc<BitcoindClient>,
Arc<BitcoindClient>, Arc<FilesystemLogger>, Arc<FilesystemPersister>>;

pub(crate) type PeerManager = SimpleArcPeerManager<SocketDescriptor, ArcChainMonitor, BitcoindClient,
BitcoindClient, dyn chain::Access, FilesystemLogger>;

pub(crate) type ChannelManager = SimpleArcChannelManager<ArcChainMonitor, BitcoindClient, BitcoindClient,
FilesystemLogger>;

fn handle_ldk_events(peer_manager: Arc<PeerManager>, channel_manager: Arc<ChannelManager>,
                     chain_monitor: Arc<ArcChainMonitor>, bitcoind_client: Arc<BitcoindClient>,
                     keys_manager: Arc<KeysManager>, payment_storage: PaymentInfoStorage,
                     network: Network)
{
    let mut pending_txs: HashMap<OutPoint, Transaction> = HashMap::new();
    loop {
        peer_manager.process_events();
        let loop_channel_manager = channel_manager.clone();
        let mut events = channel_manager.get_and_clear_pending_events();
		    events.append(&mut chain_monitor.get_and_clear_pending_events());
        for event in events {
			      match event {
				        Event::FundingGenerationReady { temporary_channel_id, channel_value_satoshis,
                                                output_script, .. } => {
                    // Construct the raw transaction with one output, that is paid the amount of the
                    // channel.
					          let addr = WitnessProgram::from_scriptpubkey(&output_script[..], match network {
							          Network::Bitcoin => bitcoin_bech32::constants::Network::Bitcoin,
							          Network::Testnet => bitcoin_bech32::constants::Network::Testnet,
							          Network::Regtest => bitcoin_bech32::constants::Network::Regtest,
							          Network::Signet => panic!("Signet unsupported"),
						        }
					          ).expect("Lightning funding tx should always be to a SegWit output").to_address();
                    let mut outputs = vec![HashMap::with_capacity(1)];
                    outputs[0].insert(addr, channel_value_satoshis as f64 / 100_000_000.0);
                    let raw_tx = bitcoind_client.create_raw_transaction(outputs);

                    // Have your wallet put the inputs into the transaction such that the output is
                    // satisfied.
                    let funded_tx = bitcoind_client.fund_raw_transaction(raw_tx);
                    let change_output_position = funded_tx.changepos;
							      assert!(change_output_position == 0 || change_output_position == 1);

                    // Sign the final funding transaction and broadcast it.
                    let signed_tx = bitcoind_client.sign_raw_transaction_with_wallet(funded_tx.hex);
								    assert_eq!(signed_tx.complete, true);
                    let final_tx: Transaction = encode::deserialize(&hex_utils::to_vec(&signed_tx.hex).unwrap()).unwrap();
								    let outpoint = OutPoint {
                        txid: final_tx.txid(),
                        index: if change_output_position == 0 { 1 } else { 0 }
                    };
                    loop_channel_manager.funding_transaction_generated(&temporary_channel_id,
                                                                       outpoint);
                    pending_txs.insert(outpoint, final_tx);
				        },
				        Event::FundingBroadcastSafe { funding_txo, .. } => {
                    let funding_tx = pending_txs.remove(&funding_txo).unwrap();
                    bitcoind_client.broadcast_transaction(&funding_tx);
                    println!("\nEVENT: broadcasted funding transaction");
                    print!("> "); io::stdout().flush().unwrap();
				        },
				        Event::PaymentReceived { payment_hash, payment_secret, amt: amt_msat } => {
                    let mut payments = payment_storage.lock().unwrap();
                    if let Some((Some(preimage), _, _, _)) = payments.get(&payment_hash) {
						            assert!(loop_channel_manager.claim_funds(preimage.clone(), &payment_secret,
                                                                 amt_msat));
                        println!("\nEVENT: received payment from payment_hash {} of {} satoshis",
                                 hex_utils::hex_str(&payment_hash.0), amt_msat / 1000);
                        print!("> "); io::stdout().flush().unwrap();
                        let (_, _, ref mut status, _) = payments.get_mut(&payment_hash).unwrap();
                        *status = HTLCStatus::Succeeded;
                    } else {
                        println!("\nERROR: we received a payment but didn't know the preimage");
                        print!("> "); io::stdout().flush().unwrap();
                        loop_channel_manager.fail_htlc_backwards(&payment_hash, &payment_secret);
                        payments.insert(payment_hash, (None, HTLCDirection::Inbound,
                                                       HTLCStatus::Failed, SatoshiAmount(None)));
                    }
				        },
				        Event::PaymentSent { payment_preimage } => {
                    let hashed = PaymentHash(Sha256::hash(&payment_preimage.0).into_inner());
                    let mut payments = payment_storage.lock().unwrap();
                    for (payment_hash, (preimage_option, _, status, amt_sat)) in payments.iter_mut() {
                        if *payment_hash == hashed {
                            *preimage_option = Some(payment_preimage);
                            *status = HTLCStatus::Succeeded;
                            println!("\nNEW EVENT: successfully sent payment of {} satoshis from \
                                         payment hash {:?} with preimage {:?}", amt_sat,
                                         hex_utils::hex_str(&payment_hash.0),
                                         hex_utils::hex_str(&payment_preimage.0));
                            print!("> "); io::stdout().flush().unwrap();
                        }
                    }
				        },
				        Event::PaymentFailed { payment_hash, rejected_by_dest, .. } => {
                    print!("\nNEW EVENT: Failed to send payment to payment hash {:?}: ",
                           hex_utils::hex_str(&payment_hash.0));
                    if rejected_by_dest {
                        println!("rejected by destination node");
                    } else {
                        println!("route failed");
                    }
                    print!("> "); io::stdout().flush().unwrap();

                    let mut payments = payment_storage.lock().unwrap();
                    if payments.contains_key(&payment_hash) {
                        let (_, _, ref mut status, _) = payments.get_mut(&payment_hash).unwrap();
                        *status = HTLCStatus::Failed;
                    }
				        },
				        Event::PendingHTLCsForwardable { .. } => {
                    loop_channel_manager.process_pending_htlc_forwards();
				        },
                Event::SpendableOutputs { outputs } => {
                    let destination_address = bitcoind_client.get_new_address();
                    let output_descriptors = &outputs.iter().map(|a| a).collect::<Vec<_>>();
                    let tx_feerate = bitcoind_client.get_est_sat_per_1000_weight(ConfirmationTarget::Normal);
                    let spending_tx = keys_manager.spend_spendable_outputs(output_descriptors,
                                                                           Vec::new(),
                                                                           destination_address.script_pubkey(),
                                                                           tx_feerate, &Secp256k1::new()).unwrap();
                    bitcoind_client.broadcast_transaction(&spending_tx);
                    // XXX maybe need to rescan and blah?
                }
            }
        }
        thread::sleep(Duration::new(1, 0));
    }
}

fn main() {
    let args = match cli::parse_startup_args() {
        Ok(user_args) => user_args,
        Err(()) => return
    };

    // Initialize the LDK data directory if necessary.
    let ldk_data_dir = format!("{}/.ldk", args.ldk_storage_dir_path);
    fs::create_dir_all(ldk_data_dir.clone()).unwrap();

    // Initialize our bitcoind client.
    let bitcoind_client = match BitcoindClient::new(args.bitcoind_rpc_host.clone(),
                                         args.bitcoind_rpc_port, args.bitcoind_rpc_username.clone(),
                                         args.bitcoind_rpc_password.clone()) {
        Ok(client) => Arc::new(client),
        Err(e) => {
            println!("Failed to connect to bitcoind client: {}", e);
            return
        }
    };
    let mut bitcoind_rpc_client = bitcoind_client.get_new_rpc_client().unwrap();

    // ## Setup
    // Step 1: Initialize the FeeEstimator

    // BitcoindClient implements the FeeEstimator trait, so it'll act as our fee estimator.
    let fee_estimator = bitcoind_client.clone();

    // Step 2: Initialize the Logger
    let logger = Arc::new(FilesystemLogger::new(ldk_data_dir.clone()));

    // Step 3: Initialize the BroadcasterInterface

    // BitcoindClient implements the BroadcasterInterface trait, so it'll act as our transaction
    // broadcaster.
    let broadcaster = bitcoind_client.clone();

    // Step 4: Initialize Persist
    let persister = Arc::new(FilesystemPersister::new(ldk_data_dir.clone()));

    // Step 5: Initialize the ChainMonitor
    let chain_monitor: Arc<ArcChainMonitor> = Arc::new(ChainMonitor::new(None, broadcaster.clone(),
                                                           logger.clone(), fee_estimator.clone(),
                                                           persister.clone()));

    // Step 6: Initialize the KeysManager

    // The key seed that we use to derive the node privkey (that corresponds to the node pubkey) and
    // other secret key material.
    let keys_seed_path = format!("{}/keys_seed", ldk_data_dir.clone());
	  let keys_seed = if let Ok(seed) = fs::read(keys_seed_path.clone()) {
		    assert_eq!(seed.len(), 32);
		    let mut key = [0; 32];
		    key.copy_from_slice(&seed);
		    key
	  } else {
		    let mut key = [0; 32];
		    thread_rng().fill_bytes(&mut key);
		    let mut f = File::create(keys_seed_path).unwrap();
		    f.write_all(&key).expect("Failed to write node keys seed to disk");
		    f.sync_all().expect("Failed to sync node keys seed to disk");
		    key
	  };
	  let cur = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap();
    let keys_manager = Arc::new(KeysManager::new(&keys_seed, cur.as_secs(), cur.subsec_nanos()));

    // Step 7: Read ChannelMonitor state from disk
    let monitors_path = format!("{}/monitors", ldk_data_dir.clone());
    let mut outpoint_to_channelmonitor = disk::read_channelmonitors(monitors_path.to_string(),
                                                                    keys_manager.clone()).unwrap();

    // Step 9: Initialize the ChannelManager
    let user_config = UserConfig::default();
    let runtime = Runtime::new().unwrap();
    let mut restarting_node = true;
    let (channel_manager_blockhash, mut channel_manager) = {
        if let Ok(mut f) = fs::File::open(format!("{}/manager", ldk_data_dir.clone())) {
            let mut channel_monitor_mut_references = Vec::new();
            for (_, channel_monitor) in outpoint_to_channelmonitor.iter_mut() {
                channel_monitor_mut_references.push(&mut channel_monitor.1);
            }
            let read_args = ChannelManagerReadArgs::new(keys_manager.clone(), fee_estimator.clone(),
                                                        chain_monitor.clone(), broadcaster.clone(),
                                                        logger.clone(), user_config,
                                                        channel_monitor_mut_references);
            <(BlockHash, ChannelManager)>::read(&mut f, read_args).unwrap()
        } else { // We're starting a fresh node.
            restarting_node = false;
            let getinfo_resp = bitcoind_client.get_blockchain_info();
            let chain_params = ChainParameters {
                network: args.network,
                latest_hash: getinfo_resp.latest_blockhash,
                latest_height: getinfo_resp.latest_height,
            };
            let fresh_channel_manager = channelmanager::ChannelManager::new(fee_estimator.clone(),
                                                                            chain_monitor.clone(),
                                                                            broadcaster.clone(),
                                                                            logger.clone(),
                                                                            keys_manager.clone(),
                                                                            user_config, chain_params);
            (getinfo_resp.latest_blockhash, fresh_channel_manager)
        }
    };

    // Step 10: Sync ChannelMonitors and ChannelManager to chain tip
    let mut chain_listener_channel_monitors = Vec::new();
    let mut cache = UnboundedCache::new();
    let mut chain_tip: Option<poll::ValidatedBlockHeader> = None;
    if restarting_node {
        let mut chain_listeners = vec![
            (channel_manager_blockhash, &mut channel_manager as &mut dyn chain::Listen)];

        for (outpoint, blockhash_and_monitor) in outpoint_to_channelmonitor.drain() {
            let blockhash = blockhash_and_monitor.0;
            let channel_monitor = blockhash_and_monitor.1;
            chain_listener_channel_monitors.push((blockhash, (channel_monitor,
                                                              broadcaster.clone(), fee_estimator.clone(),
                                                              logger.clone()), outpoint));
        }

        for monitor_listener_info in chain_listener_channel_monitors.iter_mut() {
            chain_listeners.push((monitor_listener_info.0,
                                  &mut monitor_listener_info.1 as &mut dyn chain::Listen));
        }
        chain_tip = Some(runtime.block_on(init::synchronize_listeners(&mut bitcoind_rpc_client, args.network,
                                                                      &mut cache, chain_listeners)).unwrap());
    }

    // Step 11: Give ChannelMonitors to ChainMonitor
    for item in chain_listener_channel_monitors.drain(..) {
        let channel_monitor = item.1.0;
        let funding_outpoint = item.2;
        chain_monitor.watch_channel(funding_outpoint, channel_monitor).unwrap();
    }

    // Step 13: Optional: Initialize the NetGraphMsgHandler
    // XXX persist routing data
    let genesis = genesis_block(args.network).header.block_hash();
    let router = Arc::new(NetGraphMsgHandler::new(genesis, None::<Arc<dyn chain::Access>>, logger.clone()));

    // Step 14: Initialize the PeerManager
    let channel_manager: Arc<ChannelManager> = Arc::new(channel_manager);
	  let mut ephemeral_bytes = [0; 32];
	  rand::thread_rng().fill_bytes(&mut ephemeral_bytes);
    let lightning_msg_handler = MessageHandler { chan_handler: channel_manager.clone(),
                                                 route_handler: router.clone() };
    let peer_manager: Arc<PeerManager> = Arc::new(PeerManager::new(lightning_msg_handler,
                                                        keys_manager.get_node_secret(),
                                                        &ephemeral_bytes, logger.clone()));

    // ## Running LDK
    // Step 16: Initialize Peer Connection Handling

    // We poll for events in handle_ldk_events(..) rather than waiting for them over the
    // mpsc::channel, so we can leave the event receiver as unused.
    let (event_ntfn_sender, mut _event_ntfn_receiver) = mpsc::channel(2);
    let peer_manager_connection_handler = peer_manager.clone();
    let event_notifier = event_ntfn_sender.clone();
    let listening_port = args.ldk_peer_listening_port;
    runtime.spawn(async move {
	      let listener = std::net::TcpListener::bind(format!("0.0.0.0:{}", listening_port)).unwrap();
        loop {
            let tcp_stream = listener.accept().unwrap().0;
            lightning_net_tokio::setup_inbound(peer_manager_connection_handler.clone(),
                                               event_notifier.clone(), tcp_stream).await;
        }
    });

    // Step 17: Connect and Disconnect Blocks
    if chain_tip.is_none() {
        chain_tip = Some(runtime.block_on(init::validate_best_block_header(&mut bitcoind_rpc_client)).unwrap());
    }
    let channel_manager_listener = channel_manager.clone();
    let chain_monitor_listener = chain_monitor.clone();
    let network = args.network;
    runtime.spawn(async move {
        let chain_poller = poll::ChainPoller::new(&mut bitcoind_rpc_client, network);
        let chain_listener = (chain_monitor_listener, channel_manager_listener);
        let mut spv_client = SpvClient::new(chain_tip.unwrap(), chain_poller, &mut cache,
                                            &chain_listener);
        loop {
            spv_client.poll_best_tip().await.unwrap();
            thread::sleep(Duration::new(1, 0));
        }
    });

    // Step 17 & 18: Initialize ChannelManager persistence & Once Per Minute: ChannelManager's
    // timer_chan_freshness_every_min() and PeerManager's timer_tick_occurred
    let runtime_handle = runtime.handle();
    let data_dir = ldk_data_dir.clone();
    let persist_channel_manager_callback = move |node: &ChannelManager| {
        FilesystemPersister::persist_manager(data_dir.clone(), &*node)
    };
    BackgroundProcessor::start(persist_channel_manager_callback, channel_manager.clone(),
    logger.clone());

    let peer_manager_processor = peer_manager.clone();
    runtime_handle.spawn(async move {
        loop {
            peer_manager_processor.timer_tick_occurred();
            thread::sleep(Duration::new(60, 0));
        }
    });

    // Step 15: Initialize LDK Event Handling
    let peer_manager_event_listener = peer_manager.clone();
    let channel_manager_event_listener = channel_manager.clone();
    let chain_monitor_event_listener = chain_monitor.clone();
    let keys_manager_listener = keys_manager.clone();
    let payment_info: PaymentInfoStorage = Arc::new(Mutex::new(HashMap::new()));
    let payment_info_for_events = payment_info.clone();
    let handle = runtime_handle.clone();
    let network = args.network;
    thread::spawn(move || {
        handle_ldk_events(peer_manager_event_listener, channel_manager_event_listener,
                          chain_monitor_event_listener, bitcoind_client.clone(),
                          keys_manager_listener, payment_info_for_events, network);
    });

    // Reconnect to channel peers if possible.
    let peer_data_path = format!("{}/channel_peer_data", ldk_data_dir.clone());
    match disk::read_channel_peer_data(Path::new(&peer_data_path)) {
        Ok(mut info) => {
            for (pubkey, peer_addr) in info.drain() {
                let _ = cli::connect_peer_if_necessary(pubkey, peer_addr, peer_manager.clone(),
                                                       event_ntfn_sender.clone(), handle.clone());
            }
        },
        Err(e) => println!("ERROR: errored reading channel peer info from disk: {:?}", e),
    }

    // Start the CLI.
    cli::poll_for_user_input(peer_manager.clone(), channel_manager.clone(), router.clone(),
                             payment_info, keys_manager.get_node_secret(), event_ntfn_sender,
                             ldk_data_dir.clone(), logger.clone(), handle, args.network);
}
