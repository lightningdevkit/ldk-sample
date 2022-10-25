use crate::wc_proto::Bitcoin;
use crate::walletcore_iface::*;
use crate::convert::Unspents;
use crate::bitcoind_client::BitcoindClient;
use protobuf::Message;
use bitcoin::network::constants::Network;

pub struct Wallet {
    // own address
    pub address: String,
    // private key, private field
    private_key: Vec<u8>,
    pub public_key: Vec<u8>,
    pub utxos: Unspents,
    pub balance: f64,
}

pub fn is_mnemonic_valid(mnemonic: &str) -> bool {
    return mnemonic_is_valid(&TWString::from_str(mnemonic));
}

pub fn priv_key_from_mnemonic(mnemonic: &str, network: Network) -> Option<Vec<u8>> {
    let wallet = hd_wallet_create_with_mnemonic(&TWString::from_str(mnemonic), &TWString::from_str(""));
    let derivation_path = if network == Network::Testnet { "m/84'/1'/0'/0/0" } else { "m/84'/0'/0'/0/0" };
    let dp_twstring = TWString::from_str(derivation_path);
    let key = hd_wallet_get_key(&wallet, 0, &dp_twstring);
    Some(private_key_data(&key).to_vec())
}

fn derive_pubkey_from_pk_intern(priv_key: &Vec<u8>) -> PublicKey {
    let priv_key_obj = private_key_create_with_data(&TWData::from_vec(&priv_key));
    private_key_get_public_key_secp256k1(&priv_key_obj, true)
}

fn derive_pubkey_from_pk(priv_key: &Vec<u8>) -> Vec<u8> {
    let pub_key = derive_pubkey_from_pk_intern(priv_key);
    public_key_data(&pub_key).to_vec()
}

pub fn derive_address_from_pk(priv_key: &Vec<u8>, network: Network) -> String {
    let pub_key = derive_pubkey_from_pk_intern(priv_key);
    let any_addr = any_address_create_with_public_key(&pub_key, 0);
    let addr_twstring = any_address_description(&any_addr);
    ////////////// TODO hardcoded testnet address
    if network == Network::Testnet {
        //"tb1qp265zr4w7c9s3nmd0mv3x357f6y8mdphn9qw92".to_string()
        "tb1q7t37kn0rm2ftlfw6uwghvy59hv7hhz76u9j00d".to_string()
        //"tb1qwj4ezzdhcnk687utkhns8xens5832f8sthluw5".to_string()
    } else {
        addr_twstring.to_string()
    }
}

impl Wallet {
    pub fn from_pk(priv_key: &Vec<u8>, network: Network) -> Wallet {
        Wallet {
            address: derive_address_from_pk(priv_key, network),
            private_key: priv_key.clone(),
            public_key: derive_pubkey_from_pk(&priv_key.clone()),
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

    pub fn create_send_tx(&self, to_address: &str, output_amount: u64) -> Vec<u8> {
        let mut signing_input = Bitcoin::SigningInput::new();
        signing_input.hash_type = 1; // hashTypeAll
        signing_input.amount = output_amount as i64;
        signing_input.use_max_amount = false;
        signing_input.byte_fee = 1; // TODO
        signing_input.to_address = to_address.to_string();
        signing_input.change_address = self.address.clone();
        signing_input.coin_type = 0;
        signing_input.private_key.push(self.private_key.clone());

        for u in &self.utxos.utxos {
            if u.address != self.address {
                println!("discarding utxo, not own-address {} {}", u.address, self.address);
            } else {
                let mut utxo = Bitcoin::UnspentTransaction::new();
                let mut outpoint = Bitcoin::OutPoint::new();
                let mut hash = hex::decode(u.tx_id.clone()).unwrap();
                hash.reverse();
                outpoint.hash = hash;
                outpoint.index = u.vout;
                outpoint.sequence = u32::MAX - 1;
                utxo.out_point = ::protobuf::MessageField::some(outpoint);
                utxo.script = hex::decode(&u.script_pub_key).unwrap();
                utxo.amount = (u.amount * 100_000_000.0) as i64;
                println!("input utxo  '{}' '{}' '{}' {}", u.address, u.script_pub_key, u.witness_script, utxo.amount);
                signing_input.utxo.push(utxo);
            }
        }
        if signing_input.utxo.len() == 0 {
            println!("Error: 0 utxos to consider");
            return Vec::new();
        }

        let input_ser = signing_input.write_to_bytes().unwrap();
        let input_ser_data = TWData::from_vec(&input_ser);

        let output_ser_data = any_signer_sign(&input_ser_data, 0);

        let outputp: Bitcoin::SigningOutput = protobuf::Message::parse_from_bytes(&output_ser_data.to_vec()).unwrap();

        println!("tx encoded: {}", hex::encode(outputp.encoded.clone()));
        println!("tx tx_id:   {}", outputp.transaction_id);
        println!("tx error:   {} {}", outputp.error.unwrap() as u16, outputp.error_message);

        outputp.encoded
    }
}
