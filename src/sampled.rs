use ldk::utils::{hex_to_vec, RpcClient};

use lightning::chain;
use lightning::chain::{Access, AccessError, Filter, Watch};
use lightning::chain::{chaininterface, channelmonitor};
use lightning::chain::chaininterface::{BroadcasterInterface, FeeEstimator};
use lightning::chain::chainmonitor::ChainMonitor;
use lightning::chain::channelmonitor::{ChannelMonitor, ChannelMonitorUpdate, ChannelMonitorUpdateErr, MonitorEvent};
use lightning::chain::keysinterface::InMemoryChannelKeys;
use lightning::chain::transaction::OutPoint;
use lightning::chain::keysinterface::KeysInterface;
use lightning::ln::peer_handler;
use lightning::ln::channelmanager::{ChannelManager, PaymentHash};
use lightning::ln::features::{InitFeatures, ChannelFeatures, NodeFeatures};
use lightning::ln::msgs::*;
use lightning::routing::network_graph::NetGraphMsgHandler;
use lightning::routing::router::{Route, RouteHop};
use lightning::util::config::UserConfig;
use lightning::util::events::{MessageSendEvent, MessageSendEventsProvider, EventsProvider, Event};
use lightning::util::logger::Logger;

use lightning_net_tokio::*;

use lightning_block_sync::*;
use lightning_block_sync::http_clients::RPCClient;
use lightning_block_sync::http_endpoint::HttpEndpoint;

use bitcoin::{BlockHash, TxOut};
use bitcoin::blockdata;
use bitcoin::blockdata::block::{Block, BlockHeader};
use bitcoin::blockdata::script::Script;
use bitcoin::blockdata::transaction::Transaction;
use bitcoin::consensus::encode;
use bitcoin::hash_types::Txid;
use bitcoin::hashes::Hash;
use bitcoin::hashes::sha256::Hash as Sha256Hash;
use bitcoin::network::constants;
use bitcoin::secp256k1::key::{PublicKey, SecretKey};

use std::cmp;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::ops::Deref;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::sleep;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::sync::mpsc::{Sender, Receiver};
use tokio::task::JoinHandle;

pub(crate) struct InboundEventConnector {
	peers: Receiver<()>,
	chan_manager: Receiver<()>,
}

impl InboundEventConnector {
	pub(crate) fn new(peers: Receiver<()>, chan_manager: Receiver<()>) -> Self {
		InboundEventConnector {
			peers,
			chan_manager,
		}
	}
}

pub(super) async fn setup_event_handler(mut inbound: InboundEventConnector) -> JoinHandle<()> {

	let join_handle: JoinHandle<()> = tokio::spawn(async move {
		loop {
			if let Some(_) = inbound.peers.recv().await {
				println!("Peer Manager: connect !");
			}
		}
	});
	join_handle
}

pub(crate) struct WrapperBlock {
	header: BlockHeader,
	txdata: Vec<Transaction>,
	height: u32,
}

pub(crate) struct ChainWatchdog {}

impl ChainWatchdog {
	pub(crate) fn new() -> Self {
		ChainWatchdog {}
	}
}

impl Watch for ChainWatchdog {
	type Keys = InMemoryChannelKeys;
	fn watch_channel(&self, funding_txo: OutPoint, monitor: ChannelMonitor<Self::Keys>) -> Result<(), ChannelMonitorUpdateErr> {
		Ok(())
	}
	fn update_channel(&self, funding_txo: OutPoint, update: ChannelMonitorUpdate) -> Result<(), ChannelMonitorUpdateErr> {
		Ok(())
	}
	fn release_pending_monitor_events(&self) -> Vec<MonitorEvent> {
		vec![]
	}
}

pub(crate) struct InboundChanManagerConnector {
	rpccmd: Receiver<Vec<u8>>,
	netmsg: Receiver<WrapperMsg>,
}

impl InboundChanManagerConnector {
	pub(crate) fn new(rpccmd: Receiver<Vec<u8>>, netmsg: Receiver<WrapperMsg>) -> Self {
		InboundChanManagerConnector {
			rpccmd,
			netmsg,
		}
	}
}

#[derive(Clone)]
pub(crate) struct OutboundChanManagerConnector {
	chanman_msg_events: Sender<Vec<MessageSendEvent>>,
	peerman_notify: Sender<()>,
}

impl OutboundChanManagerConnector {
	pub(crate) fn new(chanman_msg_events: Sender<Vec<MessageSendEvent>>, peerman_notify: Sender<()>) -> Self {
		OutboundChanManagerConnector {
			chanman_msg_events,
			peerman_notify,
		}
	}
}

