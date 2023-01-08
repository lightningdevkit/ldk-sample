use crate::disk;
use crate::hex_utils;
use crate::{
	ChannelManager, HTLCStatus, InvoicePayer, MillisatAmount, NetworkGraph, OnionMessenger,
	PaymentInfo, PaymentInfoStorage, PeerManager,
};
use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::hashes::Hash;
use bitcoin::network::constants::Network;
use bitcoin::secp256k1::PublicKey;
use lightning::chain::keysinterface::{KeysInterface, KeysManager, Recipient};
use lightning::ln::msgs::{DecodeError, NetAddress};
use lightning::ln::{PaymentHash, PaymentPreimage};
use lightning::onion_message::{CustomOnionMessageContents, Destination, OnionMessageContents};
use lightning::routing::gossip::NodeId;
use lightning::util::config::{ChannelHandshakeConfig, ChannelHandshakeLimits, UserConfig};
use lightning::util::errors::APIError;
use lightning::util::events::EventHandler;
use lightning::util::ser::{MaybeReadableArgs, Writeable, Writer};
use lightning_invoice::payment::PaymentError;
use lightning_invoice::{utils, Currency, Invoice};
use serde_json::json;
use std::env;
use std::io;
use std::io::Write;
use std::net::{SocketAddr, ToSocketAddrs};
use std::ops::Deref;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

pub(crate) struct LdkUserInfo {
	pub(crate) bitcoind_rpc_username: String,
	pub(crate) bitcoind_rpc_password: String,
	pub(crate) bitcoind_rpc_port: u16,
	pub(crate) bitcoind_rpc_host: String,
	pub(crate) ldk_storage_dir_path: String,
	pub(crate) ldk_peer_listening_port: u16,
	pub(crate) ldk_announced_listen_addr: Vec<NetAddress>,
	pub(crate) ldk_announced_node_name: [u8; 32],
	pub(crate) network: Network,
}

struct UserOnionMessageContents {
	tlv_type: u64,
	data: Vec<u8>,
}

impl CustomOnionMessageContents for UserOnionMessageContents {
	fn tlv_type(&self) -> u64 {
		self.tlv_type
	}
}
impl MaybeReadableArgs<u64> for UserOnionMessageContents {
	fn read<R: std::io::Read>(_r: &mut R, _args: u64) -> Result<Option<Self>, DecodeError> {
		// UserOnionMessageContents is only ever passed to `send_onion_message`, never to an
		// `OnionMessageHandler`, thus it does not need to implement the read side here.
		unreachable!();
	}
}
impl Writeable for UserOnionMessageContents {
	fn write<W: Writer>(&self, w: &mut W) -> Result<(), std::io::Error> {
		w.write_all(&self.data)
	}
}

