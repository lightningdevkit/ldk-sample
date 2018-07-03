use base64;
use hyper;
use serde_json;

use futures::{future, Future, Stream};

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
	pub fn make_rpc_call(&self, method: &str, params: &Vec<&str>) -> impl Future<Item=serde_json::Value, Error=()> {
		let mut request = hyper::Request::post(&self.uri);
		let auth: &str = &self.basic_auth;
		request.header("Authorization", auth);
		let mut param_str = String::new();
		for (idx, param) in params.iter().enumerate() {
			param_str += param;
			if idx != params.len() - 1 {
				param_str += ",";
			}
		}
		self.client.request(request.body(hyper::Body::from("{\"method\":\"".to_string() + method + "\",\"params\":[" + &param_str + "],\"id\":" + &self.id.fetch_add(1, Ordering::AcqRel).to_string() + "}")).unwrap()).map_err(|_| {
			println!("Failed to connect to RPC server!");
			()
		}).and_then(|res| {
			if res.status() != hyper::StatusCode::OK {
				println!("Failed to get RPC server response (probably bad auth)!");
				future::Either::A(future::err(()))
			} else {
				future::Either::B(res.into_body().concat2().map_err(|_| {
					println!("Failed to load RPC server response!");
					()
				}).and_then(|body| {
					let v: serde_json::Value = match serde_json::from_slice(&body) {
						Ok(v) => v,
						Err(_) => {
							println!("Failed to parse RPC server response!");
							return future::err(())
						},
					};
					if !v.is_object() {
						println!("Failed to parse RPC server response!");
						return future::err(());
					}
					let v_obj = v.as_object().unwrap();
					if v_obj.get("error") != Some(&serde_json::Value::Null) {
						println!("Failed to parse RPC server response!");
						return future::err(());
					}
					if let Some(res) = v_obj.get("result") {
						future::result(Ok((*res).clone()))
					} else {
						println!("Failed to parse RPC server response!");
						return future::err(());
					}
				}))
			}
		})
	}
}