macro_rules! flush_msg_events {
	($outbound: expr, $manager: expr) => {
		let msg_events = $manager.get_and_clear_pending_msg_events();
		if let Ok(_) = $outbound.chanman_msg_events.send(msg_events).await {}
		if let Ok(_) = $outbound.peerman_notify.send(()).await {}
	}
}

macro_rules! to_bech_network {
	($network: expr) => {
		match $network {
			constants::Network::Bitcoin => { bitcoin_bech32::constants::Network::Bitcoin },
			constants::Network::Testnet => { bitcoin_bech32::constants::Network::Testnet },
			constants::Network::Regtest => { bitcoin_bech32::constants::Network::Regtest }
		};
	}
}

pub(super) async fn setup_channel_manager<F, M, T, L, K>(mut rpccmd: Receiver<Vec<u8>>, mut netmsg: Receiver<WrapperMsg>, mut blocks: Receiver<WrapperBlock>, mut outbound: OutboundChanManagerConnector, network: constants::Network, fee_est: Arc<F>, chain_monitor: Arc<M>, tx_broadcaster: Arc<T>, logger: Arc<L>, keys_manager: Arc<K>, config: UserConfig, current_blockchain_height: usize, bitcoind_client: Arc<RpcClient>) -> (JoinHandle<()>, JoinHandle<()>)
	where F: FeeEstimator + 'static,
	      M: chain::Watch<Keys=InMemoryChannelKeys> + 'static,
	      T: BroadcasterInterface + 'static,
	      L: Logger + 'static,
	      K: KeysInterface<ChanKeySigner=InMemoryChannelKeys> + 'static,
{
	let channel_manager = ChannelManager::new(network, fee_est, chain_monitor, tx_broadcaster, logger, keys_manager, config.clone(), current_blockchain_height);
	let channel_manager_rpc = Arc::new(channel_manager);
	let channel_manager_net = channel_manager_rpc.clone();
	let mut outbound_net = outbound.clone();
	let txn_to_broadcast = Mutex::new(HashMap::new());
	let hash_to_htlc_key = Mutex::new(HashMap::new());
	let join_handle_rpc: JoinHandle<()> = tokio::spawn(async move {
		loop {
			if let Some(buffer) = rpccmd.recv().await {
				let v: serde_json::Value = serde_json::from_slice(&buffer[..]).unwrap();
				let v_obj = v.as_object().unwrap();
				match v_obj.get("cmd").unwrap().as_str().unwrap() {
					"open" => {
						let pubkey = PublicKey::from_str(v_obj.get("pubkey").unwrap().as_str().unwrap()).unwrap();
						let funding = u64::from_str_radix(v_obj.get("funding_satoshis").unwrap().as_str().unwrap(), 10).unwrap();
						let push_msat = u64::from_str_radix(v_obj.get("push_msat").unwrap().as_str().unwrap(), 10).unwrap();
						match channel_manager_rpc.create_channel(pubkey, funding, push_msat, 0, Some(config.clone())) {
							Ok(_) => println!("Channel created, sending open_channel!"),
							Err(e) => println!("Failed to open channel : {:?}", e),
						}
						flush_msg_events!(outbound, channel_manager_rpc);
					},
					"close" => {
						//TODO: call `channel_manager_rpc.close_channel(pubkey, channel_id)
					},
					"force-close" => {
						//TODO: call channel_manager_rpc.force_close_channel(pubkey, channel_id)
					},
					"send" => {
						let mut payment_preimage = [1; 32];
						let hash = Sha256Hash::hash(&payment_preimage);
						let pubkey = PublicKey::from_str(v_obj.get("pubkey").unwrap().as_str().unwrap()).unwrap();
						let short_channel_id = u64::from_str_radix(v_obj.get("short_id").unwrap().as_str().unwrap(), 10).unwrap();
						let amt = u64::from_str_radix(v_obj.get("amt").unwrap().as_str().unwrap(), 10).unwrap();
						let hop = RouteHop {
							pubkey,
							node_features: NodeFeatures::empty(),
							short_channel_id,
							channel_features: ChannelFeatures::empty(),
							fee_msat: amt,
							cltv_expiry_delta: 10
						};
						let route = Route {
							paths: vec![vec![hop]],	
						};
						channel_manager_rpc.send_payment(&route, PaymentHash(hash.into_inner()), &None).unwrap();
						flush_msg_events!(outbound, channel_manager_rpc);
					},
					"invoice" => {
						let amount = u64::from_str_radix(v_obj.get("amt").unwrap().as_str().unwrap(), 10).unwrap();
						let payment_preimage = [1; 32];
						//thread_rng().fill_bytes(&mut payment_preimage);
						let payment_hash = Sha256Hash::hash(&payment_preimage);
						if let Ok(mut hash_to_htlc_key) = hash_to_htlc_key.lock() {
							hash_to_htlc_key.insert(payment_hash, (amount, payment_preimage));
						}
						println!("Invoice hash {} amount {}", payment_hash, amount);
					},
					_ => {},
				}
			}
		}
	});
	let join_handle_net: JoinHandle<()> = tokio::spawn(async move {
		loop {
			if let Ok(wrap_msg) = netmsg.try_recv() {
				match wrap_msg.msg {
					Msg::Open(open) => {
						channel_manager_net.handle_open_channel(&wrap_msg.node_id, wrap_msg.features.unwrap(), &open);
						println!("Gotcha OpenChannel");
						flush_msg_events!(outbound_net, channel_manager_net);
					},
					Msg::Accept(accept) => {
						channel_manager_net.handle_accept_channel(&wrap_msg.node_id, wrap_msg.features.unwrap(), &accept);
						let events = channel_manager_net.get_and_clear_pending_events();
						match &events[0] {
							Event::FundingGenerationReady { temporary_channel_id, channel_value_satoshis, output_script, .. } => {
								println!("Funding Generation Ready {}", channel_value_satoshis);
								let addr = bitcoin_bech32::WitnessProgram::from_scriptpubkey(&output_script[..], to_bech_network!(network)).expect("LN funding tx should always be to a Segwit output").to_address();
								let outputs = format!("{{\"{}\":{}}}", addr, *channel_value_satoshis as f64 / 1_000_000_00.0).to_string();
								if let Ok(tx_hex) = bitcoind_client.make_rpc_call("createrawtransaction", &["[]", &outputs], false).await {
									let rawtx = format!("\"{}\"", tx_hex.as_str().unwrap()).to_string();
									let feerate = "{\"fee_rate\": 1}";
									if let Ok(funded_tx) = bitcoind_client.make_rpc_call("fundrawtransaction", &[&rawtx, &feerate], false).await {
										let changepos = funded_tx["changepos"].as_i64().unwrap();
										assert!(changepos == 0 || changepos == 1);
										let funded_tx = format!("\"{}\"", funded_tx["hex"].as_str().unwrap()).to_string();
										if let Ok(signed_tx) = bitcoind_client.make_rpc_call("signrawtransactionwithwallet", &[&funded_tx], false).await {
											assert_eq!(signed_tx["complete"].as_bool().unwrap(), true);
											let tx: blockdata::transaction::Transaction = encode::deserialize(&hex_to_vec(&signed_tx["hex"].as_str().unwrap()).unwrap()).unwrap();
											let outpoint = chain::transaction::OutPoint {
												txid: tx.txid(),
												index: if changepos == 0 { 1 } else { 0 },
											};
											channel_manager_net.funding_transaction_generated(&temporary_channel_id, outpoint);
											if let Ok(mut txn_to_broadcast) = txn_to_broadcast.lock() {
												txn_to_broadcast.insert(outpoint, tx);
											}
											flush_msg_events!(outbound_net, channel_manager_net);
										}
									}
								}
							},
							_ => panic!("Event should have been polled earlier!"),
						}
					},
					Msg::Created(created) => {
						channel_manager_net.handle_funding_created(&wrap_msg.node_id, &created);
						flush_msg_events!(outbound_net, channel_manager_net);
					},
					Msg::Signed(signed) => {
						channel_manager_net.handle_funding_signed(&wrap_msg.node_id, &signed);
						let events = channel_manager_net.get_and_clear_pending_events();
						match &events[0] {
							Event::FundingBroadcastSafe { funding_txo, user_channel_id: _ } => {
								let mut tx = Transaction {
									version: 2,
									lock_time: 0,
									input: Vec::new(),
									output: Vec::new(),
								};
								if let Ok(mut txn_to_broadcast) = txn_to_broadcast.lock() {
									let removed_tx = txn_to_broadcast.remove(&funding_txo).unwrap();
									tx.input = removed_tx.input.clone();
									tx.output = removed_tx.output.clone();
								}
								let tx_ser = "\"".to_string() + &encode::serialize_hex(&tx) + "\"";
								if let Ok(_) = bitcoind_client.make_rpc_call("sendrawtransaction", &[&tx_ser], false).await {
									println!("Transaction broadcast !");
								}
							},
							_ => panic!("Event should have been polled earlier!"),
						}
					},
					Msg::Locked(locked) => {
						channel_manager_net.handle_funding_locked(&wrap_msg.node_id, &locked);
					},
					Msg::AddHTLC(add) => {
						channel_manager_net.handle_update_add_htlc(&wrap_msg.node_id, &add);
					},
					Msg::Commitment(commitment) => {
						channel_manager_net.handle_commitment_signed(&wrap_msg.node_id, &commitment);
						flush_msg_events!(outbound_net, channel_manager_net);
					},
					Msg::RAA(raa) => {
						channel_manager_net.handle_revoke_and_ack(&wrap_msg.node_id, &raa);
						flush_msg_events!(outbound_net, channel_manager_net);
					},
					_ => panic!("Not coded yet!"),
				}
			}
			if let Ok(wrap_block) = blocks.try_recv() {
				let mut txn_data = vec![];
				for (idx, tx) in wrap_block.txdata.iter().enumerate() {
					txn_data.push((idx, tx));
				}
				channel_manager_net.block_connected(&wrap_block.header, &txn_data, wrap_block.height);
				flush_msg_events!(outbound_net, channel_manager_net);
			}
			let one_centisecond = Duration::from_millis(100);
			sleep(one_centisecond);
		}
	});
	(join_handle_rpc, join_handle_net)
}

