use crate::convert::{BlockchainInfo, FeeResponse, FundedTx, NewAddress, RawTx, SignedTx};
use base64;
use bitcoin::blockdata::block::Block;
use bitcoin::blockdata::transaction::Transaction;
use bitcoin::consensus::encode;
use bitcoin::hash_types::BlockHash;
use bitcoin::util::address::Address;
use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning_block_sync::http::HttpEndpoint;
use lightning_block_sync::rpc::RpcClient;
use lightning_block_sync::{AsyncBlockSourceResult, BlockHeaderData, BlockSource};
use serde_json;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

pub struct BitcoindClient {
	bitcoind_rpc_client: Arc<Mutex<RpcClient>>,
	host: String,
	port: u16,
	rpc_user: String,
	rpc_password: String,
	fees: Arc<HashMap<Target, AtomicU32>>,
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub enum Target {
	Background,
	Normal,
	HighPriority,
}

impl BlockSource for &BitcoindClient {
	fn get_header<'a>(
		&'a mut self, header_hash: &'a BlockHash, height_hint: Option<u32>,
	) -> AsyncBlockSourceResult<'a, BlockHeaderData> {
		Box::pin(async move {
			let mut rpc = self.bitcoind_rpc_client.lock().await;
			rpc.get_header(header_hash, height_hint).await
		})
	}

	fn get_block<'a>(
		&'a mut self, header_hash: &'a BlockHash,
	) -> AsyncBlockSourceResult<'a, Block> {
		Box::pin(async move {
			let mut rpc = self.bitcoind_rpc_client.lock().await;
			rpc.get_block(header_hash).await
		})
	}

	fn get_best_block<'a>(&'a mut self) -> AsyncBlockSourceResult<(BlockHash, Option<u32>)> {
		Box::pin(async move {
			let mut rpc = self.bitcoind_rpc_client.lock().await;
			rpc.get_best_block().await
		})
	}
}

impl BitcoindClient {
	pub async fn new(
		host: String, port: u16, rpc_user: String, rpc_password: String,
	) -> std::io::Result<Self> {
		let http_endpoint = HttpEndpoint::for_host(host.clone()).with_port(port);
		let rpc_credentials =
			base64::encode(format!("{}:{}", rpc_user.clone(), rpc_password.clone()));
		let bitcoind_rpc_client = RpcClient::new(&rpc_credentials, http_endpoint)?;
		let mut fees: HashMap<Target, AtomicU32> = HashMap::new();
		fees.insert(Target::Background, AtomicU32::new(253));
		fees.insert(Target::Normal, AtomicU32::new(2000));
		fees.insert(Target::HighPriority, AtomicU32::new(5000));
		let client = Self {
			bitcoind_rpc_client: Arc::new(Mutex::new(bitcoind_rpc_client)),
			host,
			port,
			rpc_user,
			rpc_password,
			fees: Arc::new(fees),
		};
		BitcoindClient::poll_for_fee_estimates(
			client.fees.clone(),
			client.bitcoind_rpc_client.clone(),
		)
		.await;
		Ok(client)
	}

