use crate::disk::{INBOUND_PAYMENTS_FNAME, OUTBOUND_PAYMENTS_FNAME};
use crate::hex_utils;
use crate::{
	ChainMonitor, ChannelManager, HTLCStatus, InboundPaymentInfoStorage, MillisatAmount,
	NetworkGraph, OutboundPaymentInfoStorage, PaymentInfo, PeerManager,
};
use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::hashes::Hash;
use bitcoin::network::Network;
use bitcoin::secp256k1::PublicKey;
use lightning::chain::channelmonitor::Balance;
use lightning::ln::channelmanager::{
	Bolt11InvoiceParameters, OptionalOfferPaymentParams, PaymentId, RecipientOnionFields, Retry,
};
use lightning::ln::msgs::SocketAddress;
use lightning::ln::types::ChannelId;
use lightning::offers::offer::{self, Offer};
use lightning::onion_message::dns_resolution::HumanReadableName;
use lightning::onion_message::messenger::Destination;
use lightning::routing::gossip::NodeId;
use lightning::routing::router::{PaymentParameters, RouteParameters, RouteParametersConfig};
use lightning::sign::{EntropySource, KeysManager};
use lightning::types::payment::{PaymentHash, PaymentPreimage};
use lightning::util::config::{ChannelHandshakeConfig, ChannelHandshakeLimits, UserConfig};
use lightning::util::persist::KVStore;
use lightning::util::ser::Writeable;
use lightning_invoice::Bolt11Invoice;
use lightning_persister::fs_store::FilesystemStore;
use std::env;
use std::io::Write;
use std::net::{SocketAddr, ToSocketAddrs};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};

pub(crate) struct LdkUserInfo {
	pub(crate) bitcoind_rpc_username: String,
	pub(crate) bitcoind_rpc_password: String,
	pub(crate) bitcoind_rpc_port: u16,
	pub(crate) bitcoind_rpc_host: String,
	pub(crate) ldk_storage_dir_path: String,
	pub(crate) ldk_peer_listening_port: u16,
	pub(crate) ldk_announced_listen_addr: Vec<SocketAddress>,
	pub(crate) ldk_announced_node_name: [u8; 32],
	pub(crate) network: Network,
}