pub(crate) async fn poll_for_user_input<E: EventHandler>(
	invoice_payer: Arc<InvoicePayer<E>>, peer_manager: Arc<PeerManager>,
	channel_manager: Arc<ChannelManager>, keys_manager: Arc<KeysManager>,
	network_graph: Arc<NetworkGraph>, onion_messenger: Arc<OnionMessenger>,
	inbound_payments: PaymentInfoStorage, outbound_payments: PaymentInfoStorage,
	ldk_data_dir: String, network: Network, logger: Arc<disk::FilesystemLogger>,
) {
	println!(
		"LDK startup successful. Enter \"help\" to view available commands. Press Ctrl-D to quit."
	);
	println!("LDK logs are available at <your-supplied-ldk-data-dir-path>/.ldk/logs");
	println!("Local Node ID is {}.", channel_manager.get_our_node_id());
	loop {
		print!("> ");
		io::stdout().flush().unwrap(); // Without flushing, the `>` doesn't print
		let mut line = String::new();
		if let Err(e) = io::stdin().read_line(&mut line) {
			break println!("ERROR: {}", e);
		}

		if line.len() == 0 {
			// We hit EOF / Ctrl-D
			break;
		}

		let mut words = line.split_whitespace();
		if let Some(word) = words.next() {
			match word {
				"help" => help(),
				"openchannel" => {
					let peer_pubkey_and_ip_addr = words.next();
					let channel_value_sat = words.next();
					if peer_pubkey_and_ip_addr.is_none() || channel_value_sat.is_none() {
						println!("ERROR: openchannel has 2 required arguments: `openchannel pubkey@host:port channel_amt_satoshis` [--public]");
						continue;
					}
					let peer_pubkey_and_ip_addr = peer_pubkey_and_ip_addr.unwrap();
					let (pubkey, peer_addr) =
						match parse_peer_info(peer_pubkey_and_ip_addr.to_string()) {
							Ok(info) => info,
							Err(e) => {
								println!("{:?}", e.into_inner().unwrap());
								continue;
							}
						};

					let chan_amt_sat: Result<u64, _> = channel_value_sat.unwrap().parse();
					if chan_amt_sat.is_err() {
						println!("ERROR: channel amount must be a number");
						continue;
					}

					if connect_peer_if_necessary(pubkey, peer_addr, peer_manager.clone())
						.await
						.is_err()
					{
						continue;
					};

					let announce_channel = match words.next() {
						Some("--public") | Some("--public=true") => true,
						Some("--public=false") => false,
						Some(_) => {
							println!("ERROR: invalid `--public` command format. Valid formats: `--public`, `--public=true` `--public=false`");
							continue;
						}
						None => false,
					};

					match open_channel(
						pubkey,
						chan_amt_sat.unwrap(),
						announce_channel,
						channel_manager.clone(),
					) {
						Ok(channel) => {
							let peer_data_path =
								format!("{}/channel_peer_data", ldk_data_dir.clone());
							let _ = disk::persist_channel_peer(
								Path::new(&peer_data_path),
								peer_pubkey_and_ip_addr,
							);
							println!(
								"Channel details:\n{}",
								serde_json::to_string_pretty(&channel).unwrap()
							)
						}
						Err(message) => {
							println!("ERROR: {}", message);
						}
					}
				}
				"sendpayment" => {
					let invoice_str = words.next();
					if invoice_str.is_none() {
						println!("ERROR: sendpayment requires an invoice: `sendpayment <invoice>`");
						continue;
					}

					let invoice = match Invoice::from_str(invoice_str.unwrap()) {
						Ok(inv) => inv,
						Err(e) => {
							println!("ERROR: invalid invoice: {:?}", e);
							continue;
						}
					};

					match send_payment(&*invoice_payer, &invoice, outbound_payments.clone()) {
						Ok(_) => continue,
						Err(message) => println!("ERROR: {}", message),
					};
				}
				"keysend" => {
					let dest_pubkey = match words.next() {
						Some(dest) => match hex_utils::to_compressed_pubkey(dest) {
							Some(pk) => pk,
							None => {
								println!("ERROR: couldn't parse destination pubkey");
								continue;
							}
						},
						None => {
							println!("ERROR: keysend requires a destination pubkey: `keysend <dest_pubkey> <amt_msat>`");
							continue;
						}
					};
					let amt_msat_str = match words.next() {
						Some(amt) => amt,
						None => {
							println!("ERROR: keysend requires an amount in millisatoshis: `keysend <dest_pubkey> <amt_msat>`");
							continue;
						}
					};
					let amt_msat: u64 = match amt_msat_str.parse() {
						Ok(amt) => amt,
						Err(e) => {
							println!("ERROR: couldn't parse amount_msat: {}", e);
							continue;
						}
					};
					match keysend(
						&*invoice_payer,
						dest_pubkey,
						amt_msat,
						&*keys_manager,
						outbound_payments.clone(),
					) {
						Ok(_) => continue,
						Err(message) => println!("ERROR: {}", message),
					};
				}
				"getinvoice" => {
					let amt_str = words.next();
					if amt_str.is_none() {
						println!("ERROR: getinvoice requires an amount in millisatoshis");
						continue;
					}

					let amt_msat: Result<u64, _> = amt_str.unwrap().parse();
					if amt_msat.is_err() {
						println!("ERROR: getinvoice provided payment amount was not a number");
						continue;
					}

					let expiry_secs_str = words.next();
					if expiry_secs_str.is_none() {
						println!("ERROR: getinvoice requires an expiry in seconds");
						continue;
					}

					let expiry_secs: Result<u32, _> = expiry_secs_str.unwrap().parse();
					if expiry_secs.is_err() {
						println!("ERROR: getinvoice provided expiry was not a number");
						continue;
					}

					match get_invoice(
						amt_msat.unwrap(),
						Arc::clone(&inbound_payments),
						&*channel_manager,
						Arc::clone(&keys_manager),
						network,
						expiry_secs.unwrap(),
						Arc::clone(&logger),
					) {
						Ok(invoice) => println!("SUCCESS: Generated invoice\n{}", invoice),
						Err(message) => println!("ERROR: {}", message),
					};
				}
				"connectpeer" => {
					let peer_pubkey_and_ip_addr = words.next();
					if peer_pubkey_and_ip_addr.is_none() {
						println!("ERROR: connectpeer requires peer connection info: `connectpeer pubkey@host:port`");
						continue;
					}
					let (pubkey, peer_addr) =
						match parse_peer_info(peer_pubkey_and_ip_addr.unwrap().to_string()) {
							Ok(info) => info,
							Err(e) => {
								println!("{:?}", e.into_inner().unwrap());
								continue;
							}
						};
					match connect_peer_if_necessary(pubkey, peer_addr, peer_manager.clone()).await {
						Ok(()) => println!("SUCCESS: connected to peer {}", pubkey),
						Err(()) => println!("ERROR: Could not connect to peer"),
					}
				}
				"disconnectpeer" => {
					let peer_pubkey = words.next();
					if peer_pubkey.is_none() {
						println!("ERROR: disconnectpeer requires peer public key: `disconnectpeer <peer_pubkey>`");
						continue;
					}

					let peer_pubkey =
						match bitcoin::secp256k1::PublicKey::from_str(peer_pubkey.unwrap()) {
							Ok(pubkey) => pubkey,
							Err(e) => {
								println!("ERROR: {}", e.to_string());
								continue;
							}
						};

					if do_disconnect_peer(
						peer_pubkey,
						peer_manager.clone(),
						channel_manager.clone(),
					)
					.is_ok()
					{
						println!("SUCCESS: disconnected from peer {}", peer_pubkey);
					}
				}
				"listchannels" => {
					match list_channels(&channel_manager, &network_graph) {
						Ok(channels) => {
							println!("{}", serde_json::to_string_pretty(&channels).unwrap())
						}
						Err(_) => print!("ERROR: Could not fetch channel info"),
					};
				}
				"listpayments" => {
					match list_payments(inbound_payments.clone(), outbound_payments.clone()) {
						Ok(payments) => {
							println!("{}", serde_json::to_string_pretty(&payments).unwrap())
						}
						Err(_) => print!("ERROR: Could not fetch payments"),
					};
				}
				"closechannel" => {
					let channel_id_str = words.next();
					if channel_id_str.is_none() {
						println!("ERROR: closechannel requires a channel ID: `closechannel <channel_id> <peer_pubkey>`");
						continue;
					}
					let channel_id_vec = hex_utils::to_vec(channel_id_str.unwrap());
					if channel_id_vec.is_none() || channel_id_vec.as_ref().unwrap().len() != 32 {
						println!("ERROR: couldn't parse channel_id");
						continue;
					}
					let mut channel_id = [0; 32];
					channel_id.copy_from_slice(&channel_id_vec.unwrap());

					let peer_pubkey_str = words.next();
					if peer_pubkey_str.is_none() {
						println!("ERROR: closechannel requires a peer pubkey: `closechannel <channel_id> <peer_pubkey>`");
						continue;
					}
					let peer_pubkey_vec = match hex_utils::to_vec(peer_pubkey_str.unwrap()) {
						Some(peer_pubkey_vec) => peer_pubkey_vec,
						None => {
							println!("ERROR: couldn't parse peer_pubkey");
							continue;
						}
					};
					let peer_pubkey = match PublicKey::from_slice(&peer_pubkey_vec) {
						Ok(peer_pubkey) => peer_pubkey,
						Err(_) => {
							println!("ERROR: couldn't parse peer_pubkey");
							continue;
						}
					};

					match close_channel(channel_id, peer_pubkey, channel_manager.clone()) {
						Ok(msg) => println!("SUCCESS: {}", msg),
						Err(e) => println!("ERROR: Could not close channel\n{:?}", e),
					}
				}
				"forceclosechannel" => {
					let channel_id_str = words.next();
					if channel_id_str.is_none() {
						println!("ERROR: forceclosechannel requires a channel ID: `forceclosechannel <channel_id> <peer_pubkey>`");
						continue;
					}
					let channel_id_vec = hex_utils::to_vec(channel_id_str.unwrap());
					if channel_id_vec.is_none() || channel_id_vec.as_ref().unwrap().len() != 32 {
						println!("ERROR: couldn't parse channel_id");
						continue;
					}
					let mut channel_id = [0; 32];
					channel_id.copy_from_slice(&channel_id_vec.unwrap());

					let peer_pubkey_str = words.next();
					if peer_pubkey_str.is_none() {
						println!("ERROR: forceclosechannel requires a peer pubkey: `forceclosechannel <channel_id> <peer_pubkey>`");
						continue;
					}
					let peer_pubkey_vec = match hex_utils::to_vec(peer_pubkey_str.unwrap()) {
						Some(peer_pubkey_vec) => peer_pubkey_vec,
						None => {
							println!("ERROR: couldn't parse peer_pubkey");
							continue;
						}
					};
					let peer_pubkey = match PublicKey::from_slice(&peer_pubkey_vec) {
						Ok(peer_pubkey) => peer_pubkey,
						Err(_) => {
							println!("ERROR: couldn't parse peer_pubkey");
							continue;
						}
					};

					match force_close_channel(channel_id, peer_pubkey, channel_manager.clone()) {
						Ok(msg) => println!("SUCCESS: {}", msg),
						Err(e) => println!("ERROR: Could not force close channel\n{:?}", e),
					}
				}
				"nodeinfo" => {
					match node_info(&channel_manager, &peer_manager) {
						Ok(info) => println!("{}", serde_json::to_string_pretty(&info).unwrap()),
						Err(_) => print!("ERROR: Could not fetch node info"),
					};
				}
				"listpeers" => {
					match list_peers(peer_manager.clone()) {
						Ok(peers) => println!("{}", serde_json::to_string_pretty(&peers).unwrap()),
						Err(_) => print!("ERROR: Could not fetch peer info"),
					};
				}
				"signmessage" => {
					const MSG_STARTPOS: usize = "signmessage".len() + 1;
					if line.as_bytes().len() <= MSG_STARTPOS {
						println!("ERROR: signmsg requires a message");
						continue;
					}
					match lightning::util::message_signing::sign(
						&line.as_bytes()[MSG_STARTPOS..],
						&keys_manager.get_node_secret(Recipient::Node).unwrap(),
					) {
						Ok(sig) => println!("{:?}", sig),
						Err(e) => println!("ERROR: Couldn't sign message {:?}", e),
					}
				}
				"sendonionmessage" => {
					let path_pks_str = words.next();
					if path_pks_str.is_none() {
						println!(
							"ERROR: sendonionmessage requires at least one node id for the path"
						);
						continue;
					}
					let mut node_pks = Vec::new();
					let mut errored = false;
					for pk_str in path_pks_str.unwrap().split(",") {
						let node_pubkey_vec = match hex_utils::to_vec(pk_str) {
							Some(peer_pubkey_vec) => peer_pubkey_vec,
							None => {
								println!("ERROR: couldn't parse peer_pubkey");
								errored = true;
								break;
							}
						};
						let node_pubkey = match PublicKey::from_slice(&node_pubkey_vec) {
							Ok(peer_pubkey) => peer_pubkey,
							Err(_) => {
								println!("ERROR: couldn't parse peer_pubkey");
								errored = true;
								break;
							}
						};
						node_pks.push(node_pubkey);
					}
					if errored {
						continue;
					}
					let tlv_type = match words.next().map(|ty_str| ty_str.parse()) {
						Some(Ok(ty)) if ty >= 64 => ty,
						_ => {
							println!("Need an integral message type above 64");
							continue;
						}
					};
					let data = match words.next().map(|s| hex_utils::to_vec(s)) {
						Some(Some(data)) => data,
						_ => {
							println!("Need a hex data string");
							continue;
						}
					};
					let destination_pk = node_pks.pop().unwrap();
					match onion_messenger.send_onion_message(
						&node_pks,
						Destination::Node(destination_pk),
						OnionMessageContents::Custom(UserOnionMessageContents { tlv_type, data }),
						None,
					) {
						Ok(()) => println!("SUCCESS: forwarded onion message to first hop"),
						Err(e) => println!("ERROR: failed to send onion message: {:?}", e),
					}
				}
				"quit" | "exit" => break,
				_ => println!("Unknown command. See `\"help\" for available commands."),
			}
		}
	}
}

