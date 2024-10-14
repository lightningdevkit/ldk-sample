mod args;
pub mod bitcoind_client;
mod cli;
mod convert;
mod disk;
mod hex_utils;
mod sweep;

use crate::bitcoind_client::BitcoindClient;
use crate::disk::FilesystemLogger;
use bitcoin::blockdata::transaction::Transaction;
use bitcoin::consensus::encode;
use bitcoin::io;
use bitcoin::network::Network;
use bitcoin::BlockHash;
use bitcoin_bech32::WitnessProgram;
use disk::{INBOUND_PAYMENTS_FNAME, OUTBOUND_PAYMENTS_FNAME};
use lightning::chain::{chainmonitor, ChannelMonitorUpdateStatus};
use lightning::chain::{BestBlock, Filter, Watch};
use lightning::events::bump_transaction::{BumpTransactionEventHandler, Wallet};
use lightning::events::{Event, PaymentFailureReason, PaymentPurpose};
use lightning::ln::channelmanager::{self, RecentPaymentDetails};
use lightning::ln::channelmanager::{
	ChainParameters, ChannelManagerReadArgs, PaymentId, SimpleArcChannelManager,
};
use lightning::ln::msgs::DecodeError;
use lightning::ln::peer_handler::{IgnoringMessageHandler, MessageHandler, SimpleArcPeerManager};
use lightning::ln::types::ChannelId;
use lightning::onion_message::messenger::{DefaultMessageRouter, SimpleArcOnionMessenger};
use lightning::routing::gossip;
use lightning::routing::gossip::{NodeId, P2PGossipSync};
use lightning::routing::router::DefaultRouter;
use lightning::routing::scoring::ProbabilisticScoringFeeParameters;
use lightning::sign::{EntropySource, InMemorySigner, KeysManager};
use lightning::types::payment::{PaymentHash, PaymentPreimage, PaymentSecret};
use lightning::util::config::UserConfig;
use lightning::util::persist::{
	self, KVStore, MonitorUpdatingPersister, OUTPUT_SWEEPER_PERSISTENCE_KEY,
	OUTPUT_SWEEPER_PERSISTENCE_PRIMARY_NAMESPACE, OUTPUT_SWEEPER_PERSISTENCE_SECONDARY_NAMESPACE,
};
use lightning::util::ser::{Readable, ReadableArgs, Writeable, Writer};
use lightning::util::sweep as ldk_sweep;
use lightning::{chain, impl_writeable_tlv_based, impl_writeable_tlv_based_enum};
use lightning_background_processor::{process_events_async, GossipSync};
use lightning_block_sync::init;
use lightning_block_sync::poll;
use lightning_block_sync::SpvClient;
use lightning_block_sync::UnboundedCache;
use lightning_net_tokio::SocketDescriptor;
use lightning_persister::fs_store::FilesystemStore;
use rand::{thread_rng, Rng};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::convert::TryInto;
use std::fmt;
use std::fs;
use std::fs::File;
use std::io::{BufReader, Write};
use std::net::ToSocketAddrs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

#[derive(Copy, Clone)]
pub(crate) enum HTLCStatus {
	Pending,
	Succeeded,
	Failed,
}

impl_writeable_tlv_based_enum!(HTLCStatus,
	(0, Pending) => {},
	(1, Succeeded) => {},
	(2, Failed) => {},
);

pub(crate) struct MillisatAmount(Option<u64>);

impl fmt::Display for MillisatAmount {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match self.0 {
			Some(amt) => write!(f, "{}", amt),
			None => write!(f, "unknown"),
		}
	}
}

impl Readable for MillisatAmount {
	fn read<R: io::Read>(r: &mut R) -> Result<Self, DecodeError> {
		let amt: Option<u64> = Readable::read(r)?;
		Ok(MillisatAmount(amt))
	}
}

impl Writeable for MillisatAmount {
	fn write<W: Writer>(&self, w: &mut W) -> Result<(), io::Error> {
		self.0.write(w)
	}
}

pub(crate) struct PaymentInfo {
	preimage: Option<PaymentPreimage>,
	secret: Option<PaymentSecret>,
	status: HTLCStatus,
	amt_msat: MillisatAmount,
}

impl_writeable_tlv_based!(PaymentInfo, {
	(0, preimage, required),
	(2, secret, required),
	(4, status, required),
	(6, amt_msat, required),
});

pub(crate) struct InboundPaymentInfoStorage {
	payments: HashMap<PaymentHash, PaymentInfo>,
}

impl_writeable_tlv_based!(InboundPaymentInfoStorage, {
	(0, payments, required),
});

pub(crate) struct OutboundPaymentInfoStorage {
	payments: HashMap<PaymentId, PaymentInfo>,
}

impl_writeable_tlv_based!(OutboundPaymentInfoStorage, {
	(0, payments, required),
});

type ChainMonitor = chainmonitor::ChainMonitor<
	InMemorySigner,
	Arc<dyn Filter + Send + Sync>,
	Arc<BitcoindClient>,
	Arc<BitcoindClient>,
	Arc<FilesystemLogger>,
	Arc<
		MonitorUpdatingPersister<
			Arc<FilesystemStore>,
			Arc<FilesystemLogger>,
			Arc<KeysManager>,
			Arc<KeysManager>,
			Arc<BitcoindClient>,
			Arc<BitcoindClient>,
		>,
	>,
>;

pub(crate) type GossipVerifier = lightning_block_sync::gossip::GossipVerifier<
	lightning_block_sync::gossip::TokioSpawner,
	Arc<lightning_block_sync::rpc::RpcClient>,
	Arc<FilesystemLogger>,
