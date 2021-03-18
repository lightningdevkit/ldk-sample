use bitcoin::network::constants::Network;
use bitcoin::hashes::Hash;
use bitcoin::hashes::sha256::Hash as Sha256Hash;
use bitcoin::secp256k1::Secp256k1;
use bitcoin::secp256k1::key::{PublicKey, SecretKey};
use crate::{ChannelManager, FilesystemLogger, HTLCDirection, HTLCStatus,
            PaymentInfoStorage, PeerManager, SatoshiAmount};
use crate::disk;
use crate::hex_utils;
use lightning::chain;
use lightning::ln::channelmanager::{PaymentHash, PaymentPreimage, PaymentSecret};
use lightning::ln::features::InvoiceFeatures;
use lightning::routing::network_graph::NetGraphMsgHandler;
use lightning::routing::router;
use lightning::util::config::UserConfig;
use rand;
use rand::Rng;
use std::env;
use std::io;
use std::io::{BufRead, Write};
use std::net::{SocketAddr, TcpStream};
use std::ops::Deref;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::mpsc;

pub(crate) struct LdkUserInfo {
    pub(crate) bitcoind_rpc_username: String,
    pub(crate) bitcoind_rpc_password: String,
    pub(crate) bitcoind_rpc_port: u16,
    pub(crate) bitcoind_rpc_host: String,
    pub(crate) ldk_storage_dir_path: String,
    pub(crate) ldk_peer_listening_port: u16,
    pub(crate) network: Network,
}

pub(crate) fn parse_startup_args() -> Result<LdkUserInfo, ()> {
    if env::args().len() < 4 {
        println!("ldk-tutorial-node requires 3 arguments: `cargo run <bitcoind-rpc-username>:<bitcoind-rpc-password>@<bitcoind-rpc-host>:<bitcoind-rpc-port> ldk_storage_directory_path [<ldk-incoming-peer-listening-port>] [bitcoin-network]`");
        return Err(())
    }
    let bitcoind_rpc_info = env::args().skip(1).next().unwrap();
    let bitcoind_rpc_info_parts: Vec<&str> = bitcoind_rpc_info.split("@").collect();
    if bitcoind_rpc_info_parts.len() != 2 {
        println!("ERROR: bad bitcoind RPC URL provided");
        return Err(())
    }
    let rpc_user_and_password: Vec<&str> = bitcoind_rpc_info_parts[0].split(":").collect();
    if rpc_user_and_password.len() != 2 {
        println!("ERROR: bad bitcoind RPC username/password combo provided");
        return Err(())
    }
    let bitcoind_rpc_username = rpc_user_and_password[0].to_string();
    let bitcoind_rpc_password = rpc_user_and_password[1].to_string();
    let bitcoind_rpc_path: Vec<&str> = bitcoind_rpc_info_parts[1].split(":").collect();
    if bitcoind_rpc_path.len() != 2 {
        println!("ERROR: bad bitcoind RPC path provided");
        return Err(())
    }
    let bitcoind_rpc_host = bitcoind_rpc_path[0].to_string();
    let bitcoind_rpc_port = bitcoind_rpc_path[1].parse::<u16>().unwrap();

    let ldk_storage_dir_path = env::args().skip(2).next().unwrap();

    let mut ldk_peer_port_set = true;
    let ldk_peer_listening_port: u16 = match env::args().skip(3).next().map(|p| p.parse()) {
        Some(Ok(p)) => p,
        Some(Err(e)) => panic!(e),
        None => {
            ldk_peer_port_set = false;
            9735
        },
    };

    let arg_idx = match ldk_peer_port_set {
        true => 4,
        false => 3,
    };
    let network: Network = match env::args().skip(arg_idx).next().as_ref().map(String::as_str) {
        Some("testnet") => Network::Testnet,
        Some("regtest") => Network::Regtest,
        Some(_) => panic!("Unsupported network provided. Options are: `regtest`, `testnet`"),
        None => Network::Testnet
    };
    Ok(LdkUserInfo {
        bitcoind_rpc_username,
        bitcoind_rpc_password,
        bitcoind_rpc_host,
        bitcoind_rpc_port,
        ldk_storage_dir_path,
        ldk_peer_listening_port,
        network,
    })
}

