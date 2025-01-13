use crate::convert::{
	BlockchainInfo, FeeResponse, FundedTx, ListUnspentResponse, MempoolMinFeeResponse, NewAddress,
	RawTx, SignedTx,
};
use crate::disk::FilesystemLogger;
use crate::hex_utils;
use base64;
use bitcoin::address::Address;
use bitcoin::blockdata::constants::WITNESS_SCALE_FACTOR;
use bitcoin::blockdata::script::ScriptBuf;
use bitcoin::blockdata::transaction::Transaction;
use bitcoin::consensus::{encode, Decodable, Encodable};
use bitcoin::hash_types::{BlockHash, Txid};
use bitcoin::hashes::Hash;
use bitcoin::key::XOnlyPublicKey;
use bitcoin::psbt::Psbt;
use bitcoin::{Network, OutPoint, TxOut, WPubkeyHash};
use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning::events::bump_transaction::{Utxo, WalletSource};
use lightning::log_error;
use lightning::sign::ChangeDestinationSource;
use lightning::util::logger::Logger;
use lightning_block_sync::http::HttpEndpoint;
use lightning_block_sync::rpc::RpcClient;
use lightning_block_sync::{AsyncBlockSourceResult, BlockData, BlockHeaderData, BlockSource};
use serde_json;
use std::collections::HashMap;
use std::future::Future;
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::{self, Runtime};

pub struct BitcoindClient {
	pub(crate) bitcoind_rpc_client: Arc<RpcClient>,
	network: Network,
	host: String,
	port: u16,
	rpc_user: String,
	rpc_password: String,
	fees: Arc<HashMap<ConfirmationTarget, AtomicU32>>,
	main_runtime_handle: runtime::Handle,
	inner_runtime: Arc<Runtime>,
	logger: Arc<FilesystemLogger>,
}

impl BlockSource for BitcoindClient {
	fn get_header<'a>(
		&'a self, header_hash: &'a BlockHash, height_hint: Option<u32>,
	) -> AsyncBlockSourceResult<'a, BlockHeaderData> {
		Box::pin(async move { self.bitcoind_rpc_client.get_header(header_hash, height_hint).await })
	}

	fn get_block<'a>(
		&'a self, header_hash: &'a BlockHash,
	) -> AsyncBlockSourceResult<'a, BlockData> {
		Box::pin(async move { self.bitcoind_rpc_client.get_block(header_hash).await })
	}

	fn get_best_block<'a>(&'a self) -> AsyncBlockSourceResult<(BlockHash, Option<u32>)> {
		Box::pin(async move { self.bitcoind_rpc_client.get_best_block().await })
	}
}

/// The minimum feerate we are allowed to send, as specify by LDK.
const MIN_FEERATE: u32 = 253;

