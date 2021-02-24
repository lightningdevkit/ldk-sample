use base64;
use serde_json;

use lightning::chain::chaininterface::{ConfirmationTarget, FeeEstimator};
use lightning::util::logger::{Logger, Record};
use lightning_block_sync::http::HttpEndpoint;
use lightning_block_sync::rpc::RpcClient;

use std::sync::Mutex;

pub struct BitcoindFeeEstimator {
    bitcoind_rpc_client: Mutex<RpcClient>,
}

impl BitcoindFeeEstimator {
    fn new(host: String, port: u16, path: Option<String>, rpc_user: String, rpc_password: String) ->
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

impl FeeEstimator for BitcoindFeeEstimator {
    fn get_est_sat_per_1000_weight(&self, confirmation_target: ConfirmationTarget) -> u32 {
        let mut rpc_client_guard = self.bitcoind_rpc_client.lock().unwrap();
        match confirmation_target {
            ConfirmationTarget::Background => {
                let conf_target = serde_json::json!(144);
                let estimate_mode = serde_json::json!("ECONOMICAL");
                let resp = rpc_client_guard.call_method("estimatesmartfee",
                                                        &vec![conf_target, estimate_mode]).unwrap();
                resp["feerate"].as_u64().unwrap() as u32
            },
            ConfirmationTarget::Normal => {
                let conf_target = serde_json::json!(18);
                let estimate_mode = serde_json::json!("ECONOMICAL");
                let resp = rpc_client_guard.call_method("estimatesmartfee",
                                                        &vec![conf_target, estimate_mode]).unwrap();
                resp["feerate"].as_u64().unwrap() as u32
            },
            ConfirmationTarget::HighPriority => {
                let conf_target = serde_json::json!(6);
                let estimate_mode = serde_json::json!("CONSERVATIVE");
                let resp = rpc_client_guard.call_method("estimatesmartfee",
                                                        &vec![conf_target, estimate_mode]).unwrap();
                resp["feerate"].as_u64().unwrap() as u32
            },
        }
    }
}

fn main() {
    let bitcoind_host = "127.0.0.1".to_string();
    let bitcoind_port = 18443;
    let rpc_user = "polaruser".to_string();
    let rpc_password = "polarpass".to_string();
    let fee_estimator = BitcoindFeeEstimator::new(bitcoind_host, bitcoind_port, None, rpc_user, rpc_password).unwrap();
    let normal_fee = fee_estimator.get_est_sat_per_1000_weight(ConfirmationTarget::Normal);
    println!("VMW: {}", normal_fee);
}