pub(crate) fn poll_for_user_input(peer_manager: Arc<PeerManager>, channel_manager:
                                  Arc<ChannelManager>, router: Arc<NetGraphMsgHandler<Arc<dyn
                                  chain::Access>, Arc<FilesystemLogger>>>, payment_storage:
                                  PaymentInfoStorage, node_privkey: SecretKey, event_notifier:
                                  mpsc::Sender<()>, ldk_data_dir: String, logger: Arc<FilesystemLogger>,
                                  runtime_handle: Handle, network: Network) {
    println!("LDK startup successful. To view available commands: \"help\".\nLDK logs are available at <your-supplied-ldk-data-dir-path>/.ldk/logs");
    let stdin = io::stdin();
    print!("> "); io::stdout().flush().unwrap(); // Without flushing, the `>` doesn't print
    for line in stdin.lock().lines() {
        let _ = event_notifier.try_send(());
        let line = line.unwrap();
        let mut words = line.split_whitespace();
        if let Some(word) = words.next() {
            match word {
                "help" => help(),
                "openchannel" => {
                    let peer_pubkey_and_ip_addr = words.next();
                    let channel_value_sat = words.next();
                    if peer_pubkey_and_ip_addr.is_none() || channel_value_sat.is_none() {
                        println!("ERROR: openchannel takes 2 arguments: `openchannel pubkey@host:port channel_amt_satoshis`");
                        print!("> "); io::stdout().flush().unwrap(); continue;
                    }
                    let peer_pubkey_and_ip_addr = peer_pubkey_and_ip_addr.unwrap();
                    let (pubkey, peer_addr) = match parse_peer_info(peer_pubkey_and_ip_addr.to_string()) {
                        Ok(info) => info,
                        Err(e) => {
                            println!("{:?}", e.into_inner().unwrap());
                            print!("> "); io::stdout().flush().unwrap(); continue;
                        }
                    };

                    let chan_amt_sat: Result<u64, _> = channel_value_sat.unwrap().parse();
                    if chan_amt_sat.is_err() {
                        println!("ERROR: channel amount must be a number");
                        print!("> "); io::stdout().flush().unwrap(); continue;
                    }

                    if connect_peer_if_necessary(pubkey, peer_addr, peer_manager.clone(),
                                                 event_notifier.clone(),
                                                 runtime_handle.clone()).is_err() {
                        print!("> "); io::stdout().flush().unwrap(); continue;
                    };

                    if open_channel(pubkey, chan_amt_sat.unwrap(), channel_manager.clone()).is_ok() {
                        let peer_data_path = format!("{}/channel_peer_data", ldk_data_dir.clone());
                        let _ = disk::persist_channel_peer(Path::new(&peer_data_path), peer_pubkey_and_ip_addr);
                    }
                },
                "sendpayment" => {
                    let invoice_str = words.next();
                    if invoice_str.is_none() {
                        println!("ERROR: sendpayment requires an invoice: `sendpayment <invoice>`");
                        print!("> "); io::stdout().flush().unwrap(); continue;
                    }

                    let invoice_res = lightning_invoice::Invoice::from_str(invoice_str.unwrap());
                    if invoice_res.is_err() {
                        println!("ERROR: invalid invoice: {:?}", invoice_res.unwrap_err());
                        print!("> "); io::stdout().flush().unwrap(); continue;
                    }
                    let invoice = invoice_res.unwrap();

                    let amt_pico_btc = invoice.amount_pico_btc();
                    if amt_pico_btc.is_none () {
                        println!("ERROR: invalid invoice: must contain amount to pay");
                        print!("> "); io::stdout().flush().unwrap(); continue;
                    }
                    let amt_msat = amt_pico_btc.unwrap() / 10;

                    let payee_pubkey = invoice.recover_payee_pub_key();
                    let final_cltv = *invoice.min_final_cltv_expiry().unwrap_or(&9) as u32;

                    let mut payment_hash = PaymentHash([0; 32]);
                    payment_hash.0.copy_from_slice(&invoice.payment_hash().as_ref()[0..32]);


                    let payment_secret = match invoice.payment_secret() {
                        Some(secret) => {
                            let mut payment_secret = PaymentSecret([0; 32]);
                            payment_secret.0.copy_from_slice(&secret.0);
                            Some(payment_secret)
                        },
                        None => None
                    };

                    // rust-lightning-invoice doesn't currently support features, so we parse features
                    // manually from the invoice.
                    let mut invoice_features = InvoiceFeatures::empty();
                    for field in &invoice.into_signed_raw().raw_invoice().data.tagged_fields {
                        match field {
                            lightning_invoice::RawTaggedField::UnknownSemantics(vec) => {
                                if vec[0] == bech32::u5::try_from_u8(5).unwrap() {
                                    if vec.len() >= 6 && vec[5].to_u8() & 0b10000 != 0 {
                                        invoice_features = invoice_features.set_variable_length_onion_optional();
                                    }
                                    if vec.len() >= 6 && vec[5].to_u8() & 0b01000 != 0 {
                                        invoice_features = invoice_features.set_variable_length_onion_required();
                                    }
                                    if vec.len() >= 4 && vec[3].to_u8() & 0b00001 != 0 {
                                        invoice_features = invoice_features.set_payment_secret_optional();
                                    }
                                    if vec.len() >= 5 && vec[4].to_u8() & 0b10000 != 0 {
                                        invoice_features = invoice_features.set_payment_secret_required();
                                    }
                                    if vec.len() >= 4 && vec[3].to_u8() & 0b00100 != 0 {
                                        invoice_features = invoice_features.set_basic_mpp_optional();
                                    }
                                    if vec.len() >= 4 && vec[3].to_u8() & 0b00010 != 0 {
                                        invoice_features = invoice_features.set_basic_mpp_required();
                                    }
                                }
                            },
                            _ => {}
                        }
                    }
                    let invoice_features_opt = match invoice_features == InvoiceFeatures::empty() {
                        true => None,
                        false => Some(invoice_features)
                    };
                    send_payment(payee_pubkey, amt_msat, final_cltv, payment_hash,
                                        payment_secret, invoice_features_opt, router.clone(),
                                        channel_manager.clone(), payment_storage.clone(),
                                        logger.clone());
                },
                "getinvoice" => {
                    let amt_str = words.next();
                    if amt_str.is_none() {
                        println!("ERROR: getinvoice requires an amount in satoshis");
                        print!("> "); io::stdout().flush().unwrap(); continue;
                    }

                    let amt_sat: Result<u64, _> = amt_str.unwrap().parse();
                    if amt_sat.is_err() {
                        println!("ERROR: getinvoice provided payment amount was not a number");
                        print!("> "); io::stdout().flush().unwrap(); continue;
                    }
                    get_invoice(amt_sat.unwrap(), payment_storage.clone(), node_privkey.clone(),
                                channel_manager.clone(), network);
                },
                "connectpeer" => {
                    let peer_pubkey_and_ip_addr = words.next();
                    if peer_pubkey_and_ip_addr.is_none() {
                        println!("ERROR: connectpeer requires peer connection info: `connectpeer pubkey@host:port`");
                        print!("> "); io::stdout().flush().unwrap(); continue;
                    }
                    let (pubkey, peer_addr) = match parse_peer_info(peer_pubkey_and_ip_addr.unwrap().to_string()) {
                        Ok(info) => info,
                        Err(e) => {
                            println!("{:?}", e.into_inner().unwrap());
                            print!("> "); io::stdout().flush().unwrap(); continue;
                        }
                    };
                    if connect_peer_if_necessary(pubkey, peer_addr, peer_manager.clone(),
                                                 event_notifier.clone(), runtime_handle.clone()).is_ok() {
                        println!("SUCCESS: connected to peer {}", pubkey);
                    }

                },
                "listchannels" => list_channels(channel_manager.clone()),
                "listpayments" => list_payments(payment_storage.clone()),
                "closechannel" => {
                    let channel_id_str = words.next();
                    if channel_id_str.is_none() {
                        println!("ERROR: closechannel requires a channel ID: `closechannel <channel_id>`");
                        print!("> "); io::stdout().flush().unwrap(); continue;
                    }
                    let channel_id_vec = hex_utils::to_vec(channel_id_str.unwrap());
                    if channel_id_vec.is_none() {
                        println!("ERROR: couldn't parse channel_id as hex");
                        print!("> "); io::stdout().flush().unwrap(); continue;
                    }
                    let mut channel_id = [0; 32];
                    channel_id.copy_from_slice(&channel_id_vec.unwrap());
                    close_channel(channel_id, channel_manager.clone());
                },
                "forceclosechannel" => {
                    let channel_id_str = words.next();
                    if channel_id_str.is_none() {
                        println!("ERROR: forceclosechannel requires a channel ID: `forceclosechannel <channel_id>`");
                        print!("> "); io::stdout().flush().unwrap(); continue;
                    }
                    let channel_id_vec = hex_utils::to_vec(channel_id_str.unwrap());
                    if channel_id_vec.is_none() {
                        println!("ERROR: couldn't parse channel_id as hex");
                        print!("> "); io::stdout().flush().unwrap(); continue;
                    }
                    let mut channel_id = [0; 32];
                    channel_id.copy_from_slice(&channel_id_vec.unwrap());
                    force_close_channel(channel_id, channel_manager.clone());
                }
                _ => println!("Unknown command. See `\"help\" for available commands.")
            }
        }
        print!("> "); io::stdout().flush().unwrap();

    }
}

