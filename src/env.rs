use bitcoin::network::constants::Network;
use lightning::ln::msgs::NetAddress;
use std::net::IpAddr;
use dotenv;
use std::env;
use std::str::FromStr;

pub struct Env {
    pub network: Network,
    pub is_testnet: bool,
    pub default_peer: String,
    pub ldk_data_dir: String,
    pub bitcoind_rpc_username: String,
    pub bitcoind_rpc_password: String,
    pub bitcoind_rpc_host: String,
    pub bitcoind_rpc_port: u16,
    pub ldk_peer_listening_port: u16,
	pub ldk_announced_listen_addr: Vec<NetAddress>,
	pub ldk_announced_node_name: [u8; 32],
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
        Err(_e) => Network::Testnet,
        Ok(val) => match val.as_str() {
            "main" => Network::Bitcoin,
            "test" => Network::Testnet,
            "regtest" => Network::Regtest,
            "signet" => Network::Signet,
            &_ => panic!("Unsupported network provided. Options are: `main`, `test`, `regtest`, `signet`. Got {}", val),
        },
    };

    let is_testnet = network == Network::Testnet; 

    let rpc_host_port = env_with_no_default("RPC_HOST");
    let rpc_host_port_path: Vec<&str> = rpc_host_port.split(":").collect();
    let rpc_host = rpc_host_port_path[0];
    let rpc_port: u16 = if rpc_host_port_path.len() < 2 { 0 } else { rpc_host_port_path[1].parse::<u16>().unwrap() };

	let ldk_peer_listening_port: u16 = match env_with_no_default("LDK_PEER_LISTENING_PORT").parse() {
		Ok(p) => p,
		Err(_) => 9735,
	};

    let ldk_announced_node_name = match env::var("LDK_ANNOUNCED_NODE_NAME") {
        Err(_e) => [0; 32],
		Ok(s) => {
			if s.len() > 32 {
				panic!("Node Alias can not be longer than 32 bytes");
			}
			let mut bytes = [0; 32];
			bytes[..s.len()].copy_from_slice(s.as_bytes());
			bytes
		},
	};

    let listen_addrs_str = env_with_no_default("LDK_ANNOUNCED_LISTEN_ADDR");
    let listen_addrs_str_vec: Vec<&str> = listen_addrs_str.split("|").collect();
    let mut listen_addrs_vec: Vec<NetAddress> = Vec::new();
    for astr in listen_addrs_str_vec {
        if astr.len() > 0 {
            println!("{}", astr);
            let listen_addr = match IpAddr::from_str(astr) {
                Ok(IpAddr::V4(a)) => NetAddress::IPv4 { addr: a.octets(), port: ldk_peer_listening_port },
                Ok(IpAddr::V6(a)) => NetAddress::IPv6 { addr: a.octets(), port: ldk_peer_listening_port },
                Err(_) => panic!("Failed to parse announced-listen-addr into an IP address"),
            };
            listen_addrs_vec.push(listen_addr);
        }
    };

    return Env{
        network: network,
        is_testnet: is_testnet,
        default_peer: env_with_no_default("DEFAULT_PEER"),
        ldk_data_dir: match env::var("LDK_DATA_DIR") {
            Ok(path) => path,
            Err(_e) => ".ldk-data".to_string() // default
        },
        bitcoind_rpc_username: env_with_no_default("RPC_USERNAME"),
        bitcoind_rpc_password: env_with_no_default("RPC_PASSWORD"),
        bitcoind_rpc_host: rpc_host.to_string(),
        bitcoind_rpc_port: rpc_port,
        ldk_peer_listening_port: ldk_peer_listening_port,
        ldk_announced_node_name: ldk_announced_node_name,
        ldk_announced_listen_addr: listen_addrs_vec,
    };
}

impl Env {
    pub fn print(&self) {
        println!("Env:");
        println!("  network:      {}", self.network.to_string());
        println!("  default peer: {}", self.default_peer);
        println!("  RPC node:     {}:*****@{}:{}", self.bitcoind_rpc_username, self.bitcoind_rpc_host, self.bitcoind_rpc_port);
        println!("  data path:    {}", self.ldk_data_dir);
    }
}