pub(crate) struct ChainSource {}

impl ChainSource {
	pub(crate) fn new() -> Self {
		ChainSource {}
	}
}

impl Filter for ChainSource {
	fn register_tx(&self, txid: &Txid, script_pubkey: &Script) {}
	fn register_output(&self, outpoint: &OutPoint, script_pubkey: &Script) {}
}

pub(super) async fn setup_chain_processing<C: Deref + Send + 'static, T: Deref + Send + 'static, F: Deref + Send + 'static, L: Deref + Send + 'static, P: Deref + Send + 'static>(chain_source: C, broadcaster: T, feeest: F, logger: L, persister: P) -> JoinHandle<()>
	where C::Target: chain::Filter,
	      T::Target: BroadcasterInterface,
	      F::Target: FeeEstimator,
	      L::Target: Logger,
	      P::Target: channelmonitor::Persist<InMemoryChannelKeys>,
{
	let join_handle: JoinHandle<()> = tokio::spawn(async move {
		let _chain_processing = ChainMonitor::new(Some(chain_source), broadcaster, logger, feeest, persister);
		//TODO: actually verify what happens on chain :)
	});
	join_handle
}

pub(crate) struct UtxoWatchdog {}

impl UtxoWatchdog {
	pub(crate) fn new() -> Self {
		UtxoWatchdog {}
	}
}