fn help() {
    println!("openchannel pubkey@host:port <channel_amt_satoshis>");
    println!("sendpayment <invoice>");
    println!("getinvoice <amt_in_satoshis>");
    println!("connectpeer pubkey@host:port");
    println!("listchannels");
    println!("listpayments");
    println!("closechannel <channel_id>");
    println!("forceclosechannel <channel_id>");
}

fn list_channels(channel_manager: Arc<ChannelManager>) {
    print!("[");
    for chan_info in channel_manager.list_channels() {
        println!("");
        println!("\t{{");
        println!("\t\tchannel_id: {},", hex_utils::hex_str(&chan_info.channel_id[..]));
        println!("\t\tpeer_pubkey: {},", hex_utils::hex_str(&chan_info.remote_network_id.serialize()));
        let mut pending_channel = false;
        match chan_info.short_channel_id {
            Some(id) => println!("\t\tshort_channel_id: {},", id),
            None => {
                pending_channel = true;
            }
        }
        println!("\t\tpending_open: {},", pending_channel);
        println!("\t\tchannel_value_satoshis: {},", chan_info.channel_value_satoshis);
        println!("\t\tchannel_can_send_payments: {},", chan_info.is_live);
        println!("\t}},");
    }
    println!("]");
}

fn list_payments(payment_storage: PaymentInfoStorage) {
    let payments = payment_storage.lock().unwrap();
    print!("[");
    for (payment_hash, payment_info) in payments.deref() {
        let direction_str = match payment_info.1 {
            HTLCDirection::Inbound => "inbound",
            HTLCDirection::Outbound => "outbound",
        };
        println!("");
        println!("\t{{");
        println!("\t\tamount_satoshis: {},", payment_info.3);
        println!("\t\tpayment_hash: {},", hex_utils::hex_str(&payment_hash.0));
        println!("\t\thtlc_direction: {},", direction_str);
        println!("\t\thtlc_status: {},", match payment_info.2 {
            HTLCStatus::Pending => "pending",
            HTLCStatus::Succeeded => "succeeded",
            HTLCStatus::Failed => "failed"
        });


        println!("\t}},");
    }
    println!("]");
}

