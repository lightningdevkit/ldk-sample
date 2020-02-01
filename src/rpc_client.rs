use base64;
use hyper;
use serde_json;

use bitcoin_hashes::sha256d::Hash as Sha256dHash;
use bitcoin_hashes::hex::FromHex;

use bitcoin::blockdata::block::BlockHeader;

use futures_util::future::TryFutureExt;

use hyper::body::HttpBody;

use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Deserialize)]
pub struct GetHeaderResponse {
	pub hash: String,
	pub confirmations: u64,
	pub height: u32,
	pub version: u32,
	pub merkleroot: String,
	pub time: u32,
	pub nonce: u32,
	pub bits: String,
	pub previousblockhash: String,
}

impl GetHeaderResponse {
	pub fn to_block_header(&self) -> BlockHeader {
		BlockHeader {
			version: self.version,
			prev_blockhash: Sha256dHash::from_hex(&self.previousblockhash).unwrap(),
			merkle_root: Sha256dHash::from_hex(&self.merkleroot).unwrap(),
			time: self.time,
			bits: self.bits.parse().unwrap(),
			nonce: self.nonce,
		}
	}
}

pub struct RPCClient {
	basic_auth: String,
	uri: String,
	id: AtomicUsize,
	client: hyper::Client<hyper::client::HttpConnector, hyper::Body>,
}

impl RPCClient {
	pub fn new(user_auth: &str, host_port: &str) -> Self {
		Self {
			basic_auth: "Basic ".to_string() + &base64::encode(user_auth),
			uri: "http://".to_string() + host_port,
			id: AtomicUsize::new(0),
			client: hyper::Client::new(),
		}
	}

	/// params entries must be pre-quoted if appropriate
	/// may_fail is only used to change logging
	pub async fn make_rpc_call(&self, method: &str, params: &[&str], may_fail: bool) -> Result<serde_json::Value, ()> {
		let auth: &str = &self.basic_auth;
		let request = hyper::Request::post(&self.uri).header("Authorization", auth);
		let mut param_str = String::new();
		for (idx, param) in params.iter().enumerate() {
			param_str += param;
			if idx != params.len() - 1 {
				param_str += ",";
			}
		}
		if let Ok(res) = self.client.request(request.body(hyper::Body::from("{\"method\":\"".to_string() + method + "\",\"params\":[" + &param_str + "],\"id\":" + &self.id.fetch_add(1, Ordering::AcqRel).to_string() + "}")).unwrap()).map_err(|_| {
			println!("Failed to connect to RPC server!");
			()
		}).await {
			if res.status() != hyper::StatusCode::OK {
				if !may_fail {
					println!("Failed to get RPC server response (probably bad auth)!");
				}
				Err(())
			} else {
				if let Some(Ok(body)) = res.into_body().data().await {
					let v: serde_json::Value = match serde_json::from_slice(&body[..]) {
						Ok(v) => v,
						Err(_) => {
							println!("Failed to parse RPC server response!");
							return Err(());
						},
					};
					if !v.is_object() {
						println!("Failed to parse RPC server response!");
						return Err(());
					}
					let v_obj = v.as_object().unwrap();
					if v_obj.get("error") != Some(&serde_json::Value::Null) {
						println!("Failed to parse RPC server response!");
						return Err(());
					}
					if let Some(res) = v_obj.get("result") {
						Ok((*res).clone())
					} else {
						println!("Failed to parse RPC server response!");
						Err(())
					}
				} else {
					println!("Failed to load RPC server response!");
					Err(())
				}
			}
		} else { Err(()) }
	}

	pub async fn get_header(&self, header_hash: &str) -> Result<GetHeaderResponse, ()> {
		let param = "\"".to_string() + header_hash + "\"";
		let res = self.make_rpc_call("getblockheader", &[&param], false).await;
		if let Ok(mut v) = res {
			if v.is_object() {
				if let None = v.get("previousblockhash") {
					// Got a request for genesis block, add a dummy previousblockhash
					v.as_object_mut().unwrap().insert("previousblockhash".to_string(), serde_json::Value::String("".to_string()));
				}
			}
			let deser_res: Result<GetHeaderResponse, _> = serde_json::from_value(v);
			match deser_res {
				Ok(resp) => Ok(resp),
				Err(_) => {
					println!("Got invalid header message from RPC server!");
					Err(())
				},
			}
		} else { Err(()) }
	}
}