impl Access for UtxoWatchdog {
	fn get_utxo(&self, genesis_hash: &BlockHash, short_channel_id: u64) -> Result<TxOut, AccessError> {
		panic!();
	}
}

pub(crate) struct OutboundRouterConnector {
	rpcreply: Sender<Vec<u8>>,
}

impl OutboundRouterConnector {
	pub(crate) fn new(rpcreply: Sender<Vec<u8>>) -> Self {
		OutboundRouterConnector {
			rpcreply
		}
	}
}

pub(crate) struct InboundRouterConnector {
	rpccmd: Receiver<Vec<u8>>,
	netmsg: Receiver<Vec<u8>>,
}

impl InboundRouterConnector {
	pub(crate) fn new(rpccmd: Receiver<Vec<u8>>, netmsg: Receiver<Vec<u8>>) -> Self {
		InboundRouterConnector {
			rpccmd,
			netmsg,
		}
	}
}

pub(super) async fn setup_router<F: Deref + Send + 'static, L: Deref + Send + 'static>(_outbound: OutboundRouterConnector, mut inbound: InboundRouterConnector, utxo_accessor: F, logger: L) -> JoinHandle<()>
	where F::Target: Access,
	      L::Target: Logger,
{
	let join_handle: JoinHandle<()> = tokio::spawn(async move {
		let router = NetGraphMsgHandler::new(Some(utxo_accessor), logger);
		loop {
			if let Some(buffer) = inbound.rpccmd.recv().await {
				let v: serde_json::Value = serde_json::from_slice(&buffer[..]).unwrap();
				let v_obj = v.as_object().unwrap();
				match v_obj.get("cmd").unwrap().as_str().unwrap() {
					"list" => {
						match v_obj.get("element").unwrap().as_str().unwrap() {
							"peers" => {
								if let Ok(_) = router.network_graph.read() {
									//TODO: serialize in an acceptable format and answer back the client
								}
							},
							_ => {},
						}
					},
					_ => {},
				}
			}
		}
	});
	join_handle
}

enum Msg {
	Open(OpenChannel),
	Accept(AcceptChannel),
	Created(FundingCreated),
	Signed(FundingSigned),
	Locked(FundingLocked),
	AddHTLC(UpdateAddHTLC),
	FulfillHTLC(UpdateFulfillHTLC),
	Commitment(CommitmentSigned),
	RAA(RevokeAndACK),
}