pub(crate) fn connect_peer_if_necessary(pubkey: PublicKey, peer_addr: SocketAddr, peer_manager:
                             Arc<PeerManager>, event_notifier: mpsc::Sender<()>, runtime: Handle) ->
                             Result<(), ()> {
    for node_pubkey in peer_manager.get_peer_node_ids() {
        if node_pubkey == pubkey {
            return Ok(())
        }
    }
    match TcpStream::connect_timeout(&peer_addr, Duration::from_secs(10)) {
        Ok(stream) => {
            let peer_mgr = peer_manager.clone();
            let event_ntfns = event_notifier.clone();
            runtime.spawn(async move {
                lightning_net_tokio::setup_outbound(peer_mgr, event_ntfns, pubkey, stream).await;
            });
            let mut peer_connected = false;
            while !peer_connected {
                for node_pubkey in peer_manager.get_peer_node_ids() {
                    if node_pubkey == pubkey { peer_connected = true; }
                }
            }
        },
        Err(e) => {println!("ERROR: failed to connect to peer: {:?}", e);
            return Err(())
        }
    }
    Ok(())
}

fn open_channel(peer_pubkey: PublicKey, channel_amt_sat: u64, channel_manager: Arc<ChannelManager>)
                       -> Result<(), ()> {
    let mut config = UserConfig::default();
    // lnd's max to_self_delay is 2016, so we want to be compatible.
    config.peer_channel_config_limits.their_to_self_delay = 2016;
    match channel_manager.create_channel(peer_pubkey, channel_amt_sat, 0, 0, None) {
        Ok(_) => {
            println!("EVENT: initiated channel with peer {}. ", peer_pubkey);
            return Ok(())
        },
        Err(e) => {
            println!("ERROR: failed to open channel: {:?}", e);
            return Err(())
        }
    }
}

