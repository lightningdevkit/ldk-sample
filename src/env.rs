use std::env;
use std::fs;
use dotenv;

static PK_FILENAME: &str = ".pk_secret";

pub struct Env {
}

pub fn env_init() -> Env {
    dotenv::dotenv().ok();
    return Env{};
}

pub fn testnet() -> bool {
    match env::var("TESTNET") {
        Err(_e) => false,
        Ok(val) => val == "true" || val == "TRUE" || val == "1",
    }
}
pub fn network() -> String {
    if testnet() { return "testnet".to_string(); }
    "mainnet".to_string()
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
pub fn ldk_data_dir() -> String {
    match env::var("LDK_DATA_DIR") {
        Ok(path) => path,
        Err(_e) => ".ldk-data".to_string() // default
    }
}

impl Env {
    pub fn print(&self) {
        println!("Env:");
        let mut net = "mainnet";
        if testnet() { net = "testnet"; }
        println!("  network:     {}", net);
        let pk = private_key();
        match pk {
            Some(key) => println!("  private key: ******** ({})", key.len()),
            None => println!("  private key: not set!"),
        }
        let path1 = ldk_data_dir();
        println!("  data path:   {}", path1);
    }
}
