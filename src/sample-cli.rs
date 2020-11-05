/// This file contains the client logic.

mod rpc_cmd;
use rpc_cmd::RPCBuilder;

use ldk::utils::{LogPrinter, RpcClient};

use std::env;
use std::process::exit;
use std::sync::Arc;

fn display_cmds() {
	//println!("'list [graph|channels|peers|invoices]'	List details about given class of elements");
	println!("'connect/disconnect peer_id'		Open or close a connection with the given peer");
	//TODO: htlc/channel/invoices
	exit(0);
}

fn sanitize_cmd(cmd: &str) {

	if cmd.find("list").is_some() 
		|| cmd.find("ping").is_some()
		|| cmd.find("connect").is_some()
		|| cmd.find("disconnect").is_some()
		|| cmd.find("invoice").is_some()
		|| cmd.find("send").is_some()
		|| cmd.find("open").is_some() {
	} else {
		eprintln!("ldk-cli: {} is not a ldk-cli command", cmd);
	}
}

#[tokio::main]
async fn main() {

	// If not argument is provided, display helper and exit.
	if env::args().len() == 1 {
		display_cmds();
	}

	// If an argument is provided, capture and sanitize
	let cmd = env::args().skip(1).next().unwrap();
	let args: Vec<String> = env::args().skip(2).collect::<Vec<String>>();
	sanitize_cmd(&cmd);
	let json_rpc = RPCBuilder::new(&cmd, args);

	let logger = Arc::new(LogPrinter::new("/home/user/.ldk-node/debug.log"));

	let rpc_client = RpcClient::new("", "127.0.0.1:7688");
	rpc_client.send_request(&logger, json_rpc).await;
}
