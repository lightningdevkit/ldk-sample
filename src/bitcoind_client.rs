use base64;
use serde_json;

use bitcoin::blockdata::transaction::Transaction;
use bitcoin::consensus::encode;
use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning_block_sync::http::HttpEndpoint;
use lightning_block_sync::rpc::RpcClient;

use std::sync::Mutex;

pub struct BitcoindClient {
    pub bitcoind_rpc_client: Mutex<RpcClient>,
}

impl BitcoindClient {
    pub fn new(host: String, port: u16, path: Option<String>, rpc_user: String, rpc_password: String) ->
        std::io::Result<Self>
    {
        let mut http_endpoint = HttpEndpoint::for_host(host).with_port(port);
        if let Some(p) = path {
            http_endpoint = http_endpoint.with_path(p);
        }
        let rpc_credentials = base64::encode(format!("{}:{}", rpc_user, rpc_password));
        let bitcoind_rpc_client = RpcClient::new(&rpc_credentials, http_endpoint)?;
        Ok(Self {
            bitcoind_rpc_client: Mutex::new(bitcoind_rpc_client)
        })
    }
}

impl FeeEstimator for BitcoindClient {
    fn get_est_sat_per_1000_weight(&self, confirmation_target: ConfirmationTarget) -> u32 {
        let mut rpc_client_guard = self.bitcoind_rpc_client.lock().unwrap();
        match confirmation_target {
            ConfirmationTarget::Background => {
                let conf_target = serde_json::json!(144);
                let estimate_mode = serde_json::json!("ECONOMICAL");
                let resp = rpc_client_guard.call_method("estimatesmartfee",
                                                        &vec![conf_target, estimate_mode]).unwrap();
                if !resp["errors"].is_null() && resp["errors"].as_array().unwrap().len() > 0 {
                    return 253
                }
                resp["feerate"].as_u64().unwrap() as u32
            },
            ConfirmationTarget::Normal => {
                let conf_target = serde_json::json!(18);
                let estimate_mode = serde_json::json!("ECONOMICAL");
                let resp = rpc_client_guard.call_method("estimatesmartfee",
                                                        &vec![conf_target, estimate_mode]).unwrap();
                if !resp["errors"].is_null() && resp["errors"].as_array().unwrap().len() > 0 {
                    return 253
                }
                resp["feerate"].as_u64().unwrap() as u32
            },
            ConfirmationTarget::HighPriority => {
                let conf_target = serde_json::json!(6);
                let estimate_mode = serde_json::json!("CONSERVATIVE");
                let resp = rpc_client_guard.call_method("estimatesmartfee",
                                                        &vec![conf_target, estimate_mode]).unwrap();
                if !resp["errors"].is_null() && resp["errors"].as_array().unwrap().len() > 0 {
                    return 253
                }
                resp["feerate"].as_u64().unwrap() as u32
            },
        }
    }
}

impl BroadcasterInterface for BitcoindClient {
	  fn broadcast_transaction(&self, tx: &Transaction) {
        let mut rpc_client_guard = self.bitcoind_rpc_client.lock().unwrap();
        let tx_serialized = serde_json::json!(encode::serialize_hex(tx));
        rpc_client_guard.call_method("sendrawtransaction", &vec![tx_serialized]).unwrap();
    }
}