pub(crate) async fn poll_for_user_input(
	peer_manager: Arc<PeerManager>, channel_manager: Arc<ChannelManager>,
	chain_monitor: Arc<ChainMonitor>, keys_manager: Arc<KeysManager>,
	network_graph: Arc<NetworkGraph>, inbound_payments: Arc<Mutex<InboundPaymentInfoStorage>>,
	outbound_payments: Arc<Mutex<OutboundPaymentInfoStorage>>, fs_store: Arc<FilesystemStore>,
) {
	println!(
		"LDK startup successful. Enter \"help\" to view available commands. Press Ctrl-D to quit."
	);
	println!("LDK logs are available at <your-supplied-ldk-data-dir-path>/.ldk/logs");
	println!("Local Node ID is {}.", channel_manager.get_our_node_id());

	let mut input = BufReader::new(tokio::io::stdin()).lines();
	'read_command: loop {
		print!("> ");
		std::io::stdout().flush().unwrap(); // Without flushing, the `>` doesn't print
		let line = match input.next_line().await {
			Ok(Some(l)) => l,
			Err(e) => {
				break println!("ERROR: {}", e);
			},
			Ok(None) => {
				break println!("ERROR: End of stdin");
			},
		};

		let mut words = line.split_whitespace();
		if let Some(word) = words.next() {
			match word {
				"help" => help(),
				"openchannel" => {
					let peer_pubkey_and_ip_addr = words.next();
					let channel_value_sat = words.next();
					if peer_pubkey_and_ip_addr.is_none() || channel_value_sat.is_none() {
						println!("ERROR: openchannel has 2 required arguments: `openchannel pubkey@host:port channel_amt_satoshis` [--public] [--with-anchors]");
						continue;
					}
					let peer_pubkey_and_ip_addr = peer_pubkey_and_ip_addr.unwrap();

					let mut pubkey_and_addr = peer_pubkey_and_ip_addr.split("@");
					let pubkey = pubkey_and_addr.next();
					let peer_addr_str = pubkey_and_addr.next();
					let pubkey = hex_utils::to_compressed_pubkey(pubkey.unwrap());
					if pubkey.is_none() {
						println!("ERROR: unable to parse given pubkey for node");
						continue;
					}
					let pubkey = pubkey.unwrap();

					if peer_addr_str.is_none() {
						if peer_manager.peer_by_node_id(&pubkey).is_none() {
							println!("ERROR: Peer address not provided and peer is not connected");
							continue;
						}
					} else {
						let (pubkey, peer_addr) =
							match parse_peer_info(peer_pubkey_and_ip_addr.to_string()) {
								Ok(info) => info,
								Err(e) => {
									println!("{:?}", e.into_inner().unwrap());
									continue;
								},
							};

						if tokio::runtime::Handle::current()
							.block_on(connect_peer_if_necessary(
								pubkey,
								peer_addr,
								peer_manager.clone(),
							))
							.is_err()
						{
							continue;
						};
					}

					let chan_amt_sat: Result<u64, _> = channel_value_sat.unwrap().parse();
					if chan_amt_sat.is_err() {
						println!("ERROR: channel amount must be a number");
						continue;
					}
					let (mut announce_channel, mut with_anchors) = (false, false);
					while let Some(word) = words.next() {
						match word {
							"--public" | "--public=true" => announce_channel = true,
							"--public=false" => announce_channel = false,
							"--with-anchors" | "--with-anchors=true" => with_anchors = true,
							"--with-anchors=false" => with_anchors = false,
							_ => {
								println!("ERROR: invalid boolean flag format. Valid formats: `--option`, `--option=true` `--option=false`");
								continue;
							},
						}
					}

					let _ = open_channel(
						pubkey,
						chan_amt_sat.unwrap(),
						announce_channel,
						with_anchors,
						channel_manager.clone(),
					);
				},
				"sendpayment" => {
					let invoice_str = words.next();
					if invoice_str.is_none() {
						println!("ERROR: sendpayment requires an invoice: `sendpayment <invoice> [amount_msat]`");
						continue;
					}
					let invoice_str = invoice_str.unwrap();

					let mut user_provided_amt: Option<u64> = None;
					if let Some(amt_msat_str) = words.next() {
						match amt_msat_str.parse() {
							Ok(amt) => user_provided_amt = Some(amt),
							Err(e) => {
								println!("ERROR: couldn't parse amount_msat: {}", e);
								continue;
							},
						};
					}

					if let Ok(offer) = Offer::from_str(invoice_str) {
						let random_bytes = keys_manager.get_secure_random_bytes();
						let payment_id = PaymentId(random_bytes);

						let amt_msat = match (offer.amount(), user_provided_amt) {
							(Some(offer::Amount::Bitcoin { amount_msats }), _) => amount_msats,
							(_, Some(amt)) => amt,
							(amt, _) => {
								println!("ERROR: Cannot process non-Bitcoin-denominated offer value {:?}", amt);
								continue;
							},
						};
						if user_provided_amt.is_some() && user_provided_amt != Some(amt_msat) {
							println!("Amount didn't match offer of {}msat", amt_msat);
							continue;
						}

						while user_provided_amt.is_none() {
							print!("Paying offer for {} msat. Continue (Y/N)? >", amt_msat);
							std::io::stdout().flush().unwrap();

							let line = match input.next_line().await {
								Ok(Some(l)) => l,
								Err(e) => {
									println!("ERROR: {}", e);
									break 'read_command;
								},
								Ok(None) => {
									println!("ERROR: End of stdin");
									break 'read_command;
								},
							};

							if line.starts_with("Y") {
								break;
							}
							if line.starts_with("N") {
								continue 'read_command;
							}
						}

						outbound_payments.lock().unwrap().payments.insert(
							payment_id,
							PaymentInfo {
								preimage: None,
								secret: None,
								status: HTLCStatus::Pending,
								amt_msat: MillisatAmount(Some(amt_msat)),
							},
						);
						fs_store
							.write("", "", OUTBOUND_PAYMENTS_FNAME, outbound_payments.encode())
							.await
							.unwrap();

						let params = OptionalOfferPaymentParams {
							retry_strategy: Retry::Timeout(Duration::from_secs(10)),
							..Default::default()
						};
						let amt = Some(amt_msat);
						let pay = channel_manager.pay_for_offer(&offer, amt, payment_id, params);
						if pay.is_ok() {
							println!("Payment in flight");
						} else {
							println!("ERROR: Failed to pay: {:?}", pay);
						}
					} else if let Ok(hrn) = HumanReadableName::from_encoded(invoice_str) {
						let random_bytes = keys_manager.get_secure_random_bytes();
						let payment_id = PaymentId(random_bytes);

						if user_provided_amt.is_none() {
							println!("Can't pay to a human-readable-name without an amount");
							continue;
						}

						// We need some nodes that will resolve DNS for us in order to pay a Human
						// Readable Name. They don't need to be trusted, but until onion message
						// forwarding is widespread we'll directly connect to them, revealing who
						// we intend to pay.
						let mut dns_resolvers = Vec::new();
						for (node_id, node) in network_graph.read_only().nodes().unordered_iter() {
							if let Some(info) = &node.announcement_info {
								// Sadly, 31 nodes currently squat on the DNS Resolver feature bit
								// without speaking it.
								// Its unclear why they're doing so, but none of them currently
								// also have the onion messaging feature bit set, so here we check
								// for both.
								let supports_dns = info.features().supports_dns_resolution();
								let supports_om = info.features().supports_onion_messages();
								if supports_dns && supports_om {
									if let Ok(pubkey) = node_id.as_pubkey() {
										dns_resolvers.push(Destination::Node(pubkey));
									}
								}
							}
							if dns_resolvers.len() > 5 {
								break;
							}
						}
						if dns_resolvers.is_empty() {
							println!(
								"Failed to find any DNS resolving nodes, check your network graph is synced"
							);
							continue;
						}

						let amt_msat = user_provided_amt.unwrap();
						outbound_payments.lock().unwrap().payments.insert(
							payment_id,
							PaymentInfo {
								preimage: None,
								secret: None,
								status: HTLCStatus::Pending,
								amt_msat: MillisatAmount(Some(amt_msat)),
							},
						);
						fs_store
							.write("", "", OUTBOUND_PAYMENTS_FNAME, outbound_payments.encode())
							.await
							.unwrap();

						let params = OptionalOfferPaymentParams {
							retry_strategy: Retry::Timeout(Duration::from_secs(10)),
							..Default::default()
						};
						let pay = |a, b, c, d, e| {
							channel_manager.pay_for_offer_from_human_readable_name(a, b, c, d, e)
						};
						let pay = pay(hrn, amt_msat, payment_id, params, dns_resolvers);
						if pay.is_ok() {
							println!("Payment in flight");
						} else {
							println!("ERROR: Failed to pay");
						}
					} else {
						match Bolt11Invoice::from_str(invoice_str) {
							Ok(invoice) => {
								send_payment(
									&channel_manager,
									&invoice,
									user_provided_amt,
									&outbound_payments,
									&*fs_store,
								)
								.await
							},
							Err(e) => {
								println!("ERROR: invalid invoice: {:?}", e);
							},
						}
					}
				},
				"keysend" => {
					let dest_pubkey = match words.next() {
						Some(dest) => match hex_utils::to_compressed_pubkey(dest) {
							Some(pk) => pk,
							None => {
								println!("ERROR: couldn't parse destination pubkey");
								continue;
							},
						},
						None => {
							println!("ERROR: keysend requires a destination pubkey: `keysend <dest_pubkey> <amt_msat>`");
							continue;
						},
					};
					let amt_msat_str = match words.next() {
						Some(amt) => amt,
						None => {
							println!("ERROR: keysend requires an amount in millisatoshis: `keysend <dest_pubkey> <amt_msat>`");
							continue;
						},
					};
					let amt_msat: u64 = match amt_msat_str.parse() {
						Ok(amt) => amt,
						Err(e) => {
							println!("ERROR: couldn't parse amount_msat: {}", e);
							continue;
						},
					};
					keysend(
						&channel_manager,
						dest_pubkey,
						amt_msat,
						&*keys_manager,
						&outbound_payments,
						&*fs_store,
					)
					.await;
				},
				"getoffer" => {
					let offer_builder = channel_manager.create_offer_builder();
					if let Err(e) = offer_builder {
						println!("ERROR: Failed to initiate offer building: {:?}", e);
						continue;
					}

					let amt_str = words.next();
					let offer = if amt_str.is_some() {
						let amt_msat: Result<u64, _> = amt_str.unwrap().parse();
						if amt_msat.is_err() {
							println!("ERROR: getoffer provided payment amount was not a number");
							continue;
						}
						offer_builder.unwrap().amount_msats(amt_msat.unwrap()).build()
					} else {
						offer_builder.unwrap().build()
					};

					if offer.is_err() {
						println!("ERROR: Failed to build offer: {:?}", offer.unwrap_err());
					} else {
						// Note that unlike BOLT11 invoice creation we don't bother to add a
						// pending inbound payment here, as offers can be reused and don't
						// correspond with individual payments.
						println!("{}", offer.unwrap());
					}
				},
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

					let write_future = {
						let mut inbound_payments = inbound_payments.lock().unwrap();
						get_invoice(
							amt_msat.unwrap(),
							&mut inbound_payments,
							&channel_manager,
							expiry_secs.unwrap(),
						);
						fs_store.write("", "", INBOUND_PAYMENTS_FNAME, inbound_payments.encode())
					};
					write_future.await.unwrap();
				},
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
							},
						};
					if tokio::runtime::Handle::current()
						.block_on(connect_peer_if_necessary(
							pubkey,
							peer_addr,
							peer_manager.clone(),
						))
						.is_ok()
					{
						println!("SUCCESS: connected to peer {}", pubkey);
					}
				},
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
							},
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
				},
				"listchannels" => list_channels(&channel_manager, &network_graph),
				"listpayments" => list_payments(
					&inbound_payments.lock().unwrap(),
					&outbound_payments.lock().unwrap(),
				),
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
						},
					};
					let peer_pubkey = match PublicKey::from_slice(&peer_pubkey_vec) {
						Ok(peer_pubkey) => peer_pubkey,
						Err(_) => {
							println!("ERROR: couldn't parse peer_pubkey");
							continue;
						},
					};

					close_channel(channel_id, peer_pubkey, channel_manager.clone());
				},
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
						},
					};
					let peer_pubkey = match PublicKey::from_slice(&peer_pubkey_vec) {
						Ok(peer_pubkey) => peer_pubkey,
						Err(_) => {
							println!("ERROR: couldn't parse peer_pubkey");
							continue;
						},
					};

					force_close_channel(channel_id, peer_pubkey, channel_manager.clone());
				},
				"nodeinfo" => {
					node_info(&channel_manager, &chain_monitor, &peer_manager, &network_graph)
				},
				"listpeers" => list_peers(peer_manager.clone()),
				"signmessage" => {
					const MSG_STARTPOS: usize = "signmessage".len() + 1;
					if line.trim().as_bytes().len() <= MSG_STARTPOS {
						println!("ERROR: signmsg requires a message");
						continue;
					}
					println!(
						"{:?}",
						lightning::util::message_signing::sign(
							&line.trim().as_bytes()[MSG_STARTPOS..],
							&keys_manager.get_node_secret_key()
						)
					);
				},
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
	println!("      openchannel pubkey@[host:port] <amt_satoshis> [--public] [--with-anchors]");
	println!("      closechannel <channel_id> <peer_pubkey>");
	println!("      forceclosechannel <channel_id> <peer_pubkey>");
	println!("      listchannels");
	println!("\n  Peers:");
	println!("      connectpeer pubkey@host:port");
	println!("      disconnectpeer <peer_pubkey>");
	println!("      listpeers");
	println!("\n  Payments:");
	println!("      sendpayment <invoice|offer|human readable name> [<amount_msat>]");
	println!("      keysend <dest_pubkey> <amt_msats>");
	println!("      listpayments");
	println!("\n  Invoices:");
	println!("      getinvoice <amt_msats> <expiry_secs>");
	println!("      getoffer [<amt_msats>]");
	println!("\n  Other:");
	println!("      signmessage <message>");
	println!("      nodeinfo");
}