>;

pub(crate) type PeerManager = SimpleArcPeerManager<
	SocketDescriptor,
	ChainMonitor,
	BitcoindClient,
	BitcoindClient,
	GossipVerifier,
	FilesystemLogger,
>;

pub(crate) type ChannelManager =
	SimpleArcChannelManager<ChainMonitor, BitcoindClient, BitcoindClient, FilesystemLogger>;

pub(crate) type NetworkGraph = gossip::NetworkGraph<Arc<FilesystemLogger>>;

type OnionMessenger =
	SimpleArcOnionMessenger<ChainMonitor, BitcoindClient, BitcoindClient, FilesystemLogger>;

pub(crate) type BumpTxEventHandler = BumpTransactionEventHandler<
	Arc<BitcoindClient>,
	Arc<Wallet<Arc<BitcoindClient>, Arc<FilesystemLogger>>>,
	Arc<KeysManager>,
	Arc<FilesystemLogger>,
>;

pub(crate) type OutputSweeper = ldk_sweep::OutputSweeper<
	Arc<BitcoindClient>,
	Arc<BitcoindClient>,
	Arc<BitcoindClient>,
	Arc<dyn Filter + Send + Sync>,
	Arc<FilesystemStore>,
	Arc<FilesystemLogger>,
	Arc<KeysManager>,
>;

// Needed due to rust-lang/rust#63033.
struct OutputSweeperWrapper(Arc<OutputSweeper>);

