use lightning::util::logger::{Logger, Record};

use bitcoin::network::constants::Network;
use bitcoin::secp256k1::key::PublicKey;

use futures_util::future::TryFutureExt;
use futures_util::stream::TryStreamExt;

use serde_json::Value;

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::fs::read;
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::str;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::net::TcpStream;
use tokio::prelude::*;

use time::OffsetDateTime;

pub fn hex_to_vec(hex: &str) -> Option<Vec<u8>> {
	let mut out = Vec::with_capacity(hex.len() / 2);

	let mut b = 0;
	for (idx, c) in hex.as_bytes().iter().enumerate() {
		b <<= 4;
		match *c {
			b'A'..=b'F' => b |= c - b'A' + 10,
			b'a'..=b'f' => b |= c - b'a' + 10,
			b'0'..=b'9' => b |= c - b'0',
			_ => return None,
		}
		if (idx & 1) == 1 {
			out.push(b);
			b = 0;
		}
	}

	Some(out)
}

pub fn hex_to_compressed_pubkey(hex: &str) -> Option<PublicKey> {
	let data = match hex_to_vec(&hex[0..33*2]) {
		Some(bytes) => bytes,
		None => return None
	};
	match PublicKey::from_slice(&data) {
		Ok(pk) => Some(pk),
		Err(_) => None,
	}
}

#[inline]
pub fn hex_str(value: &[u8]) -> String {
	let mut res = String::with_capacity(64);
	for v in value {
		res += &format!("{:02x}", v);
	}
	res
}

#[inline]
pub fn slice_to_be64(v: &[u8]) -> u64 {
	((v[0] as u64) << 8*7) |
	((v[1] as u64) << 8*6) |
	((v[2] as u64) << 8*5) |
	((v[3] as u64) << 8*4) |
	((v[4] as u64) << 8*3) |
	((v[5] as u64) << 8*2) |
	((v[6] as u64) << 8*1) |
	((v[7] as u64) << 8*0)
}

pub(super) struct LdkConfig {
	conf_args: HashMap<String, String>,
}

impl LdkConfig {
	pub(super) fn create(config_file: &str) -> Self {
		if let Ok(config_bytes) = read(&config_file) {
			if let Ok(s) = str::from_utf8(&config_bytes) {
				let mut v: Vec<&str> = s.rsplit('\n').collect();
				v.retain(|line| line.contains("="));
				let mut conf_args = HashMap::with_capacity(v.len());
				for lines in v.iter() {
					let entry: Vec<&str> = lines.rsplit("=").collect();
					//TODO: check for forbidden duplicate
					conf_args.insert(entry[1].to_string(), entry[0].to_string());
				}
				return LdkConfig {
					conf_args,
				}
			}
		}
		panic!("ldk-node: error parsing config file {}", config_file);
	}
	pub(super) fn get_str(&self, arg: &str) -> String {
		self.conf_args.get(arg).unwrap().clone()
	}
	pub(super) fn get_network(&self) -> Result<Network, ()> {
		if let Some(net) = self.conf_args.get("network") {		
			if net.contains("regtest") { return Ok(Network::Regtest) }
			if net.contains("testnet") { return Ok(Network::Testnet) }
		}
		Err(())
	}
	pub(super) fn get_ln_port(&self) -> Result<u16, ()> {
		if let Some(hostport) = self.conf_args.get("ln_hostport") {
			let hostport: Vec<&str> = hostport.split(':').collect();
			return Ok(hostport[1].parse::<u16>().unwrap())
		}
		Err(())
	}
	pub(super) fn get_ldk_port(&self) -> Result<u16, ()> {
		if let Some(hostport) = self.conf_args.get("ldk_hostport") {
			let hostport: Vec<&str> = hostport.split(':').collect();
			return Ok(hostport[1].parse::<u16>().unwrap())
		}
		Err(())
	}
}

#[macro_export]
macro_rules! log_sample {
	($logger: expr, $record: expr) => {
		$logger.print($record);
	}
}

#[macro_export]
macro_rules! log_client {
	($logger: expr, $record: expr) => {
		$logger.print_client($record);
	}
}

/// Basic log logic, excluding gossips messages
pub struct LogPrinter {
	log_file: String,
}

impl LogPrinter {
	pub fn new(file: &str) -> Self {
		Self {
			log_file: file.to_string()
		}
	}
	pub fn print(&self, record: &str) {
		if let Ok(mut log_file) = OpenOptions::new().append(true).open(&self.log_file) {
			if let Err (_) = log_file.write(format!("NODE: {}\n", record).as_bytes()) {
				panic!("Can't write file {}", self.log_file);
			}
		} else {
			panic!("Can't open file {}", self.log_file);
		}
	}
	pub fn print_client(&self, record: &str) {
		if let Ok(mut log_file) = OpenOptions::new().append(true).open(&self.log_file) {
			if let Err (_) = log_file.write(format!("CLIENT: {}\n", record).as_bytes()) {
				panic!("Can't write file {}", self.log_file);
			}
		} else {
			panic!("Can't open file {}", self.log_file);
		}
	}
}

impl Logger for LogPrinter {
	fn log(&self, record: &Record) {
		let log = record.args.to_string();
		if !log.contains("Received message of type 258") && !log.contains("Received message of type 256") && !log.contains("Received message of type 257") {
			if let Ok(mut log_file) = OpenOptions::new().append(true).open(&self.log_file) {
				log_file.write_all(format!("{} {:<5} [{}:{}] {}", OffsetDateTime::now_utc().format("%F %T"), record.level.to_string(), record.module_path, record.line, log).as_bytes()).unwrap();
			}
		}
	}
}

#[macro_export]
macro_rules! exit_on {
	($msg: expr) => {
		eprintln!($msg);
		exit(1);
	}
}

/// Basic JSON-RPC client storing server host and credentials 
pub struct RpcClient {
	basic_auth: String,
	uri: String,
	id: AtomicUsize,
	client: hyper::Client<hyper::client::HttpConnector, hyper::Body>,
}

impl RpcClient {
	pub fn new(user_auth: &str, host_port: &str) -> Self {
		Self {
			basic_auth: "Basic ".to_string() + &base64::encode(user_auth),
			uri: "http://".to_string() + host_port,
			id: AtomicUsize::new(0),
			client: hyper::Client::new(),
		}
	}
	pub async fn send_request(&self, logger: &LogPrinter, payload: Value) {
		let host_port: Vec<&str> = "127.0.0.1:7688".split(':').collect();
		let host: Vec<u8> = host_port[0].split('.').map(|x| u8::from_str_radix(&x, 10).unwrap()).collect();
		let addr = SocketAddr::new(IpAddr::from([host[0], host[1], host[2], host[3]]), u16::from_str_radix(&host_port[1], 10).unwrap());

		if let Ok(mut stream) = TcpStream::connect(addr).await {
			if let Ok(data) = serde_json::to_string(&payload) {
				stream.write_u8(data.len() as u8).await.unwrap();
				stream.write_all(data.as_bytes()).await.unwrap();
				//stream.write(&[0]).await.unwrap();
				//stream.write_all(b"hello world!").await;
				let mut buffer = [0;20];
				stream.read_exact(&mut buffer).await.unwrap();
				if let Ok(string) = String::from_utf8(buffer.to_vec()) {
					println!("{}", string);
				}
			} else {
				panic!("JSON serialization issues");
			}
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
		}).await 
		{
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