fn help() {
	let package_version = env!("CARGO_PKG_VERSION");
	let package_name = env!("CARGO_PKG_NAME");
	println!("\nVERSION:");
	println!("  {} v{}", package_name, package_version);
	println!("\nUSAGE:");
	println!("  Command [arguments]");
	println!("\nCOMMANDS:");
	println!("  help\tShows a list of commands.");
	println!("  quit\tClose the application.");
	println!("\n  Channels:");
	println!("      openchannel pubkey@host:port <amt_satoshis> [--public]");
	println!("      closechannel <channel_id> <peer_pubkey>");
	println!("      forceclosechannel <channel_id> <peer_pubkey>");
	println!("      listchannels");
	println!("\n  Peers:");
	println!("      connectpeer pubkey@host:port");
	println!("      disconnectpeer <peer_pubkey>");
	println!("      listpeers");
	println!("\n  Payments:");
	println!("      sendpayment <invoice>");
	println!("      keysend <dest_pubkey> <amt_msats>");
	println!("      listpayments");
	println!("\n  Invoices:");
	println!("      getinvoice <amt_msats> <expiry_secs>");
	println!("\n  Other:");
	println!("      signmessage <message>");
	println!(
		"      sendonionmessage <node_id_1,node_id_2,..,destination_node_id> <type> <hex_bytes>"
	);
	println!("      nodeinfo");
}

