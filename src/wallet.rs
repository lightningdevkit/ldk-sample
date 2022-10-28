use crate::wc_proto::Bitcoin;
use crate::walletcore_iface::*;
use crate::walletcore_extra::*;
use crate::convert::Unspents;
use crate::bitcoind_client::BitcoindClient;
use protobuf::Message;
use bitcoin::network::constants::Network;
use std::fs;

static PK_FILENAME: &str = ".pk_secret";

pub struct Wallet {
    // own address
    pub address: String,
    // private key, private field
    private_key: Vec<u8>,
    pub public_key: Vec<u8>,
    pub utxos: Unspents,
    pub balance: f64,
}

pub fn import_wallet_mnemonic(mnemonic: &str, network: Network) -> Option<Wallet> {
    if !is_mnemonic_valid(mnemonic) {
		println!("Mnemonic is invalid! {}", mnemonic);
		return None;
	}
	println!("Mnemonic is valid");
	let priv_key = match priv_key_from_mnemonic(mnemonic, network) {
		None => {
			println!("Could not derive private key");
			return None
		},
		Some(pk) => pk
	};
	println!("Private key derived ({} bytes)", priv_key.len());
	if !save_private_key(&priv_key) {
		println!("Could not save private key");
		return None
	}
	// check back
	match read_private_key() {
		None => {
			println!("Could not read back saved private key");
			return None;
		},
		Some(_priv_key_read_back) => println!("Private key saved"),
	}

    Some(Wallet::from_pk(&priv_key, network))
}

pub fn load_wallet(network: Network) -> Option<Wallet> {
    let pk = read_private_key();
    match pk {
        None => {
		    println!("Could not read wallet (private key, {})", PK_FILENAME);
            return None;
        },
        Some(key) => Some(Wallet::from_pk(&key, network)),
    }
}

fn read_private_key() -> Option<Vec<u8>> {
    let contents = fs::read_to_string(PK_FILENAME);
    match contents {
        Err(_e) => None,
        Ok(s) => {
            if s.len() < 64 {
                return None;
            }
            let key_decode = hex::decode(s.trim());
            match key_decode {
                Err(_e) => None,
                Ok(key) => Some(key)
            }
        }
    }
}

fn save_private_key(key: &Vec<u8>) -> bool {
    let hex_string = hex::encode(key);
    match fs::write(PK_FILENAME, hex_string) {
        Err(_) => return false,
        Ok(_) => return true,
    }
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
    let derivation = match network {
        Network::Testnet => 4, // DerivationBitcoinTestnet
        Network::Bitcoin |
        _ => 0, // DerivationDefault
    };
    let any_addr = any_address_create_with_public_key_derivation(&pub_key, 0, derivation);
    let addr_twstring = any_address_description(&any_addr);
    addr_twstring.to_string()
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

    pub fn print_address(&self) {
        println!("L1 wallet address: {}    pubkey:  {}", self.address, hex::encode(self.public_key.clone()));
    }

    pub fn print_balance(&self) {
        println!("L1 balance:  {}   utxos: {}", self.balance, self.utxos.utxos.len());
    }

    pub fn print(&self) {
        self.print_address();
        self.print_balance();
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