	async fn poll_for_fee_estimates(
		fees: Arc<HashMap<Target, AtomicU32>>, rpc_client: Arc<Mutex<RpcClient>>,
	) {
		tokio::spawn(async move {
			loop {
				let background_estimate = {
					let mut rpc = rpc_client.lock().await;
					let background_conf_target = serde_json::json!(144);
					let background_estimate_mode = serde_json::json!("ECONOMICAL");
					let resp = rpc
						.call_method::<FeeResponse>(
							"estimatesmartfee",
							&vec![background_conf_target, background_estimate_mode],
						)
						.await
						.unwrap();
					match resp.feerate {
						Some(fee) => fee,
						None => 253,
					}
				};
				// if background_estimate.

				let normal_estimate = {
					let mut rpc = rpc_client.lock().await;
					let normal_conf_target = serde_json::json!(18);
					let normal_estimate_mode = serde_json::json!("ECONOMICAL");
					let resp = rpc
						.call_method::<FeeResponse>(
							"estimatesmartfee",
							&vec![normal_conf_target, normal_estimate_mode],
						)
						.await
						.unwrap();
					match resp.feerate {
						Some(fee) => fee,
						None => 2000,
					}
				};

				let high_prio_estimate = {
					let mut rpc = rpc_client.lock().await;
					let high_prio_conf_target = serde_json::json!(6);
					let high_prio_estimate_mode = serde_json::json!("CONSERVATIVE");
					let resp = rpc
						.call_method::<FeeResponse>(
							"estimatesmartfee",
							&vec![high_prio_conf_target, high_prio_estimate_mode],
						)
						.await
						.unwrap();

					match resp.feerate {
						Some(fee) => fee,
						None => 5000,
					}
				};

				fees.get(&Target::Background)
					.unwrap()
					.store(background_estimate, Ordering::Release);
				fees.get(&Target::Normal).unwrap().store(normal_estimate, Ordering::Release);
				fees.get(&Target::HighPriority)
					.unwrap()
					.store(high_prio_estimate, Ordering::Release);
				// match fees.get(Target::Background) {
				//     Some(fee) => fee.store(background_estimate, Ordering::Release),
				//     None =>
				// }
				// if let Some(fee) = background_estimate.feerate {
				//     fees.get("background").unwrap().store(fee, Ordering::Release);
				// }
				// if let Some(fee) = normal_estimate.feerate {
				//     fees.get("normal").unwrap().store(fee, Ordering::Release);
				// }
				// if let Some(fee) = high_prio_estimate.feerate {
				//     fees.get("high_prio").unwrap().store(fee, Ordering::Release);
				// }
				tokio::time::sleep(Duration::from_secs(60)).await;
			}
		});
	}

	pub fn get_new_rpc_client(&self) -> std::io::Result<RpcClient> {
		let http_endpoint = HttpEndpoint::for_host(self.host.clone()).with_port(self.port);
		let rpc_credentials =
			base64::encode(format!("{}:{}", self.rpc_user.clone(), self.rpc_password.clone()));
		RpcClient::new(&rpc_credentials, http_endpoint)
	}

	pub async fn create_raw_transaction(&self, outputs: Vec<HashMap<String, f64>>) -> RawTx {
		let mut rpc = self.bitcoind_rpc_client.lock().await;

		let outputs_json = serde_json::json!(outputs);
		rpc.call_method::<RawTx>("createrawtransaction", &vec![serde_json::json!([]), outputs_json])
			.await
			.unwrap()
	}

	pub async fn fund_raw_transaction(&self, raw_tx: RawTx) -> FundedTx {
		let mut rpc = self.bitcoind_rpc_client.lock().await;

		let raw_tx_json = serde_json::json!(raw_tx.0);
		rpc.call_method("fundrawtransaction", &[raw_tx_json]).await.unwrap()
	}

	pub async fn send_raw_transaction(&self, raw_tx: RawTx) {
		let mut rpc = self.bitcoind_rpc_client.lock().await;

		let raw_tx_json = serde_json::json!(raw_tx.0);
		rpc.call_method::<RawTx>("sendrawtransaction", &[raw_tx_json]).await.unwrap();
	}

	pub async fn sign_raw_transaction_with_wallet(&self, tx_hex: String) -> SignedTx {
		let mut rpc = self.bitcoind_rpc_client.lock().await;

		let tx_hex_json = serde_json::json!(tx_hex);
		rpc.call_method("signrawtransactionwithwallet", &vec![tx_hex_json]).await.unwrap()
	}

	pub async fn get_new_address(&self) -> Address {
		let mut rpc = self.bitcoind_rpc_client.lock().await;

		let addr_args = vec![serde_json::json!("LDK output address")];
		let addr = rpc.call_method::<NewAddress>("getnewaddress", &addr_args).await.unwrap();
		Address::from_str(addr.0.as_str()).unwrap()
	}

