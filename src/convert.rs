use bitcoin::BlockHash;
use bitcoin::hashes::hex::FromHex;
use lightning_block_sync::http::JsonResponse;
use std::convert::TryInto;

pub struct FundedTx {
    pub changepos: i64,
    pub hex: String,
}

impl TryInto<FundedTx> for JsonResponse {
    type Error = std::io::Error;
    fn try_into(self) -> std::io::Result<FundedTx> {
        Ok(FundedTx {
            changepos: self.0["changepos"].as_i64().unwrap(),
            hex: self.0["hex"].as_str().unwrap().to_string(),
        })
    }
}

pub struct RawTx(pub String);

impl TryInto<RawTx> for JsonResponse {
    type Error = std::io::Error;
    fn try_into(self) -> std::io::Result<RawTx> {
        Ok(RawTx(self.0.as_str().unwrap().to_string()))
    }
}

pub struct SignedTx {
    pub complete: bool,
    pub hex: String,
}

impl TryInto<SignedTx> for JsonResponse {
    type Error = std::io::Error;
    fn try_into(self) -> std::io::Result<SignedTx> {
        Ok(SignedTx {
            hex: self.0["hex"].as_str().unwrap().to_string(),
            complete: self.0["complete"].as_bool().unwrap(),
        })
    }
}

pub struct NewAddress(pub String);
impl TryInto<NewAddress> for JsonResponse {
    type Error = std::io::Error;
    fn try_into(self) -> std::io::Result<NewAddress> {
        Ok(NewAddress(self.0.as_str().unwrap().to_string()))
    }
}

pub struct FeeResponse {
    pub feerate: Option<u32>,
    pub errored: bool,
}

impl TryInto<FeeResponse> for JsonResponse {
    type Error = std::io::Error;
    fn try_into(self) -> std::io::Result<FeeResponse> {
        let errored = !self.0["errors"].is_null();
        Ok(FeeResponse {
            errored,
            feerate: match errored {
                true => None,
                // The feerate from bitcoind is in BTC/kb, and we want satoshis/kb.
                false => Some((self.0["feerate"].as_f64().unwrap() * 100_000_000.0).round() as u32)
            }
        })
    }
}

pub struct BlockchainInfo {
    pub latest_height: usize,
    pub latest_blockhash: BlockHash,
}

impl TryInto<BlockchainInfo> for JsonResponse {
    type Error = std::io::Error;
    fn try_into(self) -> std::io::Result<BlockchainInfo> {
        Ok(BlockchainInfo {
            latest_height: self.0["blocks"].as_u64().unwrap() as usize,
            latest_blockhash: BlockHash::from_hex(self.0["bestblockhash"].as_str().unwrap()).unwrap(),
        })
    }
}