fn node_info(
	channel_manager: &Arc<ChannelManager>, chain_monitor: &Arc<ChainMonitor>,
	peer_manager: &Arc<PeerManager>, network_graph: &Arc<NetworkGraph>,
) {
	println!("\t{{");
	println!("\t\t node_pubkey: {}", channel_manager.get_our_node_id());
	let chans = channel_manager.list_channels();
	println!("\t\t num_channels: {}", chans.len());
	println!("\t\t num_usable_channels: {}", chans.iter().filter(|c| c.is_usable).count());
	let balances = chain_monitor.get_claimable_balances(&[]);
	let local_balance_sat = balances.iter().map(|b| b.claimable_amount_satoshis()).sum::<u64>();
	println!("\t\t local_balance_sats: {}", local_balance_sat);
	let close_fees_map = |b| match b {
		&Balance::ClaimableOnChannelClose {
			ref balance_candidates,
			confirmed_balance_candidate_index,
			..
		} => balance_candidates[confirmed_balance_candidate_index].transaction_fee_satoshis,
		_ => 0,
	};
	let close_fees_sats = balances.iter().map(close_fees_map).sum::<u64>();
	println!("\t\t eventual_close_fees_sats: {}", close_fees_sats);
	let pending_payments_map = |b| match b {
		&Balance::MaybeTimeoutClaimableHTLC { amount_satoshis, outbound_payment, .. } => {
			if outbound_payment {
				amount_satoshis
			} else {
				0
			}
		},
		_ => 0,
	};
	let pending_payments = balances.iter().map(pending_payments_map).sum::<u64>();
	println!("\t\t pending_outbound_payments_sats: {}", pending_payments);
	println!("\t\t num_peers: {}", peer_manager.list_peers().len());
	let graph_lock = network_graph.read_only();
	println!("\t\t network_nodes: {}", graph_lock.nodes().len());
	println!("\t\t network_channels: {}", graph_lock.channels().len());
	println!("\t}},");
}

