use std::env;
use std::fs;
use dotenv;

static PK_FILENAME: &str = ".pk_secret";

pub struct Env {
    pub network: String,
    pub is_testnet: bool,
    pub ldk_data_dir: String,
    pub rpc_user: String,
    pub rpc_password: String,
    pub rpc_host: String,
}

fn env_with_no_default(var: &str) -> String {
    match env::var(var) {
        Ok(val) => val,
        Err(_e) => "".to_string()
    }
}

pub fn env_init() -> Env {
    dotenv::dotenv().ok();

    let network = match env::var("NETWORK") {
        Err(_e) => "test".to_string(),
        Ok(val) => val,
    };
    let is_testnet = network == "test"; 
    
    return Env{
        network: network,
        is_testnet: is_testnet,
        ldk_data_dir: match env::var("LDK_DATA_DIR") {
            Ok(path) => path,
            Err(_e) => ".ldk-data".to_string() // default
        },
        rpc_user: env_with_no_default("RPC_USER"),
        rpc_password: env_with_no_default("RPC_PASSWORD"),
        rpc_host: env_with_no_default("RPC_HOST"),
    };
}

pub fn private_key() -> Option<Vec<u8>> {
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

pub fn set_private_key(key: &Vec<u8>) -> bool {
    let hex_string = hex::encode(key);
    match fs::write(PK_FILENAME, hex_string) {
        Err(_) => return false,
        Ok(_) => return true,
    }
}

impl Env {
    pub fn print(&self) {
        println!("Env:");
        println!("  network:     {}", self.network);
        println!("  RPC node:    {}:*****@{}", self.rpc_user, self.rpc_host);
        println!("  data path:   {}", self.ldk_data_dir);
        let pk = private_key();
        match pk {
            Some(key) => println!("  private key: ******** ({})", key.len()),
            None => println!("  private key: not set!"),
        }
    }
}