pub(crate) struct WrapperMsg {
	node_id: PublicKey,
	features: Option<InitFeatures>,
	msg: Msg,
}

pub(crate) struct OutboundPeerManagerConnector {
	event_handler: Sender<()>,
}

impl OutboundPeerManagerConnector {
	pub(crate) fn new(event_handler: Sender<()>) -> Self {
		OutboundPeerManagerConnector {
			event_handler,
		}
	}
}

pub(crate) struct BufferNetMsg {
	chanman_msg_events: Mutex<Receiver<Vec<MessageSendEvent>>>,
	buffer_netmsg: Mutex<Vec<WrapperMsg>>,
}

impl BufferNetMsg {
	pub(crate) fn new(chanman_msg_events: Receiver<Vec<MessageSendEvent>>) -> BufferNetMsg {
		BufferNetMsg {
			chanman_msg_events: Mutex::new(chanman_msg_events),
			buffer_netmsg: Mutex::new(vec![]),
		}
	}
	pub(crate) fn get_netmsg(&self) -> Vec<WrapperMsg> {
		if let Ok(mut buffer_netmsg) = self.buffer_netmsg.lock() {
			return buffer_netmsg.drain(..).collect();
		}
		vec![]
	}
}

impl ChannelMessageHandler for BufferNetMsg {
	fn handle_open_channel(&self, their_node_id: &PublicKey, their_features: InitFeatures, msg: &OpenChannel) {
		if let Ok(mut buffer_netmsg) = self.buffer_netmsg.lock() {
			buffer_netmsg.push(WrapperMsg { node_id: their_node_id.clone(), features: Some(their_features), msg: Msg::Open(msg.clone()) });
		}
	}
	fn handle_accept_channel(&self, their_node_id: &PublicKey, their_features: InitFeatures, msg: &AcceptChannel) {
		if let Ok(mut buffer_netmsg) = self.buffer_netmsg.lock() {
			buffer_netmsg.push(WrapperMsg { node_id: their_node_id.clone(), features: Some(their_features), msg: Msg::Accept(msg.clone()) });
		}
	}
	fn handle_funding_created(&self, their_node_id: &PublicKey, msg: &FundingCreated) {
		if let Ok(mut buffer_netmsg) = self.buffer_netmsg.lock() {
			buffer_netmsg.push(WrapperMsg { node_id: their_node_id.clone(), features: None, msg: Msg::Created(msg.clone()) });
		}
	}
	fn handle_funding_signed(&self, their_node_id: &PublicKey, msg: &FundingSigned) {
		if let Ok(mut buffer_netmsg) = self.buffer_netmsg.lock() {
			buffer_netmsg.push(WrapperMsg { node_id: their_node_id.clone(), features: None, msg: Msg::Signed(msg.clone()) });
		}
	}
	fn handle_funding_locked(&self, their_node_id: &PublicKey, msg: &FundingLocked) {
		if let Ok(mut buffer_netmsg) = self.buffer_netmsg.lock() {
			buffer_netmsg.push(WrapperMsg { node_id: their_node_id.clone(), features: None, msg: Msg::Locked(msg.clone()) });
		}
	}
	fn handle_shutdown(&self, their_node_id: &PublicKey, msg: &Shutdown) {}
	fn handle_closing_signed(&self, their_node_id: &PublicKey, msg: &ClosingSigned) {}
	fn handle_update_add_htlc(&self, their_node_id: &PublicKey, msg: &UpdateAddHTLC) {
		if let Ok(mut buffer_netmsg) = self.buffer_netmsg.lock() {
			buffer_netmsg.push(WrapperMsg { node_id: their_node_id.clone(), features: None, msg: Msg::AddHTLC(msg.clone()) });
		}
	}
	fn handle_update_fulfill_htlc(&self, their_node_id: &PublicKey, msg: &UpdateFulfillHTLC) {
		if let Ok(mut buffer_netmsg) = self.buffer_netmsg.lock() {
			buffer_netmsg.push(WrapperMsg { node_id: their_node_id.clone(), features: None, msg: Msg::FulfillHTLC(msg.clone()) });
		}
	}
	fn handle_update_fail_htlc(&self, their_node_id: &PublicKey, msg: &UpdateFailHTLC) {}
	fn handle_update_fail_malformed_htlc(&self, their_node_id: &PublicKey, msg: &UpdateFailMalformedHTLC) {}
	fn handle_commitment_signed(&self, their_node_id: &PublicKey, msg: &CommitmentSigned) {
		if let Ok(mut buffer_netmsg) = self.buffer_netmsg.lock() {
			buffer_netmsg.push(WrapperMsg { node_id: their_node_id.clone(), features: None, msg: Msg::Commitment(msg.clone()) });
		}
	}
	fn handle_revoke_and_ack(&self, their_node_id: &PublicKey, msg: &RevokeAndACK) {
		if let Ok(mut buffer_netmsg) = self.buffer_netmsg.lock() {
			buffer_netmsg.push(WrapperMsg { node_id: their_node_id.clone(), features: None, msg: Msg::RAA(msg.clone()) });
		}
	}
	fn handle_update_fee(&self, their_node_id: &PublicKey, msg: &UpdateFee) {}
	fn handle_announcement_signatures(&self, their_node_id: &PublicKey, msg: &AnnouncementSignatures) {}
	fn peer_disconnected(&self, their_node_id: &PublicKey, no_connection_possible: bool) {}
	fn peer_connected(&self, their_node_id: &PublicKey, msg: &Init) {}
	fn handle_channel_reestablish(&self, their_node_id: &PublicKey, msg: &ChannelReestablish) {}
	fn handle_error(&self, their_node_id: &PublicKey, msg: &ErrorMessage) {}
}