fn list_peers(peer_manager: Arc<PeerManager>) {
	println!("\t{{");
	for peer_details in peer_manager.list_peers() {
		println!("\t\t pubkey: {}", peer_details.counterparty_node_id);
	}
	println!("\t}},");
}

fn list_channels(channel_manager: &Arc<ChannelManager>, network_graph: &Arc<NetworkGraph>) {
	print!("[");
	for chan_info in channel_manager.list_channels() {
		println!("");
		println!("\t{{");
		println!("\t\tchannel_id: {},", chan_info.channel_id);
		if let Some(funding_txo) = chan_info.funding_txo {
			println!("\t\tfunding_txid: {},", funding_txo.txid);
		}

		println!(
			"\t\tpeer_pubkey: {},",
			hex_utils::hex_str(&chan_info.counterparty.node_id.serialize())
		);
		if let Some(node_info) = network_graph
			.read_only()
			.nodes()
			.get(&NodeId::from_pubkey(&chan_info.counterparty.node_id))
		{
			if let Some(announcement) = &node_info.announcement_info {
				println!("\t\tpeer_alias: {}", announcement.alias());
			}
		}

		if let Some(id) = chan_info.short_channel_id {
			println!("\t\tshort_channel_id: {},", id);
		}
		println!("\t\tis_channel_ready: {},", chan_info.is_channel_ready);
		println!("\t\tchannel_value_satoshis: {},", chan_info.channel_value_satoshis);
		println!("\t\toutbound_capacity_msat: {},", chan_info.outbound_capacity_msat);
		if chan_info.is_usable {
			println!("\t\tavailable_balance_for_send_msat: {},", chan_info.outbound_capacity_msat);
			println!("\t\tavailable_balance_for_recv_msat: {},", chan_info.inbound_capacity_msat);
		}
		println!("\t\tchannel_can_send_payments: {},", chan_info.is_usable);
		println!("\t\tpublic: {},", chan_info.is_announced);
		println!("\t}},");
	}
	println!("]");
}