async fn handle_ldk_events(
	channel_manager: Arc<ChannelManager>, bitcoind_client: &BitcoindClient,
	network_graph: &NetworkGraph, keys_manager: &KeysManager,
	bump_tx_event_handler: &BumpTxEventHandler, peer_manager: Arc<PeerManager>,
	inbound_payments: Arc<Mutex<InboundPaymentInfoStorage>>,
	outbound_payments: Arc<Mutex<OutboundPaymentInfoStorage>>, fs_store: Arc<FilesystemStore>,
	output_sweeper: OutputSweeperWrapper, network: Network, event: Event,
) {
	match event {
		Event::FundingGenerationReady {
			temporary_channel_id,
			counterparty_node_id,
			channel_value_satoshis,
			output_script,
			..
		} => {
			// Construct the raw transaction with one output, that is paid the amount of the
			// channel.
			let addr = WitnessProgram::from_scriptpubkey(
				&output_script.as_bytes(),
				match network {
					Network::Bitcoin => bitcoin_bech32::constants::Network::Bitcoin,
					Network::Regtest => bitcoin_bech32::constants::Network::Regtest,
					Network::Signet => bitcoin_bech32::constants::Network::Signet,
					Network::Testnet | _ => bitcoin_bech32::constants::Network::Testnet,
				},
			)
			.expect("Lightning funding tx should always be to a SegWit output")
			.to_address();
			let mut outputs = vec![HashMap::with_capacity(1)];
			outputs[0].insert(addr, channel_value_satoshis as f64 / 100_000_000.0);
			let raw_tx = bitcoind_client.create_raw_transaction(outputs).await;

			// Have your wallet put the inputs into the transaction such that the output is
			// satisfied.
			let funded_tx = bitcoind_client.fund_raw_transaction(raw_tx).await;

			// Sign the final funding transaction and give it to LDK, who will eventually broadcast it.
			let signed_tx = bitcoind_client.sign_raw_transaction_with_wallet(funded_tx.hex).await;
			assert_eq!(signed_tx.complete, true);
			let final_tx: Transaction =
				encode::deserialize(&hex_utils::to_vec(&signed_tx.hex).unwrap()).unwrap();
			// Give the funding transaction back to LDK for opening the channel.
			if channel_manager
				.funding_transaction_generated(temporary_channel_id, counterparty_node_id, final_tx)
				.is_err()
			{
				println!(
					"\nERROR: Channel went away before we could fund it. The peer disconnected or refused the channel.");
				print!("> ");
				std::io::stdout().flush().unwrap();
			}
		},
		Event::FundingTxBroadcastSafe { .. } => {
			// We don't use the manual broadcasting feature, so this event should never be seen.
		},
		Event::PaymentClaimable {
			payment_hash,
			purpose,
			amount_msat,
			receiver_node_id: _,
			via_channel_id: _,
			via_user_channel_id: _,
			claim_deadline: _,
			onion_fields: _,
			counterparty_skimmed_fee_msat: _,
		} => {
			println!(
				"\nEVENT: received payment from payment hash {} of {} millisatoshis",
				payment_hash, amount_msat,
			);
			print!("> ");
			std::io::stdout().flush().unwrap();
			let payment_preimage = match purpose {
				PaymentPurpose::Bolt11InvoicePayment { payment_preimage, .. } => payment_preimage,
				PaymentPurpose::Bolt12OfferPayment { payment_preimage, .. } => payment_preimage,
				PaymentPurpose::Bolt12RefundPayment { payment_preimage, .. } => payment_preimage,
				PaymentPurpose::SpontaneousPayment(preimage) => Some(preimage),
			};
			channel_manager.claim_funds(payment_preimage.unwrap());
		},
		Event::PaymentClaimed { payment_hash, purpose, amount_msat, .. } => {
			println!(
				"\nEVENT: claimed payment from payment hash {} of {} millisatoshis",
				payment_hash, amount_msat,
			);
			print!("> ");
			std::io::stdout().flush().unwrap();
			let (payment_preimage, payment_secret) = match purpose {
				PaymentPurpose::Bolt11InvoicePayment {
					payment_preimage, payment_secret, ..
				} => (payment_preimage, Some(payment_secret)),
				PaymentPurpose::Bolt12OfferPayment { payment_preimage, payment_secret, .. } => {
					(payment_preimage, Some(payment_secret))
				},
				PaymentPurpose::Bolt12RefundPayment {
					payment_preimage, payment_secret, ..
				} => (payment_preimage, Some(payment_secret)),
				PaymentPurpose::SpontaneousPayment(preimage) => (Some(preimage), None),
			};
			let mut inbound = inbound_payments.lock().unwrap();
			match inbound.payments.entry(payment_hash) {
				Entry::Occupied(mut e) => {
					let payment = e.get_mut();
					payment.status = HTLCStatus::Succeeded;
					payment.preimage = payment_preimage;
					payment.secret = payment_secret;
				},
				Entry::Vacant(e) => {
					e.insert(PaymentInfo {
						preimage: payment_preimage,
						secret: payment_secret,
						status: HTLCStatus::Succeeded,
						amt_msat: MillisatAmount(Some(amount_msat)),
					});
				},
			}
			fs_store.write("", "", INBOUND_PAYMENTS_FNAME, &inbound.encode()).unwrap();
		},
		Event::PaymentSent {
			payment_preimage, payment_hash, fee_paid_msat, payment_id, ..
		} => {
			let mut outbound = outbound_payments.lock().unwrap();
			for (id, payment) in outbound.payments.iter_mut() {
				if *id == payment_id.unwrap() {
					payment.preimage = Some(payment_preimage);
					payment.status = HTLCStatus::Succeeded;
					println!(
						"\nEVENT: successfully sent payment of {} millisatoshis{} from \
								 payment hash {} with preimage {}",
						payment.amt_msat,
						if let Some(fee) = fee_paid_msat {
							format!(" (fee {} msat)", fee)
						} else {
							"".to_string()
						},
						payment_hash,
						payment_preimage
					);
					print!("> ");
					std::io::stdout().flush().unwrap();
				}
			}
			fs_store.write("", "", OUTBOUND_PAYMENTS_FNAME, &outbound.encode()).unwrap();
		},
		Event::OpenChannelRequest {
			ref temporary_channel_id, ref counterparty_node_id, ..
		} => {
			let mut random_bytes = [0u8; 16];
			random_bytes.copy_from_slice(&keys_manager.get_secure_random_bytes()[..16]);
			let user_channel_id = u128::from_be_bytes(random_bytes);
			let res = channel_manager.accept_inbound_channel(
				temporary_channel_id,
				counterparty_node_id,
				user_channel_id,
			);

			if let Err(e) = res {
				print!(
					"\nEVENT: Failed to accept inbound channel ({}) from {}: {:?}",
					temporary_channel_id,
					hex_utils::hex_str(&counterparty_node_id.serialize()),
					e,
				);
			} else {
				print!(
					"\nEVENT: Accepted inbound channel ({}) from {}",
					temporary_channel_id,
					hex_utils::hex_str(&counterparty_node_id.serialize()),
				);
			}
			print!("> ");
			std::io::stdout().flush().unwrap();
		},
		Event::PaymentPathSuccessful { .. } => {},
		Event::PaymentPathFailed { .. } => {},
		Event::ProbeSuccessful { .. } => {},
		Event::ProbeFailed { .. } => {},
		Event::PaymentFailed { payment_hash, reason, payment_id, .. } => {
			if let Some(hash) = payment_hash {
				print!(
					"\nEVENT: Failed to send payment to payment ID {}, payment hash {}: {:?}",
					payment_id,
					hash,
					if let Some(r) = reason { r } else { PaymentFailureReason::RetriesExhausted }
				);
			} else {
				print!(
					"\nEVENT: Failed fetch invoice for payment ID {}: {:?}",
					payment_id,
					if let Some(r) = reason { r } else { PaymentFailureReason::RetriesExhausted }
				);
			}
			print!("> ");
			std::io::stdout().flush().unwrap();

			let mut outbound = outbound_payments.lock().unwrap();
			if outbound.payments.contains_key(&payment_id) {
				let payment = outbound.payments.get_mut(&payment_id).unwrap();
				payment.status = HTLCStatus::Failed;
			}
			fs_store.write("", "", OUTBOUND_PAYMENTS_FNAME, &outbound.encode()).unwrap();
		},
		Event::InvoiceReceived { .. } => {
			// We don't use the manual invoice payment logic, so this event should never be seen.
		},
		Event::PaymentForwarded {
			prev_channel_id,
			next_channel_id,
			total_fee_earned_msat,
			claim_from_onchain_tx,
			outbound_amount_forwarded_msat,
			skimmed_fee_msat: _,
			prev_user_channel_id: _,
			next_user_channel_id: _,
		} => {
			let read_only_network_graph = network_graph.read_only();
			let nodes = read_only_network_graph.nodes();
			let channels = channel_manager.list_channels();

			let node_str = |channel_id: &Option<ChannelId>| match channel_id {
				None => String::new(),
				Some(channel_id) => match channels.iter().find(|c| c.channel_id == *channel_id) {
					None => String::new(),
					Some(channel) => {
						match nodes.get(&NodeId::from_pubkey(&channel.counterparty.node_id)) {
							None => "private node".to_string(),
							Some(node) => match &node.announcement_info {
								None => "unnamed node".to_string(),
								Some(announcement) => {
									format!("node {}", announcement.alias())
								},
							},
						}
					},
				},
			};
			let channel_str = |channel_id: &Option<ChannelId>| {
				channel_id
					.map(|channel_id| format!(" with channel {}", channel_id))
					.unwrap_or_default()
			};
			let from_prev_str =
				format!(" from {}{}", node_str(&prev_channel_id), channel_str(&prev_channel_id));
			let to_next_str =
				format!(" to {}{}", node_str(&next_channel_id), channel_str(&next_channel_id));

			let from_onchain_str = if claim_from_onchain_tx {
				"from onchain downstream claim"
			} else {
				"from HTLC fulfill message"
			};
			let amt_args = if let Some(v) = outbound_amount_forwarded_msat {
				format!("{}", v)
			} else {
				"?".to_string()
			};
			if let Some(fee_earned) = total_fee_earned_msat {
				println!(
					"\nEVENT: Forwarded payment for {} msat{}{}, earning {} msat {}",
					amt_args, from_prev_str, to_next_str, fee_earned, from_onchain_str
				);
			} else {
				println!(
					"\nEVENT: Forwarded payment for {} msat{}{}, claiming onchain {}",
					amt_args, from_prev_str, to_next_str, from_onchain_str
				);
			}
			print!("> ");
			std::io::stdout().flush().unwrap();
		},
		Event::HTLCHandlingFailed { .. } => {},
		Event::PendingHTLCsForwardable { time_forwardable } => {
			let forwarding_channel_manager = channel_manager.clone();
			let min = time_forwardable.as_millis() as u64;
			tokio::spawn(async move {
				let millis_to_sleep = thread_rng().gen_range(min, min * 5) as u64;
				tokio::time::sleep(Duration::from_millis(millis_to_sleep)).await;
				forwarding_channel_manager.process_pending_htlc_forwards();
			});
		},
		Event::SpendableOutputs { outputs, channel_id } => {
			output_sweeper.0.track_spendable_outputs(outputs, channel_id, false, None).unwrap();
		},
		Event::ChannelPending { channel_id, counterparty_node_id, .. } => {
			println!(
				"\nEVENT: Channel {} with peer {} is pending awaiting funding lock-in!",
				channel_id,
				hex_utils::hex_str(&counterparty_node_id.serialize()),
			);
			print!("> ");
			std::io::stdout().flush().unwrap();
		},
		Event::ChannelReady {
			ref channel_id,
			user_channel_id: _,
			ref counterparty_node_id,
			channel_type: _,
		} => {
			println!(
				"\nEVENT: Channel {} with peer {} is ready to be used!",
				channel_id,
				hex_utils::hex_str(&counterparty_node_id.serialize()),
			);
			print!("> ");
			std::io::stdout().flush().unwrap();
		},
		Event::ChannelClosed {
			channel_id,
			reason,
			user_channel_id: _,
			counterparty_node_id,
			channel_capacity_sats: _,
			channel_funding_txo: _,
		} => {
			println!(
				"\nEVENT: Channel {} with counterparty {} closed due to: {:?}",
				channel_id,
				counterparty_node_id.map(|id| format!("{}", id)).unwrap_or("".to_owned()),
				reason
			);
			print!("> ");
			std::io::stdout().flush().unwrap();
		},
		Event::DiscardFunding { .. } => {
			// A "real" node should probably "lock" the UTXOs spent in funding transactions until
			// the funding transaction either confirms, or this event is generated.
		},
		Event::HTLCIntercepted { .. } => {},
		Event::OnionMessageIntercepted { .. } => {
			// We don't use the onion message interception feature, so this event should never be
			// seen.
		},
		Event::OnionMessagePeerConnected { .. } => {
			// We don't use the onion message interception feature, so we have no use for this
			// event.
		},
		Event::BumpTransaction(event) => bump_tx_event_handler.handle_event(&event),
		Event::ConnectionNeeded { node_id, addresses } => {
			tokio::spawn(async move {
				for address in addresses {
					if let Ok(sockaddrs) = address.to_socket_addrs() {
						for addr in sockaddrs {
							let pm = Arc::clone(&peer_manager);
							if cli::connect_peer_if_necessary(node_id, addr, pm).await.is_ok() {
								return;
							}
						}
					}
				}
			});
		},
	}
}

