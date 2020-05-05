use base64;
use hyper;
use serde_json;

use futures_util::future::TryFutureExt;
use futures_util::stream::TryStreamExt;

use std::sync::atomic::{AtomicUsize, Ordering};

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
		let req = "{\"method\":\"".to_string() + method + "\",\"params\":[" + &param_str + "],\"id\":" + &self.id.fetch_add(1, Ordering::AcqRel).to_string() + "}";
		if let Ok(res) = self.client.request(request.body(hyper::Body::from(req.clone())).unwrap()).map_err(|e| {
			println!("Failed to connect to RPC server!");
			eprintln!("RPC Gave {} in response to {}", e, req);
			()
		}).await {
			if res.status() != hyper::StatusCode::OK {
				if !may_fail {
					println!("Failed to get RPC server response (probably bad auth)!");
					eprintln!("RPC returned status {} in response to {}", res.status(), req);
				}
				Err(())
			} else {
				if let Ok(body) = res.into_body().map_ok(|b| b.to_vec()).try_concat().await {
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
}