fn list_payments(
	inbound_payments: &InboundPaymentInfoStorage, outbound_payments: &OutboundPaymentInfoStorage,
) {
	print!("[");
	for (payment_hash, payment_info) in &inbound_payments.payments {
		println!("");
		println!("\t{{");
		println!("\t\tamount_millisatoshis: {},", payment_info.amt_msat);
		println!("\t\tpayment_hash: {},", payment_hash);
		println!("\t\thtlc_direction: inbound,");
		println!(
			"\t\thtlc_status: {},",
			match payment_info.status {
				HTLCStatus::Pending => "pending",
				HTLCStatus::Succeeded => "succeeded",
				HTLCStatus::Failed => "failed",
			}
		);

		println!("\t}},");
	}

	for (payment_hash, payment_info) in &outbound_payments.payments {
		println!("");
		println!("\t{{");
		println!("\t\tamount_millisatoshis: {},", payment_info.amt_msat);
		println!("\t\tpayment_hash: {},", payment_hash);
		println!("\t\thtlc_direction: outbound,");
		println!(
			"\t\thtlc_status: {},",
			match payment_info.status {
				HTLCStatus::Pending => "pending",
				HTLCStatus::Succeeded => "succeeded",
				HTLCStatus::Failed => "failed",
			}
		);

		println!("\t}},");
	}
	println!("]");
}

