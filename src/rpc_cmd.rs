// RPCcmd
// 
// RPCresponse
//

use serde_json::{Value, json};

use std::process::exit;

pub(crate) struct RPCBuilder {}

macro_rules! exit_on {
	($msg: expr) => {
		eprintln!($msg);
		exit(1);
	}
}

impl RPCBuilder {
	pub(crate) fn new(cmd: &str, args: Vec<String>) -> Value {
		match cmd {
			"connect" => {
				if args.len() != 3 {  exit_on!("ldk-cli: connect pubkey hostname port (uncomplete)"); }
				json!({
					"cmd": "connect",
					"pubkey": args[0],
					"hostname": args[1],
					"port": args[2],
				})
			},
			"disconnect" => {
				if args.len() != 1 { exit_on!("ldk-cli disconnect pubkey (uncomplete)"); }
				json!({
					"cmd": "disconnect",
					"pubkey": args[0],
				})
			},
			"open" => {
				if args.len() != 3 { exit_on!("ldk-cli open pubkey funding_satoshis push_msat (uncomplete)"); }
				json!({
					"cmd": "open",
					"pubkey": args[0],
					"funding_satoshis": args[1],
					"push_msat": args[2],
				})
			},
			"close" => {
				if args.len() != 2 { exit_on!("ldk-cli close pubkey chan_id"); }
				json!({
					"cmd": "close",
					"pubkey": args[0],
					"chan_id": args[1],
				})
			},
			"force-close" => {
				if args.len() != 2 { exit_on!("ldk-cli force-close pubkey chan_id"); }
				json!({
					"cmd": "force-close",
					"pubkey": args[0],
					"chan_id": args[1],
				})
			},
			"send" => {
				if args.len() != 3 { exit_on!("ldk-cli send pubkey short_id amt"); }
				json!({
					"cmd": "send",
					"pubkey": args[0],
					"short_id": args[1],
					"amt": args[2],
				})
			},
			"invoice" => {
				if args.len() != 1 { exit_on!("ldk-cli invoice amt"); }
				json!({
					"cmd": "invoice",
					"amt": args[0],
				})
			},
			"list" => {
				if args.len() != 1 { exit_on!("ldk-cli list nodes|chans|peers"); }
				json!({
					"cmd": "list",
					"element": args[0],
				})
			},
			_ => {
				exit_on!("ldk-cli: unknown RPC cmd");	
			}
		}
	}
}
