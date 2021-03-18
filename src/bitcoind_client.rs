use base64;
use bitcoin::blockdata::transaction::Transaction;
use bitcoin::consensus::encode;
use bitcoin::util::address::Address;
use crate::convert::{BlockchainInfo, FeeResponse, FundedTx, NewAddress, RawTx, SignedTx};
use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning_block_sync::http::HttpEndpoint;
use lightning_block_sync::rpc::RpcClient;
use serde_json;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Mutex;
use tokio::runtime::{Handle, Runtime};

pub struct BitcoindClient {
    bitcoind_rpc_client: Mutex<RpcClient>,
    host: String,
    port: u16,
    rpc_user: String,
    rpc_password: String,
    runtime: Mutex<Runtime>,
}

impl BitcoindClient {
    pub fn new(host: String, port: u16, rpc_user: String, rpc_password: String) ->
        std::io::Result<Self>
    {
        let http_endpoint = HttpEndpoint::for_host(host.clone()).with_port(port);
        let rpc_credentials = base64::encode(format!("{}:{}", rpc_user.clone(),
                                                     rpc_password.clone()));
        let bitcoind_rpc_client = RpcClient::new(&rpc_credentials, http_endpoint)?;
        let client = Self {
            bitcoind_rpc_client: Mutex::new(bitcoind_rpc_client),
            host,
            port,
            rpc_user,
            rpc_password,
            runtime: Mutex::new(Runtime::new().unwrap()),
        };
        Ok(client)
    }

    pub fn get_new_rpc_client(&self) -> std::io::Result<RpcClient> {
        let http_endpoint = HttpEndpoint::for_host(self.host.clone()).with_port(self.port);
        let rpc_credentials = base64::encode(format!("{}:{}",
                                                     self.rpc_user.clone(),
                                                     self.rpc_password.clone()));
        RpcClient::new(&rpc_credentials, http_endpoint)
    }

    pub fn create_raw_transaction(&self, outputs: Vec<HashMap<String, f64>>) -> RawTx {
        let runtime = self.runtime.lock().unwrap();
        let mut rpc = self.bitcoind_rpc_client.lock().unwrap();

        let outputs_json = serde_json::json!(outputs);
        runtime.block_on(rpc.call_method::<RawTx>("createrawtransaction", &vec![serde_json::json!([]), outputs_json])).unwrap()
    }

    pub fn fund_raw_transaction(&self, raw_tx: RawTx) -> FundedTx {
        let runtime = self.runtime.lock().unwrap();
        let mut rpc = self.bitcoind_rpc_client.lock().unwrap();

        let raw_tx_json = serde_json::json!(raw_tx.0);
        runtime.block_on(rpc.call_method("fundrawtransaction", &[raw_tx_json])).unwrap()
    }

    pub fn sign_raw_transaction_with_wallet(&self, tx_hex: String) -> SignedTx {
        let runtime = self.runtime.lock().unwrap();
        let mut rpc = self.bitcoind_rpc_client.lock().unwrap();

        let tx_hex_json = serde_json::json!(tx_hex);
        runtime.block_on(rpc.call_method("signrawtransactionwithwallet",
                                         &vec![tx_hex_json])).unwrap()
    }

    pub fn get_new_address(&self) -> Address {
        let runtime = self.runtime.lock().unwrap();
        let mut rpc = self.bitcoind_rpc_client.lock().unwrap();

        let addr_args = vec![serde_json::json!("LDK output address")];
        let addr = runtime.block_on(rpc.call_method::<NewAddress>("getnewaddress", &addr_args)).unwrap();
        Address::from_str(addr.0.as_str()).unwrap()
    }

    pub fn get_blockchain_info(&self) -> BlockchainInfo {
        let runtime = self.runtime.lock().unwrap();
        let mut rpc = self.bitcoind_rpc_client.lock().unwrap();

        runtime.block_on(rpc.call_method::<BlockchainInfo>("getblockchaininfo",
                                                                           &vec![])).unwrap()
    }
}

impl FeeEstimator for BitcoindClient {
    fn get_est_sat_per_1000_weight(&self, confirmation_target: ConfirmationTarget) -> u32 {
        let runtime = self.runtime.lock().unwrap();
        let mut rpc = self.bitcoind_rpc_client.lock().unwrap();

        let (conf_target, estimate_mode, default) = match confirmation_target {
            ConfirmationTarget::Background => (144, "ECONOMICAL", 253),
            ConfirmationTarget::Normal => (18, "ECONOMICAL", 20000),
            ConfirmationTarget::HighPriority => (6, "ECONOMICAL", 50000),
        };

        // This function may be called from a tokio runtime, or not. So we need to check before
        // making the call to avoid the error "cannot run a tokio runtime from within a tokio runtime".
        let conf_target_json = serde_json::json!(conf_target);
        let estimate_mode_json = serde_json::json!(estimate_mode);
        let resp = match Handle::try_current() {
            Ok(_) => {
                tokio::task::block_in_place(|| {
                    runtime.block_on(rpc.call_method::<FeeResponse>("estimatesmartfee",
                                                                    &vec![conf_target_json,
                                                                          estimate_mode_json])).unwrap()
                })
            },
            _ => runtime.block_on(rpc.call_method::<FeeResponse>("estimatesmartfee",
                                                                 &vec![conf_target_json,
                                                                       estimate_mode_json])).unwrap()
        };
        if resp.errored {
            return default
        }
        resp.feerate.unwrap()
    }
}

impl BroadcasterInterface for BitcoindClient {
	  fn broadcast_transaction(&self, tx: &Transaction) {
        let mut rpc = self.bitcoind_rpc_client.lock().unwrap();
        let runtime = self.runtime.lock().unwrap();

        let tx_serialized = serde_json::json!(encode::serialize_hex(tx));
        // This function may be called from a tokio runtime, or not. So we need to check before
        // making the call to avoid the error "cannot run a tokio runtime from within a tokio runtime".
        match Handle::try_current() {
            Ok(_) => {
                tokio::task::block_in_place(|| {
                    runtime.block_on(rpc.call_method::<RawTx>("sendrawtransaction",
                                                                          &vec![tx_serialized])).unwrap();
                });
            },
            _ => {
                runtime.block_on(rpc.call_method::<RawTx>("sendrawtransaction",
                                                                      &vec![tx_serialized])).unwrap();
            }
        }
    }
}