pub(crate) async fn connect_peer_if_necessary(
	pubkey: PublicKey, peer_addr: SocketAddr, peer_manager: Arc<PeerManager>,
) -> Result<(), ()> {
	if peer_manager.peer_by_node_id(&pubkey).is_some() {
		return Ok(());
	}
	let res = do_connect_peer(pubkey, peer_addr, peer_manager).await;
	if res.is_err() {
		println!("ERROR: failed to connect to peer");
	}
	res
}

pub(crate) async fn do_connect_peer(
	pubkey: PublicKey, peer_addr: SocketAddr, peer_manager: Arc<PeerManager>,
) -> Result<(), ()> {
	match lightning_net_tokio::connect_outbound(Arc::clone(&peer_manager), pubkey, peer_addr).await
	{
		Some(connection_closed_future) => {
			let mut connection_closed_future = Box::pin(connection_closed_future);
			loop {
				tokio::select! {
					_ = &mut connection_closed_future => return Err(()),
					_ = tokio::time::sleep(Duration::from_millis(10)) => {},
				};
				if peer_manager.peer_by_node_id(&pubkey).is_some() {
					return Ok(());
				}
			}
		},
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
			println!("Error: Node has an active channel with this peer, close any channels first");
			return Err(());
		}
	}

	//check the pubkey matches a valid connected peer
	if peer_manager.peer_by_node_id(&pubkey).is_none() {
		println!("Error: Could not find peer {}", pubkey);
		return Err(());
	}

	peer_manager.disconnect_by_node_id(pubkey);
	Ok(())
}