	pub async fn get_blockchain_info(&self) -> BlockchainInfo {
		let mut rpc = self.bitcoind_rpc_client.lock().await;
		rpc.call_method::<BlockchainInfo>("getblockchaininfo", &vec![]).await.unwrap()
	}
}

impl FeeEstimator for BitcoindClient {
	fn get_est_sat_per_1000_weight(&self, confirmation_target: ConfirmationTarget) -> u32 {
		match confirmation_target {
			ConfirmationTarget::Background => {
				self.fees.get(&Target::Background).unwrap().load(Ordering::Acquire)
			}
			ConfirmationTarget::Normal => {
				self.fees.get(&Target::Normal).unwrap().load(Ordering::Acquire)
			}
			ConfirmationTarget::HighPriority => {
				self.fees.get(&Target::HighPriority).unwrap().load(Ordering::Acquire)
			}
		}
		// self.fees.g
		// 253
		// match confirmation_target {
		//     ConfirmationTarget::Background =>
		// }
		// let mut rpc = self.bitcoind_rpc_client.lock().unwrap();

		// let (conf_target, estimate_mode, default) = match confirmation_target {
		// 	ConfirmationTarget::Background => (144, "ECONOMICAL", 253),
		// 	ConfirmationTarget::Normal => (18, "ECONOMICAL", 20000),
		// 	ConfirmationTarget::HighPriority => (6, "CONSERVATIVE", 50000),
		// };

		// // This function may be called from a tokio runtime, or not. So we need to check before
		// // making the call to avoid the error "cannot run a tokio runtime from within a tokio runtime".
		// let conf_target_json = serde_json::json!(conf_target);
		// let estimate_mode_json = serde_json::json!(estimate_mode);
		// let resp = match Handle::try_current() {
		// 	Ok(_) => tokio::task::block_in_place(|| {
		// 		runtime
		// 			.block_on(rpc.call_method::<FeeResponse>(
		// 				"estimatesmartfee",
		// 				&vec![conf_target_json, estimate_mode_json],
		// 			))
		// 			.unwrap()
		// 	}),
		// 	_ => runtime
		// 		.block_on(rpc.call_method::<FeeResponse>(
		// 			"estimatesmartfee",
		// 			&vec![conf_target_json, estimate_mode_json],
		// 		))
		// 		.unwrap(),
		// };
		// if resp.errored {
		// 	return default;
		// }
		// resp.feerate.unwrap()
	}
}

impl BroadcasterInterface for BitcoindClient {
	fn broadcast_transaction(&self, tx: &Transaction) {
		let bitcoind_rpc_client = self.bitcoind_rpc_client.clone();
		let tx_serialized = serde_json::json!(encode::serialize_hex(tx));
		tokio::spawn(async move {
			let mut rpc = bitcoind_rpc_client.lock().await;
			rpc.call_method::<RawTx>("sendrawtransaction", &vec![tx_serialized]).await.unwrap();
		});
		// let bitcoind_rpc_client = self.bitcoind_rpc_client.clone();
		// tokio::spawn(async move {
		//     let rpc = bitcoind_rpc_client.lock().await;
		//     rpc.call_method::<R>
		// });
		// let mut rpc = self.bitcoind_rpc_client.lock().unwrap();
		// let runtime = self.runtime.lock().unwrap();

		// let tx_serialized = serde_json::json!(encode::serialize_hex(tx));
		// // This function may be called from a tokio runtime, or not. So we need to check before
		// // making the call to avoid the error "cannot run a tokio runtime from within a tokio runtime".
		// match Handle::try_current() {
		// 	Ok(_) => {
		// 		tokio::task::block_in_place(|| {
		// 			runtime
		// 				.block_on(
		// 					rpc.call_method::<RawTx>("sendrawtransaction", &vec![tx_serialized]),
		// 				)
		// 				.unwrap();
		// 		});
		// 	}
		// 	_ => {
		// 		runtime
		// 			.block_on(rpc.call_method::<RawTx>("sendrawtransaction", &vec![tx_serialized]))
		// 			.unwrap();
		// 	}
		// }
	}
}