impl MessageSendEventsProvider for BufferNetMsg {
	fn get_and_clear_pending_msg_events(&self) -> Vec<MessageSendEvent> {
		if let Ok(mut chanman_msg_events) = self.chanman_msg_events.lock() {
			if let Ok(events) = chanman_msg_events.try_recv() {
				return events;
			}
		}
		vec![]
	}
}

impl RoutingMessageHandler for OutboundPeerManagerConnector {
	fn handle_node_announcement(&self, msg: &NodeAnnouncement) -> Result<bool, LightningError> {
		Ok(true)
	}
	fn handle_channel_announcement(&self, msg: &ChannelAnnouncement) -> Result<bool, LightningError> {
		Ok(true)
	}
	fn handle_channel_update(&self, msg: &ChannelUpdate) -> Result<bool, LightningError> {
		Ok(true)
	}
	fn handle_htlc_fail_channel_update(&self, update: &HTLCFailChannelUpdate) {}
	fn get_next_channel_announcements(&self, starting_point: u64, batch_amount: u8) -> Vec<(ChannelAnnouncement, Option<ChannelUpdate>, Option<ChannelUpdate>)> {
		vec![]
	}
	fn get_next_node_announcements(&self, starting_point: Option<&PublicKey>, batch_amount: u8) -> Vec<NodeAnnouncement> {
		vec![]
	}
	fn should_request_full_sync(&self, node_id: &PublicKey) -> bool {
		true
	}
}

pub(super) async fn setup_peer_manager<C, R, L>(outbound: Arc<OutboundPeerManagerConnector>, mut chanman_netmsg: Sender<WrapperMsg>, mut peers_rpccmd: Receiver<Vec<u8>>, mut peerman_notify: Receiver<()>, buffer_netmsg: Arc<BufferNetMsg>, chan_handler: Arc<C>, router: Arc<R>, node_secret: SecretKey, ephemeral_data: [u8; 32], logger: Arc<L>) -> (JoinHandle<()>, JoinHandle<()>, JoinHandle<()>, Arc<peer_handler::PeerManager<SocketDescriptor, Arc<C>, Arc<R>, Arc<L>>>)
	where C: ChannelMessageHandler + 'static,
	      R: RoutingMessageHandler + 'static,
	      L: Logger + 'static
{
	let peer_handler: peer_handler::PeerManager<SocketDescriptor, Arc<C>, Arc<R>, Arc<L>> = peer_handler::PeerManager::new(peer_handler::MessageHandler {
		chan_handler: chan_handler,
		route_handler: router,
	}, node_secret, &ephemeral_data, logger);
	let peer_handler_arc = Arc::new(peer_handler);
	let peer_handler_listener = peer_handler_arc.clone();
	let peer_handler_event = peer_handler_arc.clone();
	let join_handle_rpc: JoinHandle<()> = tokio::spawn(async move { // RPC cmd thread
		loop {
			if let Some(buffer) = peers_rpccmd.recv().await {
				let v: serde_json::Value = serde_json::from_slice(&buffer[..]).unwrap();
				let v_obj = v.as_object().unwrap();
				match v_obj.get("cmd").unwrap().as_str().unwrap() {
					"connect" => {
						let event_notify = outbound.event_handler.clone();
						let pubkey = PublicKey::from_str(v_obj.get("pubkey").unwrap().as_str().unwrap()).unwrap();
						let ip_addr: Vec<&str> = v_obj.get("hostname").unwrap().as_str().unwrap().split('.').collect();
						let port = v_obj.get("port").unwrap().as_str().unwrap().parse::<u16>().unwrap();
						let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(u8::from_str_radix(ip_addr[0], 10).unwrap(), u8::from_str_radix(ip_addr[1], 10).unwrap(), u8::from_str_radix(ip_addr[2], 10).unwrap(), u8::from_str_radix(ip_addr[3], 10).unwrap())), port);
						connect_outbound(peer_handler_arc.clone(), event_notify, pubkey, addr).await;
					},
					"disconnect" => {
					},
					_ => {},
				}
			}
		}
	});
	let join_handle_event: JoinHandle<()> = tokio::spawn(async move { // Event processing thread
		loop {
			if let Ok(_) = peerman_notify.try_recv() {
				println!("Processing event...");
				peer_handler_event.process_events();
			}
			let one_centisecond = Duration::from_millis(100);
			sleep(one_centisecond);
		}
	});
	let join_handle_msg: JoinHandle<()> = tokio::spawn(async move {
		loop {
			for e in buffer_netmsg.get_netmsg() {
				if let Ok(_) = chanman_netmsg.send(e).await {}
					//println!("Passing down netmsg to channel manger!");
			}
			let one_centisecond = Duration::from_millis(100);
			sleep(one_centisecond);
		}
	});
	(join_handle_rpc, join_handle_event, join_handle_msg, peer_handler_listener)
}