fn open_channel(
	peer_pubkey: PublicKey, channel_amt_sat: u64, announce_for_forwarding: bool,
	with_anchors: bool, channel_manager: Arc<ChannelManager>,
) -> Result<(), ()> {
	let config = UserConfig {
		channel_handshake_limits: ChannelHandshakeLimits {
			// lnd's max to_self_delay is 2016, so we want to be compatible.
			their_to_self_delay: 2016,
			..Default::default()
		},
		channel_handshake_config: ChannelHandshakeConfig {
			announce_for_forwarding,
			negotiate_anchors_zero_fee_htlc_tx: with_anchors,
			..Default::default()
		},
		..Default::default()
	};

	match channel_manager.create_channel(peer_pubkey, channel_amt_sat, 0, 0, None, Some(config)) {
		Ok(_) => {
			println!("EVENT: initiated channel with peer {}. ", peer_pubkey);
			return Ok(());
		},
		Err(e) => {
			println!("ERROR: failed to open channel: {:?}", e);
			return Err(());
		},
	}
}

async fn send_payment(
	channel_manager: &ChannelManager, invoice: &Bolt11Invoice, required_amount_msat: Option<u64>,
	outbound_payments: &Mutex<OutboundPaymentInfoStorage>, fs_store: &FilesystemStore,
) {
	let payment_id = PaymentId((*invoice.payment_hash()).to_byte_array());
	let payment_secret = Some(*invoice.payment_secret());
	let amt_msat = match (invoice.amount_milli_satoshis(), required_amount_msat) {
		// pay_for_bolt11_invoice only validates that the amount we pay is >= the invoice's
		// required amount, not that its equal (to allow for overpayment). As that is somewhat
		// surprising, here we check and reject all disagreements in amount.
		(Some(inv_amt), Some(req_amt)) if inv_amt != req_amt => {
			println!(
				"Amount didn't match invoice value of {}msat",
				invoice.amount_milli_satoshis().unwrap_or(0)
			);
			print!("> ");
			return;
		},
		(Some(inv_amt), _) => inv_amt,
		(_, Some(req_amt)) => req_amt,
		(None, None) => {
			println!("Need an amount to pay an amountless invoice");
			print!("> ");
			return;
		},
	};
	let write_future = {
		let mut outbound_payments = outbound_payments.lock().unwrap();
		outbound_payments.payments.insert(
			payment_id,
			PaymentInfo {
				preimage: None,
				secret: payment_secret,
				status: HTLCStatus::Pending,
				amt_msat: MillisatAmount(Some(amt_msat)),
			},
		);
		fs_store.write("", "", OUTBOUND_PAYMENTS_FNAME, outbound_payments.encode())
	};
	write_future.await.unwrap();

	match channel_manager.pay_for_bolt11_invoice(
		invoice,
		payment_id,
		required_amount_msat,
		RouteParametersConfig::default(),
		Retry::Timeout(Duration::from_secs(10)),
	) {
		Ok(_) => {
			let payee_pubkey = invoice.recover_payee_pub_key();
			println!("EVENT: initiated sending {} msats to {}", amt_msat, payee_pubkey);
			print!("> ");
		},
		Err(e) => {
			println!("ERROR: failed to send payment: {:?}", e);
			print!("> ");
			let write_future = {
				let mut outbound_payments = outbound_payments.lock().unwrap();
				outbound_payments.payments.get_mut(&payment_id).unwrap().status =
					HTLCStatus::Failed;
				fs_store.write("", "", OUTBOUND_PAYMENTS_FNAME, outbound_payments.encode())
			};
			write_future.await.unwrap();
		},
	};
}