fn node_info(
	channel_manager: &Arc<ChannelManager>, peer_manager: &Arc<PeerManager>,
) -> Result<serde_json::value::Value, ()> {
	let chans = channel_manager.list_channels();
	let node_pubkey = channel_manager.get_our_node_id();
	let num_channels = chans.len();
	let num_usable_channels = chans.iter().filter(|c| c.is_usable).count();
	let local_balance_msat = chans.iter().map(|c| c.balance_msat).sum::<u64>();
	let num_peers = peer_manager.get_peer_node_ids().len();

	let node_info = json!({
		"node_pubkey": node_pubkey.to_string(),
		"num_channels": num_channels,
		"num_usable_channels": num_usable_channels,
		"local_balance_msat": local_balance_msat,
		"num_peers": num_peers
	});

	Ok(node_info)
}

fn list_peers(peer_manager: Arc<PeerManager>) -> Result<serde_json::value::Value, ()> {
	let mut peers: Vec<String> = Vec::new();
	for peer in peer_manager.get_peer_node_ids() {
		peers.push(peer.to_string());
	}
	let list_peers = json!({ "peers": peers });

	Ok(list_peers)
}

fn list_channels(
	channel_manager: &Arc<ChannelManager>, network_graph: &Arc<NetworkGraph>,
) -> Result<serde_json::value::Value, ()> {
	let mut channels: Vec<serde_json::value::Value> = Vec::new();
	for channel in channel_manager.list_channels() {
		let channel_id: String = hex_utils::hex_str(&channel.channel_id[..]);
		let funding_txid: String = match channel.funding_txo {
			Some(funding_txo) => funding_txo.txid.to_string(),
			None => "".to_string(),
		};
		let peer_pubkey: String = hex_utils::hex_str(&channel.counterparty.node_id.serialize());
		let peer_alias = match network_graph
			.read_only()
			.nodes()
			.get(&NodeId::from_pubkey(&channel.counterparty.node_id))
		{
			Some(node_info) => {
				if let Some(announcement) = &node_info.announcement_info {
					announcement.alias.to_string()
				} else {
					"".to_string()
				}
			}
			None => "".to_string(),
		};

		let short_channel_id: u64 = match channel.short_channel_id {
			Some(id) => id,
			None => 0,
		};
		let (inbound_capacity_msat, outbound_capacity_msat) = match channel.is_usable {
			true => (channel.inbound_capacity_msat, channel.outbound_capacity_msat),
			false => (0, 0),
		};
		let channel_capacity_msat = json!({
			"inbound": inbound_capacity_msat,
			"outbound": outbound_capacity_msat
		});

		let channel_json = json!({
			"channel_id": channel_id,
			"short_channel_id": short_channel_id,
			"funding_txid": funding_txid,
			"peer_pubkey": peer_pubkey,
			"peer_alias": peer_alias,
			"is_channel_ready": channel.is_channel_ready,
			"can_make_payments": channel.is_usable,
			"channel_value_satoshis": channel.channel_value_satoshis,
			"balance_msat": channel.balance_msat,
			"channel_capacity_msat": channel_capacity_msat,
			"public": channel.is_public
		});

		channels.push(channel_json);
	}

	let list_channels = json!({ "channels": channels });

	Ok(list_channels)
}