pub(crate) struct OutboundSocketListenerConnector {
	outbound_socket_events: Sender<()>
}

impl OutboundSocketListenerConnector {
	pub(crate) fn new(outbound_socket_events: Sender<()>) -> Self {
		OutboundSocketListenerConnector {
			outbound_socket_events,
		}
	}
}

pub(super) async fn setup_socket_listener<C: ChannelMessageHandler + 'static, R: RoutingMessageHandler + 'static, L: Logger + 'static>(peer_manager_arc: Arc<peer_handler::PeerManager<SocketDescriptor, Arc<C>, Arc<R>, Arc<L>>>, outbound: OutboundSocketListenerConnector, port: u16) -> JoinHandle<()>
{
	let join_handle: JoinHandle<()> = tokio::spawn(async move {
		let mut listener = tokio::net::TcpListener::bind(("::".parse::<std::net::Ipv6Addr>().unwrap(), port)).await.unwrap();
		loop {
			let sock = listener.accept().await.unwrap().0;
			let outbound_socket_events = outbound.outbound_socket_events.clone();
			let peer_manager_listener = peer_manager_arc.clone();
			tokio::spawn(async move {
				setup_inbound(peer_manager_listener, outbound_socket_events, sock).await;
			});
		}
	});
	join_handle
}

pub(crate) struct OutboundRPCServerConnector {
	peers_rpccmd: Sender<Vec<u8>>,
	chanman_rpccmd: Sender<Vec<u8>>,
	router_rpccmd: Sender<Vec<u8>>
}

impl OutboundRPCServerConnector {
	pub(crate) fn new(peers_rpccmd: Sender<Vec<u8>>, chanman_rpccmd: Sender<Vec<u8>>, router_rpccmd: Sender<Vec<u8>>) -> Self {
		OutboundRPCServerConnector {
			peers_rpccmd,
			chanman_rpccmd,
			router_rpccmd
		}
	}
}

pub(super) async fn setup_rpc_server(mut outbound: OutboundRPCServerConnector, port: u16) -> JoinHandle<()> {
	let join_handle: JoinHandle<()> = tokio::spawn(async move {
		let mut listener = tokio::net::TcpListener::bind(("::".parse::<std::net::Ipv6Addr>().unwrap(), port)).await.unwrap();
		loop {
			let mut sock = listener.accept().await.unwrap().0;
			let buffer_len = sock.read_u8().await.unwrap();
			let mut buffer = Vec::with_capacity(buffer_len as usize);
			sock.read_buf(&mut buffer).await.unwrap();
			let v: serde_json::Value = match serde_json::from_slice(&buffer[..]) {
				Ok(v) => v,
				Err(_) => {
					println!("Failed to parse RPC client command");
					return ();
				},
			};
			if !v.is_object() {
				println!("Failed to parse RPC client command");
				return ();
			}
			let v_obj = v.as_object().unwrap();
			if v_obj.get("cmd").is_none() {
				println!("Failed to get a RPC client command");
				return ();
			}
			match v_obj.get("cmd").unwrap().as_str().unwrap() {
				"connect" => {
					outbound.peers_rpccmd.send(buffer[..].to_vec()).await.unwrap();
				},
				"disconnect" => {
					outbound.peers_rpccmd.send(buffer[..].to_vec()).await.unwrap();
				},
				"list" => {
					outbound.router_rpccmd.send(buffer[..].to_vec()).await.unwrap();
				},
				"open" => {
					outbound.chanman_rpccmd.send(buffer[..].to_vec()).await.unwrap();
				},
				"invoice" => {
					outbound.chanman_rpccmd.send(buffer[..].to_vec()).await.unwrap();
				},
				"send" => {
					outbound.chanman_rpccmd.send(buffer[..].to_vec()).await.unwrap();
				},
				_ => {},
			}
		}
	});
	join_handle
}