impl BitcoindClient {
	pub(crate) async fn new(
		host: String, port: u16, rpc_user: String, rpc_password: String, network: Network,
		handle: runtime::Handle, logger: Arc<FilesystemLogger>,
	) -> std::io::Result<Self> {
		let http_endpoint = HttpEndpoint::for_host(host.clone()).with_port(port);
		let rpc_credentials =
			base64::encode(format!("{}:{}", rpc_user.clone(), rpc_password.clone()));
		let bitcoind_rpc_client = RpcClient::new(&rpc_credentials, http_endpoint);
		let _dummy = bitcoind_rpc_client
			.call_method::<BlockchainInfo>("getblockchaininfo", &vec![])
			.await
			.map_err(|_| {
				std::io::Error::new(std::io::ErrorKind::PermissionDenied,
				"Failed to make initial call to bitcoind - please check your RPC user/password and access settings")
			})?;
		let mut fees: HashMap<ConfirmationTarget, AtomicU32> = HashMap::new();
		fees.insert(ConfirmationTarget::MaximumFeeEstimate, AtomicU32::new(50000));
		fees.insert(ConfirmationTarget::UrgentOnChainSweep, AtomicU32::new(5000));
		fees.insert(
			ConfirmationTarget::MinAllowedAnchorChannelRemoteFee,
			AtomicU32::new(MIN_FEERATE),
		);
		fees.insert(
			ConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee,
			AtomicU32::new(MIN_FEERATE),
		);
		fees.insert(ConfirmationTarget::AnchorChannelFee, AtomicU32::new(MIN_FEERATE));
		fees.insert(ConfirmationTarget::NonAnchorChannelFee, AtomicU32::new(2000));
		fees.insert(ConfirmationTarget::ChannelCloseMinimum, AtomicU32::new(MIN_FEERATE));
		fees.insert(ConfirmationTarget::OutputSpendingFee, AtomicU32::new(MIN_FEERATE));

		let mut builder = runtime::Builder::new_multi_thread();
		let runtime =
			builder.enable_all().worker_threads(1).thread_name("rpc-worker").build().unwrap();
		let inner_runtime = Arc::new(runtime);
		// Tokio will panic if we drop a runtime while in another runtime. Because the entire
		// application runs inside a tokio runtime, we have to ensure this runtime is never
		// `drop`'d, which we do by leaking an Arc reference.
		std::mem::forget(Arc::clone(&inner_runtime));

		let client = Self {
			bitcoind_rpc_client: Arc::new(bitcoind_rpc_client),
			host,
			port,
			rpc_user,
			rpc_password,
			network,
			fees: Arc::new(fees),
			main_runtime_handle: handle.clone(),
			inner_runtime,
			logger,
		};
		BitcoindClient::poll_for_fee_estimates(
			client.fees.clone(),
			client.bitcoind_rpc_client.clone(),
			handle,
		);
		Ok(client)
	}

	fn poll_for_fee_estimates(
		fees: Arc<HashMap<ConfirmationTarget, AtomicU32>>, rpc_client: Arc<RpcClient>,
		handle: tokio::runtime::Handle,
	) {
		handle.spawn(async move {
			loop {
				let mempoolmin_estimate = {
					let resp = rpc_client
						.call_method::<MempoolMinFeeResponse>("getmempoolinfo", &vec![])
						.await
						.unwrap();
					match resp.feerate_sat_per_kw {
						Some(feerate) => std::cmp::max(feerate, MIN_FEERATE),
						None => MIN_FEERATE,
					}
				};
				let background_estimate = {
					let background_conf_target = serde_json::json!(144);
					let background_estimate_mode = serde_json::json!("ECONOMICAL");
					let resp = rpc_client
						.call_method::<FeeResponse>(
							"estimatesmartfee",
							&vec![background_conf_target, background_estimate_mode],
						)
						.await
						.unwrap();
					match resp.feerate_sat_per_kw {
						Some(feerate) => std::cmp::max(feerate, MIN_FEERATE),
						None => MIN_FEERATE,
					}
				};

				let normal_estimate = {
					let normal_conf_target = serde_json::json!(18);
					let normal_estimate_mode = serde_json::json!("ECONOMICAL");
					let resp = rpc_client
						.call_method::<FeeResponse>(
							"estimatesmartfee",
							&vec![normal_conf_target, normal_estimate_mode],
						)
						.await
						.unwrap();
					match resp.feerate_sat_per_kw {
						Some(feerate) => std::cmp::max(feerate, MIN_FEERATE),
						None => 2000,
					}
				};

				let high_prio_estimate = {
					let high_prio_conf_target = serde_json::json!(6);
					let high_prio_estimate_mode = serde_json::json!("CONSERVATIVE");
					let resp = rpc_client
						.call_method::<FeeResponse>(
							"estimatesmartfee",
							&vec![high_prio_conf_target, high_prio_estimate_mode],
						)
						.await
						.unwrap();

					match resp.feerate_sat_per_kw {
						Some(feerate) => std::cmp::max(feerate, MIN_FEERATE),
						None => 5000,
					}
				};

				let very_high_prio_estimate = {
					let high_prio_conf_target = serde_json::json!(2);
					let high_prio_estimate_mode = serde_json::json!("CONSERVATIVE");
					let resp = rpc_client
						.call_method::<FeeResponse>(
							"estimatesmartfee",
							&vec![high_prio_conf_target, high_prio_estimate_mode],
						)
						.await
						.unwrap();

					match resp.feerate_sat_per_kw {
						Some(feerate) => std::cmp::max(feerate, MIN_FEERATE),
						None => 50000,
					}
				};

				fees.get(&ConfirmationTarget::MaximumFeeEstimate)
					.unwrap()
					.store(very_high_prio_estimate, Ordering::Release);
				fees.get(&ConfirmationTarget::UrgentOnChainSweep)
					.unwrap()
					.store(high_prio_estimate, Ordering::Release);
				fees.get(&ConfirmationTarget::MinAllowedAnchorChannelRemoteFee)
					.unwrap()
					.store(mempoolmin_estimate, Ordering::Release);
				fees.get(&ConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee)
					.unwrap()
					.store(background_estimate - 250, Ordering::Release);
				fees.get(&ConfirmationTarget::AnchorChannelFee)
					.unwrap()
					.store(background_estimate, Ordering::Release);
				fees.get(&ConfirmationTarget::NonAnchorChannelFee)
					.unwrap()
					.store(normal_estimate, Ordering::Release);
				fees.get(&ConfirmationTarget::ChannelCloseMinimum)
					.unwrap()
					.store(background_estimate, Ordering::Release);
				fees.get(&ConfirmationTarget::OutputSpendingFee)
					.unwrap()
					.store(background_estimate, Ordering::Release);

				tokio::time::sleep(Duration::from_secs(60)).await;
			}
		});
	}

