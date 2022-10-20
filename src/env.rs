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

impl Env {
    pub fn testnet(&self) -> bool {
        return env::var("TESTNET").is_ok();
    }
    pub fn network(&self) -> String {
        if Env::testnet(self) { return "testnet".to_string(); }
        "mainnet".to_string()
    }
    pub fn private_key(&self) -> Option<Vec<u8>> {
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
    pub fn ldk_data_dir(&self) -> String {
        match env::var("LDK_DATA_DIR") {
            Ok(path) => path,
            Err(_e) => ".ldk-data".to_string()
        }
    }

    pub fn print(&self) {
        println!("Env:");
        let mut net = "mainnet";
        if Env::testnet(&self) { net = "testnet"; }
        println!("  network:     {}", net);
        let pk = Env::private_key(&self);
        match pk {
            Some(key) => println!("  private key: ******** ({})", key.len()),
            None => println!("  private key: not set"),
        }
        let path1 = Env::ldk_data_dir(&self);
        println!("  data path:   {}", path1);
    }
}
