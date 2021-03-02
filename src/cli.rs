use bitcoin::secp256k1::key::PublicKey;
use crate::{FilesystemLogger, PeerManager, ChannelManager};
use crate::utils;
use std::io;
use std::io::{BufRead, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
// use tokio::runtime::Runtime;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc;

pub fn poll_for_user_input(peer_manager: Arc<PeerManager>, channel_manager: Arc<ChannelManager>, event_notifier: mpsc::Sender<()>) {
    println!("LDK startup successful. To view available commands: \"help\".\nLDK logs are available in the `logs` folder of the current directory.");
    print!("> "); io::stdout().flush().unwrap(); // Without flushing, the `>` doesn't print
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = line.unwrap();
        let mut words = line.split_whitespace();
        if let Some(word) = words.next() {
            match word {
                "help" => handle_help(),
                "openchannel" => {
                    let peer_pubkey_and_ip_addr = words.next();
                    let channel_value_sat = words.next();
                    if peer_pubkey_and_ip_addr.is_none() || channel_value_sat.is_none() {
                        println!("ERROR: openchannel takes 2 arguments: `openchannel pubkey@host:port channel_amt_satoshis`")
                    } else {
                        let peer_pubkey_and_ip_addr = peer_pubkey_and_ip_addr.unwrap();
                        let channel_value_sat = channel_value_sat.unwrap();
                        let mut pubkey_and_addr = peer_pubkey_and_ip_addr.split("@");
                        let pubkey = pubkey_and_addr.next();
                        let peer_addr_str = pubkey_and_addr.next();
                        if peer_addr_str.is_none() || peer_addr_str.is_none() {
                            println!("ERROR: incorrectly formatted peer info. Should be formatted as: `pubkey@host:port`");
                        } else {
                            let pubkey = pubkey.unwrap();
                            let peer_addr_str = peer_addr_str.unwrap();
                            let peer_addr_res: Result<SocketAddr, _> = peer_addr_str.parse();
                            let chan_amt: Result<u64, _> = channel_value_sat.parse();
                            if let Some(pk) = utils::hex_to_compressed_pubkey(pubkey) {
                                if let Ok(addr) = peer_addr_res {
                                    if let Ok(amt) = chan_amt {
                                        handle_open_channel(pk, addr, amt,
                                                            peer_manager.clone(),
                                                            channel_manager.clone(),
                                                            event_notifier.clone());
                                    } else {
                                        println!("ERROR: channel amount must be a number");
                                    }
                                } else {
                                    println!("ERROR: couldn't parse
                                    pubkey@host:port into a socket address");
                                }
                            } else {
                                println!("ERROR: unable to parse given pubkey for node");
                            }
                        }
                    }
                },
                _ => println!("hello")
            }
        }
        print!("> "); io::stdout().flush().unwrap();

    }
}

fn handle_help() {
    println!("")
}

fn handle_open_channel(peer_pubkey: PublicKey, peer_socket: SocketAddr,
                       channel_amt_sat: u64, peer_manager: Arc<PeerManager>,
                       channel_manager: Arc<ChannelManager>, event_notifier:
                       mpsc::Sender<()>)
{
		// let runtime = Runtime::new().expect("Unable to create a runtime").enable_all();
		let runtime = Builder::new_multi_thread()
        .worker_threads(3)
        .enable_all()
        .build()
        .unwrap();
    match TcpStream::connect_timeout(&peer_socket, Duration::from_secs(10)) {
        Ok(stream) => {
            runtime.block_on(|| {
                lightning_net_tokio::setup_outbound(peer_manager,
                                                    event_notifier,
                                                    peer_pubkey,
                                                    stream);
            });
            match channel_manager.create_channel(peer_pubkey, channel_amt_sat, 0, 0, None) {
                Ok(_) => println!("SUCCESS: channel created! Mine some blocks to open it."),
                Err(e) => println!("ERROR: failed to open channel: {:?}", e)
            }
        },
        Err(e) => println!("ERROR: failed to connect to peer: {:?}", e)
    }
}