fn send_payment(payee: PublicKey, amt_msat: u64, final_cltv: u32, payment_hash: PaymentHash,
                payment_secret: Option<PaymentSecret>, payee_features: Option<InvoiceFeatures>,
                router: Arc<NetGraphMsgHandler<Arc<dyn chain::Access>, Arc<FilesystemLogger>>>,
                channel_manager: Arc<ChannelManager>, payment_storage: PaymentInfoStorage, logger:
                Arc<FilesystemLogger>) {
    let network_graph = router.network_graph.read().unwrap();
    let first_hops = channel_manager.list_usable_channels();
    let payer_pubkey = channel_manager.get_our_node_id();

    let route = router::get_route(&payer_pubkey, &network_graph, &payee, payee_features,
                                 Some(&first_hops.iter().collect::<Vec<_>>()), &vec![], amt_msat,
                                 final_cltv, logger);
    if let Err(e) = route {
        println!("ERROR: failed to find route: {}", e.err);
        return
    }
    let status = match channel_manager.send_payment(&route.unwrap(), payment_hash, &payment_secret) {
        Ok(()) => {
            println!("EVENT: initiated sending {} msats to {}", amt_msat, payee);
            HTLCStatus::Pending
        },
        Err(e) => {
            println!("ERROR: failed to send payment: {:?}", e);
            HTLCStatus::Failed
        }
    };
    let mut payments = payment_storage.lock().unwrap();
    payments.insert(payment_hash, (None, HTLCDirection::Outbound, status,
                                   SatoshiAmount(Some(amt_msat * 1000))));
}