fn list_payments(
	inbound_payments: PaymentInfoStorage, outbound_payments: PaymentInfoStorage,
) -> Result<serde_json::value::Value, ()> {
	let inbound = inbound_payments.lock().unwrap();
	let outbound = outbound_payments.lock().unwrap();
	let mut total_inbound_json: Vec<serde_json::value::Value> = Vec::new();
	let mut total_outbound_json: Vec<serde_json::value::Value> = Vec::new();

	for (payment_hash, payment_info) in inbound.deref() {
		let htlc_status = match payment_info.status {
			HTLCStatus::Pending => "pending",
			HTLCStatus::Succeeded => "succeeded",
			HTLCStatus::Failed => "failed",
		};

		let inbound_json = json!({
			"amount_msats": payment_info.amt_msat.to_string(),
			"payment_hash": hex_utils::hex_str(&payment_hash.0),
			"htlc_direction": "inbound",
			"htlc_status": htlc_status
		});

		total_inbound_json.push(inbound_json);
	}

	for (payment_hash, payment_info) in outbound.deref() {
		let htlc_status = match payment_info.status {
			HTLCStatus::Pending => "pending",
			HTLCStatus::Succeeded => "succeeded",
			HTLCStatus::Failed => "failed",
		};

		let outbound_json = json!({
			"amount_msats": payment_info.amt_msat.to_string(),
			"payment_hash": hex_utils::hex_str(&payment_hash.0),
			"htlc_direction": "outbound",
			"htlc_status": htlc_status
		});

		total_outbound_json.push(outbound_json);
	}

	let list_payments = json!({
		"received": total_inbound_json,
		"sent": total_outbound_json
	});

	Ok(list_payments)
}