	fn run_future_in_blocking_context<F: Future + Send + 'static>(&self, future: F) -> F::Output
	where
		F::Output: Send + 'static,
	{
		// Tokio deliberately makes it nigh impossible to block on a future in a sync context that
		// is running in an async task (which makes it really hard to interact with sync code that
		// has callbacks in an async project).
		//
		// Reading the docs, it *seems* like
		// `tokio::task::block_in_place(tokio::runtime::Handle::spawn(future))` should do the
		// trick, and 99.999% of the time it does! But tokio has a "non-stealable I/O driver" - if
		// the task we're running happens to, by sheer luck, be holding the "I/O driver" when we go
		// into a `block_in_place` call, and the inner future requires I/O (which of course it
		// does, its a future!), the whole thing will come to a grinding halt as no other thread is
		// allowed to poll I/O until the blocked one finishes.
		//
		// This is, of course, nuts, and an almost trivial performance penalty of occasional
		// additional wakeups would solve this, but tokio refuses to do so because any performance
		// penalty at all would be too much (tokio issue #4730).
		//
		// Instead, we have to do a rather insane dance - we have to spawn the `future` we want to
		// run on a *different* (threaded) tokio runtime (doing the `block_in_place` dance to avoid
		// blocking too many threads on the main runtime). We want to block on that `future` being
		// run on the other runtime's threads, but tokio only provides `block_on` to do so, which
		// runs the `future` itself on the current thread, panicing if this thread is already a
		// part of a tokio runtime (which in this case it is - the main tokio runtime). Thus, we
		// have to `spawn` the `future` on the secondary runtime and then `block_on` the resulting
		// `JoinHandle` on the main runtime.
		tokio::task::block_in_place(move || {
			self.main_runtime_handle.block_on(self.inner_runtime.spawn(future)).unwrap()
		})
	}

	pub fn get_new_rpc_client(&self) -> RpcClient {
		let http_endpoint = HttpEndpoint::for_host(self.host.clone()).with_port(self.port);
		let rpc_credentials = base64::encode(format!("{}:{}", self.rpc_user, self.rpc_password));
		RpcClient::new(&rpc_credentials, http_endpoint)
	}