async fn keysend<E: EntropySource>(
	channel_manager: &ChannelManager, payee_pubkey: PublicKey, amt_msat: u64, entropy_source: &E,
	outbound_payments: &Mutex<OutboundPaymentInfoStorage>, fs_store: &FilesystemStore,
) {
	let payment_preimage = PaymentPreimage(entropy_source.get_secure_random_bytes());
	let payment_id = PaymentId(Sha256::hash(&payment_preimage.0[..]).to_byte_array());

	let route_params = RouteParameters::from_payment_params_and_value(
		PaymentParameters::for_keysend(payee_pubkey, 40, false),
		amt_msat,
	);
	let write_future = {
		let mut outbound_payments = outbound_payments.lock().unwrap();
		outbound_payments.payments.insert(
			payment_id,
			PaymentInfo {
				preimage: None,
				secret: None,
				status: HTLCStatus::Pending,
				amt_msat: MillisatAmount(Some(amt_msat)),
			},
		);
		fs_store.write("", "", OUTBOUND_PAYMENTS_FNAME, outbound_payments.encode())
	};
	write_future.await.unwrap();
	match channel_manager.send_spontaneous_payment(
		Some(payment_preimage),
		RecipientOnionFields::spontaneous_empty(),
		payment_id,
		route_params,
		Retry::Timeout(Duration::from_secs(10)),
	) {
		Ok(_payment_hash) => {
			println!("EVENT: initiated sending {} msats to {}", amt_msat, payee_pubkey);
			print!("> ");
		},
		Err(e) => {
			println!("ERROR: failed to send payment: {:?}", e);
			print!("> ");
			let write_future = {
				let mut outbound_payments = outbound_payments.lock().unwrap();
				outbound_payments.payments.get_mut(&payment_id).unwrap().status =
					HTLCStatus::Failed;
				fs_store.write("", "", OUTBOUND_PAYMENTS_FNAME, outbound_payments.encode())
			};
			write_future.await.unwrap();
		},
	};
}

fn get_invoice(
	amt_msat: u64, inbound_payments: &mut InboundPaymentInfoStorage,
	channel_manager: &ChannelManager, expiry_secs: u32,
) {
	let mut invoice_params: Bolt11InvoiceParameters = Default::default();
	invoice_params.amount_msats = Some(amt_msat);
	invoice_params.invoice_expiry_delta_secs = Some(expiry_secs);
	let invoice = match channel_manager.create_bolt11_invoice(invoice_params) {
		Ok(inv) => {
			println!("SUCCESS: generated invoice: {}", inv);
			inv
		},
		Err(e) => {
			println!("ERROR: failed to create invoice: {:?}", e);
			return;
		},
	};

	let payment_hash = PaymentHash(invoice.payment_hash().to_byte_array());
	inbound_payments.payments.insert(
		payment_hash,
		PaymentInfo {
			preimage: None,
			secret: Some(invoice.payment_secret().clone()),
			status: HTLCStatus::Pending,
			amt_msat: MillisatAmount(Some(amt_msat)),
		},
	);
}

fn close_channel(
	channel_id: [u8; 32], counterparty_node_id: PublicKey, channel_manager: Arc<ChannelManager>,
) {
	match channel_manager.close_channel(&ChannelId(channel_id), &counterparty_node_id) {
		Ok(()) => println!("EVENT: initiating channel close"),
		Err(e) => println!("ERROR: failed to close channel: {:?}", e),
	}
}

fn force_close_channel(
	channel_id: [u8; 32], counterparty_node_id: PublicKey, channel_manager: Arc<ChannelManager>,
) {
	match channel_manager.force_close_broadcasting_latest_txn(
		&ChannelId(channel_id),
		&counterparty_node_id,
		"Manually force-closed".to_string(),
	) {
		Ok(()) => println!("EVENT: initiating channel force-close"),
		Err(e) => println!("ERROR: failed to force-close channel: {:?}", e),
	}
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