pub(crate) async fn connect_peer_if_necessary(
	pubkey: PublicKey, peer_addr: SocketAddr, peer_manager: Arc<PeerManager>,
) -> Result<(), ()> {
	for node_pubkey in peer_manager.get_peer_node_ids() {
		if node_pubkey == pubkey {
			return Ok(());
		}
	}

	let res = do_connect_peer(pubkey, peer_addr, peer_manager).await;
	if res.is_err() {
		return Err(());
	}

	Ok(())
}

pub(crate) async fn do_connect_peer(
	pubkey: PublicKey, peer_addr: SocketAddr, peer_manager: Arc<PeerManager>,
) -> Result<(), ()> {
	match lightning_net_tokio::connect_outbound(Arc::clone(&peer_manager), pubkey, peer_addr).await
	{
		Some(connection_closed_future) => {
			let mut connection_closed_future = Box::pin(connection_closed_future);
			loop {
				match futures::poll!(&mut connection_closed_future) {
					std::task::Poll::Ready(_) => {
						return Err(());
					}
					std::task::Poll::Pending => {}
				}
				// Avoid blocking the tokio context by sleeping a bit
				match peer_manager.get_peer_node_ids().iter().find(|id| **id == pubkey) {
					Some(_) => return Ok(()),
					None => tokio::time::sleep(Duration::from_millis(10)).await,
				}
			}
		}
		None => Err(()),
	}
}