	pub async fn create_raw_transaction(&self, outputs: Vec<HashMap<String, f64>>) -> RawTx {
		let outputs_json = serde_json::json!(outputs);
		self.bitcoind_rpc_client
			.call_method::<RawTx>(
				"createrawtransaction",
				&vec![serde_json::json!([]), outputs_json],
			)
			.await
			.unwrap()
	}

	pub async fn fund_raw_transaction(&self, raw_tx: RawTx) -> FundedTx {
		let raw_tx_json = serde_json::json!(raw_tx.0);
		let options = serde_json::json!({
			// LDK gives us feerates in satoshis per KW but Bitcoin Core here expects fees
			// denominated in satoshis per vB. First we need to multiply by 4 to convert weight
			// units to virtual bytes, then divide by 1000 to convert KvB to vB.
			"fee_rate": self
				.get_est_sat_per_1000_weight(ConfirmationTarget::NonAnchorChannelFee) as f64 / 250.0,
			// While users could "cancel" a channel open by RBF-bumping and paying back to
			// themselves, we don't allow it here as its easy to have users accidentally RBF bump
			// and pay to the channel funding address, which results in loss of funds. Real
			// LDK-based applications should enable RBF bumping and RBF bump either to a local
			// change address or to a new channel output negotiated with the same node.
			"replaceable": false,
		});
		self.bitcoind_rpc_client
			.call_method("fundrawtransaction", &[raw_tx_json, options])
			.await
			.unwrap()
	}

	pub async fn send_raw_transaction(&self, raw_tx: RawTx) {
		let raw_tx_json = serde_json::json!(raw_tx.0);
		self.bitcoind_rpc_client
			.call_method::<Txid>("sendrawtransaction", &[raw_tx_json])
			.await
			.unwrap();
	}

	pub fn sign_raw_transaction_with_wallet(
		&self, tx_hex: String,
	) -> impl Future<Output = SignedTx> {
		let tx_hex_json = serde_json::json!(tx_hex);
		let rpc_client = self.get_new_rpc_client();
		async move {
			rpc_client
				.call_method("signrawtransactionwithwallet", &vec![tx_hex_json])
				.await
				.unwrap()
		}
	}

	pub fn get_new_address(&self) -> impl Future<Output = Address> {
		let addr_args = vec![serde_json::json!("LDK output address")];
		let network = self.network;
		let rpc_client = self.get_new_rpc_client();
		async move {
			let addr =
				rpc_client.call_method::<NewAddress>("getnewaddress", &addr_args).await.unwrap();
			Address::from_str(addr.0.as_str()).unwrap().require_network(network).unwrap()
		}
	}

	pub async fn get_blockchain_info(&self) -> BlockchainInfo {
		self.bitcoind_rpc_client
			.call_method::<BlockchainInfo>("getblockchaininfo", &vec![])
			.await
			.unwrap()
	}

	pub fn list_unspent(&self) -> impl Future<Output = ListUnspentResponse> {
		let rpc_client = self.get_new_rpc_client();
		async move {
			rpc_client.call_method::<ListUnspentResponse>("listunspent", &vec![]).await.unwrap()
		}
	}
}

impl FeeEstimator for BitcoindClient {
	fn get_est_sat_per_1000_weight(&self, confirmation_target: ConfirmationTarget) -> u32 {
		self.fees.get(&confirmation_target).unwrap().load(Ordering::Acquire)
	}
}