/// Basic fee estimator
pub(crate) struct SampleFeeEstimator {
	background_est: AtomicUsize,
	normal_est: AtomicUsize,
	high_prio_est: AtomicUsize,
}
impl SampleFeeEstimator {
	pub(crate) fn new() -> Self {
		SampleFeeEstimator {
			background_est: AtomicUsize::new(0),
			normal_est: AtomicUsize::new(0),
			high_prio_est: AtomicUsize::new(0),
		}
	}
}
impl chaininterface::FeeEstimator for SampleFeeEstimator {
	fn get_est_sat_per_1000_weight(&self, conf_target: chaininterface::ConfirmationTarget) -> u32 {
		cmp::max(match conf_target {
			chaininterface::ConfirmationTarget::Background => self.background_est.load(Ordering::Acquire) as u32,
			chaininterface::ConfirmationTarget::Normal => self.normal_est.load(Ordering::Acquire) as u32,
			chaininterface::ConfirmationTarget::HighPriority => self.high_prio_est.load(Ordering::Acquire) as u32,
		}, 253)
	}
}

/// Basic transaction broadcast
pub(crate) struct TxBroadcaster {}

impl TxBroadcaster {
	pub(crate) fn new() -> Self {
		TxBroadcaster {}
	}
}

impl chaininterface::BroadcasterInterface for TxBroadcaster {
	fn broadcast_transaction(&self, _tx: &bitcoin::blockdata::transaction::Transaction) {

	}
}

pub(crate) struct ChainConnector {
	buffer_connect: Mutex<Vec<WrapperBlock>>,
	buffer_disconnect: Mutex<Vec<WrapperBlock>>,
}

impl ChainConnector {
	pub(crate) fn new() -> Self {
		ChainConnector {
			buffer_connect: Mutex::new(vec![]),
			buffer_disconnect: Mutex::new(vec![]),
		}
	}
	pub(crate) fn get_connect(&self) -> Vec<WrapperBlock> {
		if let Ok(mut buffer_connect) = self.buffer_connect.lock() {
			return buffer_connect.drain(..).collect();
		}
		vec![]
	}
}

impl ChainListener for ChainConnector {
	fn block_connected(&self, block: &Block, height: u32) {
		if let Ok(mut buffer_connect) = self.buffer_connect.lock() {
			buffer_connect.push(WrapperBlock { header: block.header.clone(), txdata: block.txdata.clone(), height });
		}
	}
	fn block_disconnected(&self, header: &BlockHeader, height: u32) {
		if let Ok(mut buffer_disconnect) = self.buffer_disconnect.lock() {
			buffer_disconnect.push(WrapperBlock { header: header.clone(), txdata: vec![], height });
		}
	}
}

pub(super) async fn setup_chain_backend<CL>(chain_tip_hash: BlockHash, block_source: (String, String), buffer_blocks: Arc<ChainConnector>, chain_listener: Arc<CL>, mut chanman_connect: Sender<WrapperBlock>) -> (JoinHandle<()>, JoinHandle<()>)
	where CL: ChainListener + 'static,
{
	let join_handle_chain: JoinHandle<()> = tokio::spawn(async move {
		let bitcoind_endpoint = HttpEndpoint::new(&("http://".to_string() + &block_source.1 + "/")).unwrap();
		let mut bitcoind_client = RPCClient::new(&block_source.0, bitcoind_endpoint);
		let chain_tip = bitcoind_client.get_header(&chain_tip_hash, None).await.unwrap();
		let block_sources = vec![&mut bitcoind_client as &mut dyn BlockSource];
		let backup_sources = vec![];

		let mut client = MicroSPVClient::init(chain_tip, block_sources, backup_sources, chain_listener, false);
		let mut interval = tokio::time::interval(Duration::from_secs(1));
		loop {
			interval.tick().await;
			if client.poll_best_tip().await {}
		}
	});
	let join_handle_block: JoinHandle<()> = tokio::spawn(async move {
		loop {
			for e in buffer_blocks.get_connect() {
				if let Ok(_) = chanman_connect.send(e).await {}
			}
			let one_centisecond = Duration::from_millis(100);
			sleep(one_centisecond);
		}
	});
	(join_handle_chain, join_handle_block)
}
