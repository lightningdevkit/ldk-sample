use crate::walletcore_iface::*;
use crate::convert::Unspents;
use crate::bitcoind_client::BitcoindClient;

pub struct Wallet {
    pub address: String,
    pub utxos: Unspents,
    pub balance: f64,
}

pub fn is_mnemonic_valid(mnemonic: &str) -> bool {
    return mnemonic_is_valid(&TWString::from_str(mnemonic));
}

pub fn priv_key_from_mnemonic(mnemonic: &str) -> Option<Vec<u8>> {
    let wallet = hd_wallet_create_with_mnemonic(&TWString::from_str(mnemonic), &TWString::from_str(""));
    let key = hd_wallet_get_key_for_coin(&wallet, 0);
    Some(private_key_data(&key).to_vec())
}

pub fn derive_address_from_pk(priv_key: &Vec<u8>, network: &str) -> String {
    let priv_key_obj = private_key_create_with_data(&TWData::from_vec(&priv_key));
    let pub_key = private_key_get_public_key_secp256k1(&priv_key_obj, true);
    let any_addr = any_address_create_with_public_key(&pub_key, 0);
    let addr_twstring = any_address_description(&any_addr);
    ////////////// TODO hardcoded testnet address
    if network == "test" {
        "tb1qwj4ezzdhcnk687utkhns8xens5832f8sthluw5".to_string() //"tb1qp265zr4w7c9s3nmd0mv3x357f6y8mdphn9qw92".to_string();
    } else {
        addr_twstring.to_string()
    }
}

impl Wallet {
    pub fn derive_address_from_pk(priv_key: &Vec<u8>, network: &str) -> Wallet {
        Wallet {
            address: derive_address_from_pk(priv_key, network),
            utxos: Unspents { utxos: Vec::new() },
            balance: 0.0,
        }
    }

    pub async fn retrieve_unspent(&self, bitcoind_client: &BitcoindClient) -> Unspents {
        bitcoind_client.list_unspent(0, self.address.as_str()).await
    }

    pub async fn retrieve_and_store_unspent(&mut self, bitcoind_client: &BitcoindClient)  {
        self.utxos = self.retrieve_unspent(bitcoind_client).await;
        self.balance = 0.0;
        for u in &self.utxos.utxos {
            self.balance += u.amount;
        }
    }
}