fn do_disconnect_peer(
	pubkey: bitcoin::secp256k1::PublicKey, peer_manager: Arc<PeerManager>,
	channel_manager: Arc<ChannelManager>,
) -> Result<(), ()> {
	//check for open channels with peer
	for channel in channel_manager.list_channels() {
		if channel.counterparty.node_id == pubkey {
			println!("ERROR: Node has an active channel with this peer, close any channels first");
			return Err(());
		}
	}

	//check the pubkey matches a valid connected peer
	let peers = peer_manager.get_peer_node_ids();
	if !peers.contains(&pubkey) {
		println!("ERROR: Could not find peer {}", pubkey);
		return Err(());
	}

	peer_manager.disconnect_by_node_id(pubkey, false);
	Ok(())
}

fn open_channel(
	peer_pubkey: PublicKey, channel_amt_sat: u64, announced_channel: bool,
	channel_manager: Arc<ChannelManager>,
) -> Result<serde_json::value::Value, String> {
	let config = UserConfig {
		channel_handshake_limits: ChannelHandshakeLimits {
			// lnd's max to_self_delay is 2016, so we want to be compatible.
			their_to_self_delay: 2016,
			..Default::default()
		},
		channel_handshake_config: ChannelHandshakeConfig {
			announced_channel,
			..Default::default()
		},
		..Default::default()
	};

	match channel_manager.create_channel(peer_pubkey, channel_amt_sat, 0, 0, Some(config)) {
		Ok(_) => {
			println!("EVENT: Initiated channel with peer {}. ", peer_pubkey);
			let channel_json = json!({
				"peer_pubkey": peer_pubkey.to_string(),
				"amount": channel_amt_sat,
				"public": announced_channel
			});
			Ok(channel_json)
		}
		Err(e) => {
			println!("ERROR: {:?}", e);
			Err("Failed to open channel".to_string())
		}
	}
}

fn send_payment<E: EventHandler>(
	invoice_payer: &InvoicePayer<E>, invoice: &Invoice, payment_storage: PaymentInfoStorage,
) -> Result<serde_json::value::Value, String> {
	let status = match invoice_payer.pay_invoice(invoice) {
		Ok(_payment_id) => {
			let payee_pubkey = invoice.recover_payee_pub_key();
			let amt_msat = invoice.amount_milli_satoshis().unwrap();
			println!("EVENT: initiated sending {} msats to {}", amt_msat, payee_pubkey);
			HTLCStatus::Pending
		}
		Err(PaymentError::Invoice(e)) => {
			return Err(e.to_string());
		}
		Err(PaymentError::Routing(e)) => {
			return Err(e.err);
		}
		Err(PaymentError::Sending(e)) => {
			println!("{:?}", e);
			HTLCStatus::Failed
		}
	};
	let payment_successful = match status {
		HTLCStatus::Succeeded | HTLCStatus::Pending => true,
		HTLCStatus::Failed => false,
	};
	let payment_hash = PaymentHash(invoice.payment_hash().clone().into_inner());
	let payment_secret = Some(invoice.payment_secret().clone());

	let mut payments = payment_storage.lock().unwrap();
	payments.insert(
		payment_hash,
		PaymentInfo {
			preimage: None,
			secret: payment_secret,
			status,
			amt_msat: MillisatAmount(invoice.amount_milli_satoshis()),
		},
	);
	let payment_json = json!({
		"payee_pubkey": invoice.recover_payee_pub_key().to_string(),
		"amount_msat": invoice.amount_milli_satoshis().unwrap()
	});

	match payment_successful {
		true => Ok(payment_json),
		false => Err("Failed to send payment".to_string()),
	}
}

fn keysend<E: EventHandler, K: KeysInterface>(
	invoice_payer: &InvoicePayer<E>, payee_pubkey: PublicKey, amt_msat: u64, keys: &K,
	payment_storage: PaymentInfoStorage,
) -> Result<serde_json::value::Value, String> {
	let payment_preimage = keys.get_secure_random_bytes();

	let status = match invoice_payer.pay_pubkey(
		payee_pubkey,
		PaymentPreimage(payment_preimage),
		amt_msat,
		40,
	) {
		Ok(_payment_id) => {
			println!("EVENT: initiated sending {} msats to {}", amt_msat, payee_pubkey);
			print!("> ");
			HTLCStatus::Pending
		}
		Err(PaymentError::Invoice(e)) => {
			return Err(e.to_string());
		}
		Err(PaymentError::Routing(e)) => {
			return Err(e.err);
		}
		Err(PaymentError::Sending(e)) => {
			println!("{:?}", e);
			HTLCStatus::Failed
		}
	};
	let payment_successful = match status {
		HTLCStatus::Succeeded | HTLCStatus::Pending => true,
		HTLCStatus::Failed => false,
	};
	let mut payments = payment_storage.lock().unwrap();
	payments.insert(
		PaymentHash(Sha256::hash(&payment_preimage).into_inner()),
		PaymentInfo {
			preimage: None,
			secret: None,
			status,
			amt_msat: MillisatAmount(Some(amt_msat)),
		},
	);
	let payment_json = json!({
		"payee_pubkey": payee_pubkey.to_string(),
		"amount_msat": amt_msat
	});

	if !payment_successful {
		return Err("Failed to send payment".to_string());
	}

	Ok(payment_json)
}