fn get_invoice(amt_sat: u64, payment_storage: PaymentInfoStorage, our_node_privkey: SecretKey,
               channel_manager: Arc<ChannelManager>, network: Network) {
    let mut payments = payment_storage.lock().unwrap();
    let secp_ctx = Secp256k1::new();

    let mut preimage = [0; 32];
		rand::thread_rng().fill_bytes(&mut preimage);
		let payment_hash = Sha256Hash::hash(&preimage);


    let our_node_pubkey = channel_manager.get_our_node_id();
		let mut invoice = lightning_invoice::InvoiceBuilder::new(match network {
				Network::Bitcoin => lightning_invoice::Currency::Bitcoin,
				Network::Testnet => lightning_invoice::Currency::BitcoinTestnet,
				Network::Regtest => lightning_invoice::Currency::Regtest,
        Network::Signet => panic!("Signet invoices not supported")
		})
        .payment_hash(payment_hash).description("rust-lightning-bitcoinrpc invoice".to_string())
				.amount_pico_btc(amt_sat * 10_000)
				.current_timestamp()
        .payee_pub_key(our_node_pubkey);

    // Add route hints to the invoice.
    let our_channels = channel_manager.list_usable_channels();
    for channel in our_channels {
        let short_channel_id = match channel.short_channel_id {
            Some(id) => id.to_be_bytes(),
            None => continue
        };
        let forwarding_info = match channel.counterparty_forwarding_info {
            Some(info) => info,
            None => continue,
        };
        println!("VMW: adding routehop, info.fee base: {}", forwarding_info.fee_base_msat);
        invoice = invoice.route(vec![
            lightning_invoice::RouteHop {
                pubkey: channel.remote_network_id,
                short_channel_id,
                fee_base_msat: forwarding_info.fee_base_msat,
                fee_proportional_millionths: forwarding_info.fee_proportional_millionths,
                cltv_expiry_delta: forwarding_info.cltv_expiry_delta,
            }
        ]);
    }

    // Sign the invoice.
    let invoice = invoice.build_signed(|msg_hash| {
        secp_ctx.sign_recoverable(msg_hash, &our_node_privkey)
		});

		match invoice {
				Ok(invoice) => println!("SUCCESS: generated invoice: {}", invoice),
				Err(e) => println!("ERROR: failed to create invoice: {:?}", e),
		}

    payments.insert(PaymentHash(payment_hash.into_inner()), (Some(PaymentPreimage(preimage)),
                                                             HTLCDirection::Inbound,
                                                             HTLCStatus::Pending,
                                                             SatoshiAmount(Some(amt_sat))));
}

fn close_channel(channel_id: [u8; 32], channel_manager: Arc<ChannelManager>) {
    match channel_manager.close_channel(&channel_id) {
        Ok(()) => println!("EVENT: initiating channel close"),
        Err(e) => println!("ERROR: failed to close channel: {:?}", e)
    }
}

fn force_close_channel(channel_id: [u8; 32], channel_manager: Arc<ChannelManager>) {
    match channel_manager.force_close_channel(&channel_id) {
        Ok(()) => println!("EVENT: initiating channel force-close"),
        Err(e) => println!("ERROR: failed to force-close channel: {:?}", e)
    }
}

pub(crate) fn parse_peer_info(peer_pubkey_and_ip_addr: String) -> Result<(PublicKey, SocketAddr), std::io::Error> {
    let mut pubkey_and_addr = peer_pubkey_and_ip_addr.split("@");
    let pubkey = pubkey_and_addr.next();
    let peer_addr_str = pubkey_and_addr.next();
    if peer_addr_str.is_none() || peer_addr_str.is_none() {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "ERROR: incorrectly formatted peer
        info. Should be formatted as: `pubkey@host:port`"));
    }

    let peer_addr: Result<SocketAddr, _> = peer_addr_str.unwrap().parse();
    if peer_addr.is_err() {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "ERROR: couldn't parse pubkey@host:port into a socket address"));
    }

    let pubkey = hex_utils::to_compressed_pubkey(pubkey.unwrap());
    if pubkey.is_none() {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "ERROR: unable to parse given pubkey for node"));
    }

    Ok((pubkey.unwrap(), peer_addr.unwrap()))
}