async fn start_ldk() {
	let args = match args::parse_startup_args() {
		Ok(user_args) => user_args,
		Err(()) => return,
	};

	// Initialize the LDK data directory if necessary.
	let ldk_data_dir = format!("{}/.ldk", args.ldk_storage_dir_path);
	fs::create_dir_all(ldk_data_dir.clone()).unwrap();

	// ## Setup
	// Step 1: Initialize the Logger
	let logger = Arc::new(FilesystemLogger::new(ldk_data_dir.clone()));

	// Initialize our bitcoind client.
	let bitcoind_client = match BitcoindClient::new(
		args.bitcoind_rpc_host.clone(),
		args.bitcoind_rpc_port,
		args.bitcoind_rpc_username.clone(),
		args.bitcoind_rpc_password.clone(),
		args.network,
		tokio::runtime::Handle::current(),
		Arc::clone(&logger),
	)
	.await
	{
		Ok(client) => Arc::new(client),
		Err(e) => {
			println!("Failed to connect to bitcoind client: {}", e);
			return;
		},
	};

	// Check that the bitcoind we've connected to is running the network we expect
	let bitcoind_chain = bitcoind_client.get_blockchain_info().await.chain;
	if bitcoind_chain
		!= match args.network {
			bitcoin::Network::Bitcoin => "main",
			bitcoin::Network::Regtest => "regtest",
			bitcoin::Network::Signet => "signet",
			bitcoin::Network::Testnet | _ => "test",
		} {
		println!(
			"Chain argument ({}) didn't match bitcoind chain ({})",
			args.network, bitcoind_chain
		);
		return;
	}

	// Step 2: Initialize the FeeEstimator

	// BitcoindClient implements the FeeEstimator trait, so it'll act as our fee estimator.
	let fee_estimator = bitcoind_client.clone();

	// Step 3: Initialize the BroadcasterInterface

	// BitcoindClient implements the BroadcasterInterface trait, so it'll act as our transaction
	// broadcaster.
	let broadcaster = bitcoind_client.clone();

	// Step 4: Initialize the KeysManager

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
		match File::create(keys_seed_path.clone()) {
			Ok(mut f) => {
				std::io::Write::write_all(&mut f, &key)
					.expect("Failed to write node keys seed to disk");
				f.sync_all().expect("Failed to sync node keys seed to disk");
			},
			Err(e) => {
				println!("ERROR: Unable to create keys seed file {}: {}", keys_seed_path, e);
				return;
			},
		}
		key
	};
	let cur = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap();
	let keys_manager = Arc::new(KeysManager::new(&keys_seed, cur.as_secs(), cur.subsec_nanos()));

	let bump_tx_event_handler = Arc::new(BumpTransactionEventHandler::new(
		Arc::clone(&broadcaster),
		Arc::new(Wallet::new(Arc::clone(&bitcoind_client), Arc::clone(&logger))),
		Arc::clone(&keys_manager),
		Arc::clone(&logger),
	));

	// Step 5: Initialize Persistence
	let fs_store = Arc::new(FilesystemStore::new(ldk_data_dir.clone().into()));
	let persister = Arc::new(MonitorUpdatingPersister::new(
		Arc::clone(&fs_store),
		Arc::clone(&logger),
		1000,
		Arc::clone(&keys_manager),
		Arc::clone(&keys_manager),
		Arc::clone(&bitcoind_client),
		Arc::clone(&bitcoind_client),
	));
	// Alternatively, you can use the `FilesystemStore` as a `Persist` directly, at the cost of
	// larger `ChannelMonitor` update writes (but no deletion or cleanup):
	//let persister = Arc::clone(&fs_store);

	// Step 6: Initialize the ChainMonitor
	let chain_monitor: Arc<ChainMonitor> = Arc::new(chainmonitor::ChainMonitor::new(
		None,
		Arc::clone(&broadcaster),
		Arc::clone(&logger),
		Arc::clone(&fee_estimator),
		Arc::clone(&persister),
	));

	// Step 7: Read ChannelMonitor state from disk
	let mut channelmonitors = persister.read_all_channel_monitors_with_updates().unwrap();
	// If you are using the `FilesystemStore` as a `Persist` directly, use
	// `lightning::util::persist::read_channel_monitors` like this:
	//read_channel_monitors(Arc::clone(&persister), Arc::clone(&keys_manager), Arc::clone(&keys_manager)).unwrap();

	// Step 8: Poll for the best chain tip, which may be used by the channel manager & spv client
	let polled_chain_tip = init::validate_best_block_header(bitcoind_client.as_ref())
		.await
		.expect("Failed to fetch best block header and best block");

	// Step 9: Initialize routing ProbabilisticScorer
	let network_graph_path = format!("{}/network_graph", ldk_data_dir.clone());
	let network_graph =
		Arc::new(disk::read_network(Path::new(&network_graph_path), args.network, logger.clone()));

	let scorer_path = format!("{}/scorer", ldk_data_dir.clone());
	let scorer = Arc::new(RwLock::new(disk::read_scorer(
		Path::new(&scorer_path),
		Arc::clone(&network_graph),
		Arc::clone(&logger),
	)));

	// Step 10: Create Router
	let scoring_fee_params = ProbabilisticScoringFeeParameters::default();
	let router = Arc::new(DefaultRouter::new(
		network_graph.clone(),
		logger.clone(),
		keys_manager.clone(),
		scorer.clone(),
		scoring_fee_params,
	));

	// Step 11: Initialize the ChannelManager
	let mut user_config = UserConfig::default();
	user_config.channel_handshake_limits.force_announced_channel_preference = false;
	user_config.channel_handshake_config.negotiate_anchors_zero_fee_htlc_tx = true;
	user_config.manually_accept_inbound_channels = true;
	let mut restarting_node = true;
	let (channel_manager_blockhash, channel_manager) = {
		if let Ok(f) = fs::File::open(format!("{}/manager", ldk_data_dir.clone())) {
			let mut channel_monitor_mut_references = Vec::new();
			for (_, channel_monitor) in channelmonitors.iter_mut() {
				channel_monitor_mut_references.push(channel_monitor);
			}
			let read_args = ChannelManagerReadArgs::new(
				keys_manager.clone(),
				keys_manager.clone(),
				keys_manager.clone(),
				fee_estimator.clone(),
				chain_monitor.clone(),
				broadcaster.clone(),
				router,
				logger.clone(),
				user_config,
				channel_monitor_mut_references,
			);
			<(BlockHash, ChannelManager)>::read(&mut BufReader::new(f), read_args).unwrap()
		} else {
			// We're starting a fresh node.
			restarting_node = false;

			let polled_best_block = polled_chain_tip.to_best_block();
			let polled_best_block_hash = polled_best_block.block_hash;
			let chain_params =
				ChainParameters { network: args.network, best_block: polled_best_block };
			let fresh_channel_manager = channelmanager::ChannelManager::new(
				fee_estimator.clone(),
				chain_monitor.clone(),
				broadcaster.clone(),
				router,
				logger.clone(),
				keys_manager.clone(),
				keys_manager.clone(),
				keys_manager.clone(),
				user_config,
				chain_params,
				cur.as_secs() as u32,
			);
			(polled_best_block_hash, fresh_channel_manager)
		}
	};

	// Step 12: Initialize the OutputSweeper.
	let (sweeper_best_block, output_sweeper) = match fs_store.read(
		OUTPUT_SWEEPER_PERSISTENCE_PRIMARY_NAMESPACE,
		OUTPUT_SWEEPER_PERSISTENCE_SECONDARY_NAMESPACE,
		OUTPUT_SWEEPER_PERSISTENCE_KEY,
	) {
		Err(e) if e.kind() == io::ErrorKind::NotFound => {
			let sweeper = OutputSweeper::new(
				channel_manager.current_best_block(),
				broadcaster.clone(),
				fee_estimator.clone(),
				None,
				keys_manager.clone(),
				bitcoind_client.clone(),
				fs_store.clone(),
				logger.clone(),
			);
			(channel_manager.current_best_block(), sweeper)
		},
		Ok(mut bytes) => {
			let read_args = (
				broadcaster.clone(),
				fee_estimator.clone(),
				None,
				keys_manager.clone(),
				bitcoind_client.clone(),
				fs_store.clone(),
				logger.clone(),
			);
			let mut reader = io::Cursor::new(&mut bytes);
			<(BestBlock, OutputSweeper)>::read(&mut reader, read_args)
				.expect("Failed to deserialize OutputSweeper")
		},
		Err(e) => panic!("Failed to read OutputSweeper with {}", e),
	};

	// Step 13: Sync ChannelMonitors, ChannelManager and OutputSweeper to chain tip
	let mut chain_listener_channel_monitors = Vec::new();
	let mut cache = UnboundedCache::new();
	let chain_tip = if restarting_node {
		let mut chain_listeners = vec![
			(channel_manager_blockhash, &channel_manager as &(dyn chain::Listen + Send + Sync)),
			(sweeper_best_block.block_hash, &output_sweeper as &(dyn chain::Listen + Send + Sync)),
		];

		for (blockhash, channel_monitor) in channelmonitors.drain(..) {
			let outpoint = channel_monitor.get_funding_txo().0;
			chain_listener_channel_monitors.push((
				blockhash,
				(channel_monitor, broadcaster.clone(), fee_estimator.clone(), logger.clone()),
				outpoint,
			));
		}

		for monitor_listener_info in chain_listener_channel_monitors.iter_mut() {
			chain_listeners.push((
				monitor_listener_info.0,
				&monitor_listener_info.1 as &(dyn chain::Listen + Send + Sync),
			));
		}

		init::synchronize_listeners(
			bitcoind_client.as_ref(),
			args.network,
			&mut cache,
			chain_listeners,
		)
		.await
		.unwrap()
	} else {
		polled_chain_tip
	};

	// Step 14: Give ChannelMonitors to ChainMonitor
	for item in chain_listener_channel_monitors.drain(..) {
		let channel_monitor = item.1 .0;
		let funding_outpoint = item.2;
		assert_eq!(
			chain_monitor.watch_channel(funding_outpoint, channel_monitor),
			Ok(ChannelMonitorUpdateStatus::Completed)
		);
	}

	// Step 15: Optional: Initialize the P2PGossipSync
	let gossip_sync =
		Arc::new(P2PGossipSync::new(Arc::clone(&network_graph), None, Arc::clone(&logger)));

	// Step 16: Initialize the PeerManager
	let channel_manager: Arc<ChannelManager> = Arc::new(channel_manager);
	let onion_messenger: Arc<OnionMessenger> = Arc::new(OnionMessenger::new(
		Arc::clone(&keys_manager),
		Arc::clone(&keys_manager),
		Arc::clone(&logger),
		Arc::clone(&channel_manager),
		Arc::new(DefaultMessageRouter::new(Arc::clone(&network_graph), Arc::clone(&keys_manager))),
		Arc::clone(&channel_manager),
		Arc::clone(&channel_manager),
		IgnoringMessageHandler {},
	));
	let mut ephemeral_bytes = [0; 32];
	let current_time = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
	rand::thread_rng().fill_bytes(&mut ephemeral_bytes);
	let lightning_msg_handler = MessageHandler {
		chan_handler: channel_manager.clone(),
		route_handler: gossip_sync.clone(),
		onion_message_handler: onion_messenger.clone(),
		custom_message_handler: IgnoringMessageHandler {},
	};
	let peer_manager: Arc<PeerManager> = Arc::new(PeerManager::new(
		lightning_msg_handler,
		current_time.try_into().unwrap(),
		&ephemeral_bytes,
		logger.clone(),
		Arc::clone(&keys_manager),
	));

	// Install a GossipVerifier in in the P2PGossipSync
	let utxo_lookup = GossipVerifier::new(
		Arc::clone(&bitcoind_client.bitcoind_rpc_client),
		lightning_block_sync::gossip::TokioSpawner,
		Arc::clone(&gossip_sync),
		Arc::clone(&peer_manager),
	);
	gossip_sync.add_utxo_lookup(Some(utxo_lookup));

	// ## Running LDK
	// Step 17: Initialize networking

	let peer_manager_connection_handler = peer_manager.clone();
	let listening_port = args.ldk_peer_listening_port;
	let stop_listen_connect = Arc::new(AtomicBool::new(false));
	let stop_listen = Arc::clone(&stop_listen_connect);
	tokio::spawn(async move {
		let listener = tokio::net::TcpListener::bind(format!("[::]:{}", listening_port))
			.await
			.expect("Failed to bind to listen port - is something else already listening on it?");
		loop {
			let peer_mgr = peer_manager_connection_handler.clone();
			let tcp_stream = listener.accept().await.unwrap().0;
			if stop_listen.load(Ordering::Acquire) {
				return;
			}
			tokio::spawn(async move {
				lightning_net_tokio::setup_inbound(
					peer_mgr.clone(),
					tcp_stream.into_std().unwrap(),
				)
				.await;
			});
		}
	});

	// Step 18: Connect and Disconnect Blocks
	let output_sweeper: Arc<OutputSweeper> = Arc::new(output_sweeper);
	let channel_manager_listener = channel_manager.clone();
	let chain_monitor_listener = chain_monitor.clone();
	let output_sweeper_listener = output_sweeper.clone();
	let bitcoind_block_source = bitcoind_client.clone();
	let network = args.network;
	tokio::spawn(async move {
		let chain_poller = poll::ChainPoller::new(bitcoind_block_source.as_ref(), network);
		let chain_listener =
			(chain_monitor_listener, &(channel_manager_listener, output_sweeper_listener));
		let mut spv_client = SpvClient::new(chain_tip, chain_poller, &mut cache, &chain_listener);
		loop {
			spv_client.poll_best_tip().await.unwrap();
			tokio::time::sleep(Duration::from_secs(1)).await;
		}
	});

	let inbound_payments = Arc::new(Mutex::new(disk::read_inbound_payment_info(Path::new(
		&format!("{}/{}", ldk_data_dir, INBOUND_PAYMENTS_FNAME),
	))));
	let outbound_payments = Arc::new(Mutex::new(disk::read_outbound_payment_info(Path::new(
		&format!("{}/{}", ldk_data_dir, OUTBOUND_PAYMENTS_FNAME),
	))));
	let recent_payments_payment_ids = channel_manager
		.list_recent_payments()
		.into_iter()
		.filter_map(|p| match p {
			RecentPaymentDetails::Pending { payment_id, .. } => Some(payment_id),
			RecentPaymentDetails::Fulfilled { payment_id, .. } => Some(payment_id),
			RecentPaymentDetails::Abandoned { payment_id, .. } => Some(payment_id),
			RecentPaymentDetails::AwaitingInvoice { payment_id } => Some(payment_id),
		})
		.collect::<Vec<PaymentId>>();
	for (payment_id, payment_info) in outbound_payments
		.lock()
		.unwrap()
		.payments
		.iter_mut()
		.filter(|(_, i)| matches!(i.status, HTLCStatus::Pending))
	{
		if !recent_payments_payment_ids.contains(payment_id) {
			payment_info.status = HTLCStatus::Failed;
		}
	}
	fs_store
		.write("", "", OUTBOUND_PAYMENTS_FNAME, &outbound_payments.lock().unwrap().encode())
		.unwrap();

	// Step 19: Handle LDK Events
	let channel_manager_event_listener = Arc::clone(&channel_manager);
	let bitcoind_client_event_listener = Arc::clone(&bitcoind_client);
	let network_graph_event_listener = Arc::clone(&network_graph);
	let keys_manager_event_listener = Arc::clone(&keys_manager);
	let inbound_payments_event_listener = Arc::clone(&inbound_payments);
	let outbound_payments_event_listener = Arc::clone(&outbound_payments);
	let fs_store_event_listener = Arc::clone(&fs_store);
	let peer_manager_event_listener = Arc::clone(&peer_manager);
	let output_sweeper_event_listener = Arc::clone(&output_sweeper);
	let network = args.network;
	let event_handler = move |event: Event| {
		let channel_manager_event_listener = Arc::clone(&channel_manager_event_listener);
		let bitcoind_client_event_listener = Arc::clone(&bitcoind_client_event_listener);
		let network_graph_event_listener = Arc::clone(&network_graph_event_listener);
		let keys_manager_event_listener = Arc::clone(&keys_manager_event_listener);
		let bump_tx_event_handler = Arc::clone(&bump_tx_event_handler);
		let inbound_payments_event_listener = Arc::clone(&inbound_payments_event_listener);
		let outbound_payments_event_listener = Arc::clone(&outbound_payments_event_listener);
		let fs_store_event_listener = Arc::clone(&fs_store_event_listener);
		let peer_manager_event_listener = Arc::clone(&peer_manager_event_listener);
		let output_sweeper_event_listener = Arc::clone(&output_sweeper_event_listener);
		async move {
			handle_ldk_events(
				channel_manager_event_listener,
				&bitcoind_client_event_listener,
				&network_graph_event_listener,
				&keys_manager_event_listener,
				&bump_tx_event_handler,
				peer_manager_event_listener,
				inbound_payments_event_listener,
				outbound_payments_event_listener,
				fs_store_event_listener,
				OutputSweeperWrapper(output_sweeper_event_listener),
				network,
				event,
			)
			.await;
			Ok(())
		}
	};

	// Step 20: Persist ChannelManager and NetworkGraph
	let persister = Arc::new(FilesystemStore::new(ldk_data_dir.clone().into()));

	// Step 21: Background Processing
	let (bp_exit, bp_exit_check) = tokio::sync::watch::channel(());
	let mut background_processor = tokio::spawn(process_events_async(
		Arc::clone(&persister),
		event_handler,
		chain_monitor.clone(),
		channel_manager.clone(),
		Some(onion_messenger),
		GossipSync::p2p(gossip_sync.clone()),
		peer_manager.clone(),
		logger.clone(),
		Some(scorer.clone()),
		move |t| {
			let mut bp_exit_fut_check = bp_exit_check.clone();
			Box::pin(async move {
				tokio::select! {
					_ = tokio::time::sleep(t) => false,
					_ = bp_exit_fut_check.changed() => true,
				}
			})
		},
		false,
		|| Some(SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap()),
	));

	// Regularly reconnect to channel peers.
	let connect_cm = Arc::clone(&channel_manager);
	let connect_pm = Arc::clone(&peer_manager);
	let peer_data_path = format!("{}/channel_peer_data", ldk_data_dir);
	let stop_connect = Arc::clone(&stop_listen_connect);
	tokio::spawn(async move {
		let mut interval = tokio::time::interval(Duration::from_secs(1));
		interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
		loop {
			interval.tick().await;
			match disk::read_channel_peer_data(Path::new(&peer_data_path)) {
				Ok(info) => {
					for node_id in connect_cm
						.list_channels()
						.iter()
						.map(|chan| chan.counterparty.node_id)
						.filter(|id| connect_pm.peer_by_node_id(id).is_none())
					{
						if stop_connect.load(Ordering::Acquire) {
							return;
						}
						for (pubkey, peer_addr) in info.iter() {
							if *pubkey == node_id {
								let _ = cli::do_connect_peer(
									*pubkey,
									peer_addr.clone(),
									Arc::clone(&connect_pm),
								)
								.await;
							}
						}
					}
				},
				Err(e) => println!("ERROR: errored reading channel peer info from disk: {:?}", e),
			}
		}
	});

	// Regularly broadcast our node_announcement. This is only required (or possible) if we have
	// some public channels.
	let peer_man = Arc::clone(&peer_manager);
	let chan_man = Arc::clone(&channel_manager);
	let network = args.network;
	tokio::spawn(async move {
		// First wait a minute until we have some peers and maybe have opened a channel.
		tokio::time::sleep(Duration::from_secs(60)).await;
		// Then, update our announcement once an hour to keep it fresh but avoid unnecessary churn
		// in the global gossip network.
		let mut interval = tokio::time::interval(Duration::from_secs(3600));
		loop {
			interval.tick().await;
			// Don't bother trying to announce if we don't have any public channls, though our
			// peers should drop such an announcement anyway. Note that announcement may not
			// propagate until we have a channel with 6+ confirmations.
			if chan_man.list_channels().iter().any(|chan| chan.is_announced) {
				peer_man.broadcast_node_announcement(
					[0; 3],
					args.ldk_announced_node_name,
					args.ldk_announced_listen_addr.clone(),
				);
			}
		}
	});

	tokio::spawn(sweep::migrate_deprecated_spendable_outputs(
		ldk_data_dir.clone(),
		Arc::clone(&keys_manager),
		Arc::clone(&logger),
		Arc::clone(&persister),
		Arc::clone(&output_sweeper),
	));

	// Start the CLI.
	let cli_channel_manager = Arc::clone(&channel_manager);
	let cli_chain_monitor = Arc::clone(&chain_monitor);
	let cli_persister = Arc::clone(&persister);
	let cli_logger = Arc::clone(&logger);
	let cli_peer_manager = Arc::clone(&peer_manager);
	let cli_poll = tokio::task::spawn_blocking(move || {
		cli::poll_for_user_input(
			cli_peer_manager,
			cli_channel_manager,
			cli_chain_monitor,
			keys_manager,
			network_graph,
			inbound_payments,
			outbound_payments,
			ldk_data_dir,
			network,
			cli_logger,
			cli_persister,
		)
	});

	// Exit if either CLI polling exits or the background processor exits (which shouldn't happen
	// unless we fail to write to the filesystem).
	let mut bg_res = Ok(Ok(()));
	tokio::select! {
		_ = cli_poll => {},
		bg_exit = &mut background_processor => {
			bg_res = bg_exit;
		},
	}

	// Disconnect our peers and stop accepting new connections. This ensures we don't continue
	// updating our channel data after we've stopped the background processor.
	stop_listen_connect.store(true, Ordering::Release);
	peer_manager.disconnect_all_peers();

	if let Err(e) = bg_res {
		let persist_res = persister
			.write(
				persist::CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
				persist::CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
				persist::CHANNEL_MANAGER_PERSISTENCE_KEY,
				&channel_manager.encode(),
			)
			.unwrap();
		use lightning::util::logger::Logger;
		lightning::log_error!(
			&*logger,
			"Last-ditch ChannelManager persistence result: {:?}",
			persist_res
		);
		panic!(
			"ERR: background processing stopped with result {:?}, exiting.\n\
			Last-ditch ChannelManager persistence result {:?}",
			e, persist_res
		);
	}

	// Stop the background processor.
	if !bp_exit.is_closed() {
		bp_exit.send(()).unwrap();
		background_processor.await.unwrap().unwrap();
	}
}

#[tokio::main]
pub async fn main() {
	#[cfg(not(target_os = "windows"))]
	{
		// Catch Ctrl-C with a dummy signal handler.
		unsafe {
			let mut new_action: libc::sigaction = core::mem::zeroed();
			let mut old_action: libc::sigaction = core::mem::zeroed();

			extern "C" fn dummy_handler(
				_: libc::c_int, _: *const libc::siginfo_t, _: *const libc::c_void,
			) {
			}

			new_action.sa_sigaction = dummy_handler as libc::sighandler_t;
			new_action.sa_flags = libc::SA_SIGINFO;

			libc::sigaction(
				libc::SIGINT,
				&new_action as *const libc::sigaction,
				&mut old_action as *mut libc::sigaction,
			);
		}
	}

	start_ldk().await;
}