fn get_invoice(
	amt_msat: u64, payment_storage: PaymentInfoStorage, channel_manager: &ChannelManager,
	keys_manager: Arc<KeysManager>, network: Network, expiry_secs: u32,
	logger: Arc<disk::FilesystemLogger>,
) -> Result<Invoice, String> {
	let mut payments = payment_storage.lock().unwrap();
	let currency = match network {
		Network::Bitcoin => Currency::Bitcoin,
		Network::Testnet => Currency::BitcoinTestnet,
		Network::Regtest => Currency::Regtest,
		Network::Signet => Currency::Signet,
	};
	let invoice = match utils::create_invoice_from_channelmanager(
		channel_manager,
		keys_manager,
		logger,
		currency,
		Some(amt_msat),
		"ldk-tutorial-node".to_string(),
		expiry_secs,
	) {
		Ok(inv) => inv,
		Err(e) => {
			println!("ERROR: failed to create invoice: {:?}", e);
			return Err(e.to_string());
		}
	};

	let payment_hash = PaymentHash(invoice.payment_hash().clone().into_inner());
	payments.insert(
		payment_hash,
		PaymentInfo {
			preimage: None,
			secret: Some(invoice.payment_secret().clone()),
			status: HTLCStatus::Pending,
			amt_msat: MillisatAmount(Some(amt_msat)),
		},
	);

	Ok(invoice)
}

fn close_channel(
	channel_id: [u8; 32], counterparty_node_id: PublicKey, channel_manager: Arc<ChannelManager>,
) -> Result<String, APIError> {
	let result = match channel_manager.close_channel(&channel_id, &counterparty_node_id) {
		Ok(()) => Ok("Initiating channel close".to_string()),
		Err(e) => Err(e),
	};
	result
}

fn force_close_channel(
	channel_id: [u8; 32], counterparty_node_id: PublicKey, channel_manager: Arc<ChannelManager>,
) -> Result<String, APIError> {
	let result = match channel_manager
		.force_close_broadcasting_latest_txn(&channel_id, &counterparty_node_id)
	{
		Ok(()) => Ok("Initiating channel force-close".to_string()),
		Err(e) => Err(e),
	};
	result
}

pub(crate) fn parse_peer_info(
	peer_pubkey_and_ip_addr: String,
) -> Result<(PublicKey, SocketAddr), std::io::Error> {
	let mut pubkey_and_addr = peer_pubkey_and_ip_addr.split("@");
	let pubkey = pubkey_and_addr.next();
	let peer_addr_str = pubkey_and_addr.next();
	if peer_addr_str.is_none() {
		return Err(std::io::Error::new(
			std::io::ErrorKind::Other,
			"ERROR: incorrectly formatted peer info. Should be formatted as: `pubkey@host:port`",
		));
	}

	let peer_addr = peer_addr_str.unwrap().to_socket_addrs().map(|mut r| r.next());
	if peer_addr.is_err() || peer_addr.as_ref().unwrap().is_none() {
		return Err(std::io::Error::new(
			std::io::ErrorKind::Other,
			"ERROR: couldn't parse pubkey@host:port into a socket address",
		));
	}

	let pubkey = hex_utils::to_compressed_pubkey(pubkey.unwrap());
	if pubkey.is_none() {
		return Err(std::io::Error::new(
			std::io::ErrorKind::Other,
			"ERROR: unable to parse given pubkey for node",
		));
	}

	Ok((pubkey.unwrap(), peer_addr.unwrap().unwrap()))
}