impl BroadcasterInterface for BitcoindClient {
	fn broadcast_transactions(&self, txs: &[&Transaction]) {
		// As of Bitcoin Core 28, using `submitpackage` allows us to broadcast multiple
		// transactions at once and have them propagate through the network as a whole, avoiding
		// some pitfalls with anchor channels where the first transaction doesn't make it into the
		// mempool at all. Several older versions of Bitcoin Core also support `submitpackage`,
		// however, so we just use it unconditionally here.
		// Sadly, Bitcoin Core has an arbitrary restriction on `submitpackage` - it must actually
		// contain a package (see https://github.com/bitcoin/bitcoin/issues/31085).
		let txn = txs.iter().map(|tx| encode::serialize_hex(tx)).collect::<Vec<_>>();
		let bitcoind_rpc_client = Arc::clone(&self.bitcoind_rpc_client);
		let logger = Arc::clone(&self.logger);
		self.main_runtime_handle.spawn(async move {
			let res = if txn.len() == 1 {
				let tx_json = serde_json::json!(txn[0]);
				bitcoind_rpc_client
					.call_method::<serde_json::Value>("sendrawtransaction", &[tx_json])
					.await
			} else {
				let tx_json = serde_json::json!(txn);
				bitcoind_rpc_client
					.call_method::<serde_json::Value>("submitpackage", &[tx_json])
					.await
			};
			// This may error due to RL calling `broadcast_transactions` with the same transaction
			// multiple times, but the error is safe to ignore.
			match res {
				Ok(_) => {}
				Err(e) => {
					let err_str = e.get_ref().unwrap().to_string();
					log_error!(logger,
						"Warning, failed to broadcast a transaction, this is likely okay but may indicate an error: {}\nTransactions: {:?}",
						err_str,
						txn);
					print!("Warning, failed to broadcast a transaction, this is likely okay but may indicate an error: {}\n> ", err_str);
				}
			}
		});
	}
}

impl ChangeDestinationSource for BitcoindClient {
	fn get_change_destination_script(&self) -> Result<ScriptBuf, ()> {
		let future = self.get_new_address();
		Ok(self.run_future_in_blocking_context(async move { future.await.script_pubkey() }))
	}
}

impl WalletSource for BitcoindClient {
	fn list_confirmed_utxos(&self) -> Result<Vec<Utxo>, ()> {
		let future = self.list_unspent();
		let utxos = self.run_future_in_blocking_context(async move { future.await.0 });
		Ok(utxos
			.into_iter()
			.filter_map(|utxo| {
				let outpoint = OutPoint { txid: utxo.txid, vout: utxo.vout };
				let value = bitcoin::Amount::from_sat(utxo.amount);
				match utxo.address.witness_program() {
					Some(prog) if prog.is_p2wpkh() => {
						WPubkeyHash::from_slice(prog.program().as_bytes())
							.map(|wpkh| Utxo::new_v0_p2wpkh(outpoint, value, &wpkh))
							.ok()
					},
					Some(prog) if prog.is_p2tr() => {
						// TODO: Add `Utxo::new_v1_p2tr` upstream.
						XOnlyPublicKey::from_slice(prog.program().as_bytes())
							.map(|_| Utxo {
								outpoint,
								output: TxOut {
									value,
									script_pubkey: utxo.address.script_pubkey(),
								},
								satisfaction_weight: 1 /* empty script_sig */ * WITNESS_SCALE_FACTOR as u64 +
									1 /* witness items */ + 1 /* schnorr sig len */ + 64, /* schnorr sig */
							})
							.ok()
					},
					_ => None,
				}
			})
			.collect())
	}

	fn get_change_script(&self) -> Result<ScriptBuf, ()> {
		let future = self.get_new_address();
		Ok(self.run_future_in_blocking_context(async move { future.await.script_pubkey() }))
	}

	fn sign_psbt(&self, tx: Psbt) -> Result<Transaction, ()> {
		let mut tx_bytes = Vec::new();
		let _ = tx.unsigned_tx.consensus_encode(&mut tx_bytes).map_err(|_| ());
		let tx_hex = hex_utils::hex_str(&tx_bytes);
		let future = self.sign_raw_transaction_with_wallet(tx_hex);
		let signed_tx = self.run_future_in_blocking_context(async move { future.await });
		let signed_tx_bytes = hex_utils::to_vec(&signed_tx.hex).ok_or(())?;
		Transaction::consensus_decode(&mut signed_tx_bytes.as_slice()).map_err(|_| ())
	}
}
