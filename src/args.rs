use crate::cli::LdkUserInfo;
use bitcoin::network::constants::Network;
use lightning::ln::msgs::NetAddress;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

pub(crate) fn parse_startup_args() -> Result<LdkUserInfo, ()> {
	if env::args().len() < 3 {
		println!("ldk-tutorial-node requires 3 arguments: `cargo run [<bitcoind-rpc-username>:<bitcoind-rpc-password>@]<bitcoind-rpc-host>:<bitcoind-rpc-port> ldk_storage_directory_path [<ldk-incoming-peer-listening-port>] [bitcoin-network] [announced-node-name announced-listen-addr*]`");
		return Err(());
	}
	let bitcoind_rpc_info = env::args().skip(1).next().unwrap();
	let bitcoind_rpc_info_parts: Vec<&str> = bitcoind_rpc_info.rsplitn(2, "@").collect();

	// Parse rpc auth after getting network for default .cookie location
	let bitcoind_rpc_path: Vec<&str> = bitcoind_rpc_info_parts[0].split(":").collect();
	if bitcoind_rpc_path.len() != 2 {
		println!("ERROR: bad bitcoind RPC path provided");
		return Err(());
	}
	let bitcoind_rpc_host = bitcoind_rpc_path[0].to_string();
	let bitcoind_rpc_port = bitcoind_rpc_path[1].parse::<u16>().unwrap();

	let ldk_storage_dir_path = env::args().skip(2).next().unwrap();

	let mut ldk_peer_port_set = true;
	let ldk_peer_listening_port: u16 = match env::args().skip(3).next().map(|p| p.parse()) {
		Some(Ok(p)) => p,
		Some(Err(_)) => {
			ldk_peer_port_set = false;
			9735
		}
		None => {
			ldk_peer_port_set = false;
			9735
		}
	};

	let mut arg_idx = match ldk_peer_port_set {
		true => 4,
		false => 3,
	};
	let network: Network = match env::args().skip(arg_idx).next().as_ref().map(String::as_str) {
		Some("testnet") => Network::Testnet,
		Some("regtest") => Network::Regtest,
		Some("signet") => Network::Signet,
		Some(net) => {
			panic!("Unsupported network provided. Options are: `regtest`, `testnet`, and `signet`. Got {}", net);
		}
		None => Network::Testnet,
	};

	let (bitcoind_rpc_username, bitcoind_rpc_password) = if bitcoind_rpc_info_parts.len() == 1 {
		get_rpc_auth_from_env_vars()
			.or(get_rpc_auth_from_env_file(None))
			.or(get_rpc_auth_from_cookie(None, Some(network), None))
			.or({
				println!("ERROR: unable to get bitcoind RPC username and password");
				print_rpc_auth_help();
				Err(())
			})?
	} else if bitcoind_rpc_info_parts.len() == 2 {
		parse_rpc_auth(bitcoind_rpc_info_parts[1])?
	} else {
		println!("ERROR: bad bitcoind RPC URL provided");
		return Err(());
	};

	let ldk_announced_node_name = match env::args().skip(arg_idx + 1).next().as_ref() {
		Some(s) => {
			if s.len() > 32 {
				panic!("Node Alias can not be longer than 32 bytes");
			}
			arg_idx += 1;
			let mut bytes = [0; 32];
			bytes[..s.len()].copy_from_slice(s.as_bytes());
			bytes
		}
		None => [0; 32],
	};

	let mut ldk_announced_listen_addr = Vec::new();
	loop {
		match env::args().skip(arg_idx + 1).next().as_ref() {
			Some(s) => match IpAddr::from_str(s) {
				Ok(IpAddr::V4(a)) => {
					ldk_announced_listen_addr
						.push(NetAddress::IPv4 { addr: a.octets(), port: ldk_peer_listening_port });
					arg_idx += 1;
				}
				Ok(IpAddr::V6(a)) => {
					ldk_announced_listen_addr
						.push(NetAddress::IPv6 { addr: a.octets(), port: ldk_peer_listening_port });
					arg_idx += 1;
				}
				Err(_) => panic!("Failed to parse announced-listen-addr into an IP address"),
			},
			None => break,
		}
	}

	Ok(LdkUserInfo {
		bitcoind_rpc_username,
		bitcoind_rpc_password,
		bitcoind_rpc_host,
		bitcoind_rpc_port,
		ldk_storage_dir_path,
		ldk_peer_listening_port,
		ldk_announced_listen_addr,
		ldk_announced_node_name,
		network,
	})
}

// Default datadir relative to home directory
#[cfg(target_os = "windows")]
const DEFAULT_BITCOIN_DATADIR: &str = "AppData/Roaming/Bitcoin";
#[cfg(target_os = "linux")]
const DEFAULT_BITCOIN_DATADIR: &str = ".bitcoin";
#[cfg(target_os = "macos")]
const DEFAULT_BITCOIN_DATADIR: &str = "Library/Application Support/Bitcoin";

// Environment variable/.env keys
const BITCOIND_RPC_USER_KEY: &str = "RPC_USER";
const BITCOIND_RPC_PASSWORD_KEY: &str = "RPC_PASSWORD";

fn print_rpc_auth_help() {
	// Get the default data directory
	let home_dir = env::home_dir()
		.as_ref()
		.map(|ref p| p.to_str())
		.flatten()
		.unwrap_or("$HOME")
		.replace("\\", "/");
	let data_dir = format!("{}/{}", home_dir, DEFAULT_BITCOIN_DATADIR);
	println!("To provide the bitcoind RPC username and password, you can either:");
	println!(
		"1. Provide the username and password as the first argument to this program in the format: \
		<bitcoind-rpc-username>:<bitcoind-rpc-password>@<bitcoind-rpc-host>:<bitcoind-rpc-port>"
	);
	println!("2. Provide <bitcoind-rpc-username>:<bitcoind-rpc-password> in a .cookie file in the default \
		bitcoind data directory (automatically created by bitcoind on startup): `{}`", data_dir);
	println!(
		"3. Set the {} and {} environment variables",
		BITCOIND_RPC_USER_KEY, BITCOIND_RPC_PASSWORD_KEY
	);
	println!(
		"4. Provide {} and {} fields in a .env file in the current directory",
		BITCOIND_RPC_USER_KEY, BITCOIND_RPC_PASSWORD_KEY
	);
}

fn parse_rpc_auth(rpc_auth: &str) -> Result<(String, String), ()> {
	let rpc_auth_info: Vec<&str> = rpc_auth.split(':').collect();
	if rpc_auth_info.len() != 2 {
		println!("ERROR: bad bitcoind RPC username/password combo provided");
		return Err(());
	}
	let rpc_username = rpc_auth_info[0].to_string();
	let rpc_password = rpc_auth_info[1].to_string();
	Ok((rpc_username, rpc_password))
}

fn get_cookie_path(
	data_dir: Option<(&str, bool)>, network: Option<Network>, cookie_file_name: Option<&str>,
) -> Result<PathBuf, ()> {
	let data_dir_path = match data_dir {
		Some((dir, true)) => env::home_dir().ok_or(())?.join(dir),
		Some((dir, false)) => PathBuf::from(dir),
		None => env::home_dir().ok_or(())?.join(DEFAULT_BITCOIN_DATADIR),
	};

	let data_dir_path_with_net = match network {
		Some(Network::Testnet) => data_dir_path.join("testnet3"),
		Some(Network::Regtest) => data_dir_path.join("regtest"),
		Some(Network::Signet) => data_dir_path.join("signet"),
		_ => data_dir_path,
	};

	let cookie_path = data_dir_path_with_net.join(cookie_file_name.unwrap_or(".cookie"));

	Ok(cookie_path)
}

fn get_rpc_auth_from_cookie(
	data_dir: Option<(&str, bool)>, network: Option<Network>, cookie_file_name: Option<&str>,
) -> Result<(String, String), ()> {
	let cookie_path = get_cookie_path(data_dir, network, cookie_file_name)?;
	let cookie_contents = fs::read_to_string(cookie_path).or(Err(()))?;
	parse_rpc_auth(&cookie_contents)
}

fn get_rpc_auth_from_env_vars() -> Result<(String, String), ()> {
	if let (Ok(username), Ok(password)) =
		(env::var(BITCOIND_RPC_USER_KEY), env::var(BITCOIND_RPC_PASSWORD_KEY))
	{
		Ok((username, password))
	} else {
		Err(())
	}
}

fn get_rpc_auth_from_env_file(env_file_name: Option<&str>) -> Result<(String, String), ()> {
	let env_file_map = parse_env_file(env_file_name)?;
	if let (Some(username), Some(password)) =
		(env_file_map.get(BITCOIND_RPC_USER_KEY), env_file_map.get(BITCOIND_RPC_PASSWORD_KEY))
	{
		Ok((username.to_string(), password.to_string()))
	} else {
		Err(())
	}
}

fn parse_env_file(env_file_name: Option<&str>) -> Result<HashMap<String, String>, ()> {
	// Default .env file name is .env
	let env_file_name = match env_file_name {
		Some(filename) => filename,
		None => ".env",
	};

	// Read .env file
	let env_file_path = Path::new(env_file_name);
	let env_file_contents = fs::read_to_string(env_file_path).or(Err(()))?;

	// Collect key-value pairs from .env file into a map
	let mut env_file_map: HashMap<String, String> = HashMap::new();
	for line in env_file_contents.lines() {
		let line_parts: Vec<&str> = line.splitn(2, '=').collect();
		if line_parts.len() != 2 {
			println!("ERROR: bad .env file format");
			return Err(());
		}
		env_file_map.insert(line_parts[0].to_string(), line_parts[1].to_string());
	}

	Ok(env_file_map)
}

#[cfg(test)]
mod rpc_auth_tests {
	use super::*;

	const TEST_ENV_FILE: &str = "test_data/test_env_file";
	const TEST_ENV_FILE_BAD: &str = "test_data/test_env_file_bad";
	const TEST_ABSENT_FILE: &str = "nonexistent_file";
	const TEST_DATA_DIR: &str = "test_data";
	const TEST_COOKIE: &str = "test_cookie";
	const TEST_COOKIE_BAD: &str = "test_cookie_bad";
	const EXPECTED_USER: &str = "testuser";
	const EXPECTED_PASSWORD: &str = "testpassword";

	#[test]
	fn test_parse_rpc_auth_success() {
		let (username, password) = parse_rpc_auth("testuser:testpassword").unwrap();
		assert_eq!(username, EXPECTED_USER);
		assert_eq!(password, EXPECTED_PASSWORD);
	}

	#[test]
	fn test_parse_rpc_auth_fail() {
		let result = parse_rpc_auth("testuser");
		assert!(result.is_err());
	}

	#[test]
	fn test_get_cookie_path_success() {
		let test_cases = vec![
			(
				None,
				None,
				None,
				env::home_dir().unwrap().join(DEFAULT_BITCOIN_DATADIR).join(".cookie"),
			),
			(
				Some((TEST_DATA_DIR, true)),
				Some(Network::Testnet),
				None,
				env::home_dir().unwrap().join(TEST_DATA_DIR).join("testnet3").join(".cookie"),
			),
			(
				Some((TEST_DATA_DIR, false)),
				Some(Network::Regtest),
				Some(TEST_COOKIE),
				PathBuf::from(TEST_DATA_DIR).join("regtest").join(TEST_COOKIE),
			),
			(
				Some((TEST_DATA_DIR, false)),
				Some(Network::Signet),
				None,
				PathBuf::from(TEST_DATA_DIR).join("signet").join(".cookie"),
			),
			(
				Some((TEST_DATA_DIR, false)),
				Some(Network::Bitcoin),
				None,
				PathBuf::from(TEST_DATA_DIR).join(".cookie"),
			),
		];

		for (data_dir, network, cookie_file, expected_path) in test_cases {
			let path = get_cookie_path(data_dir, network, cookie_file).unwrap();
			assert_eq!(path, expected_path);
		}
	}

	#[test]
	fn test_get_rpc_auth_from_cookie_success() {
		let (username, password) = get_rpc_auth_from_cookie(
			Some((TEST_DATA_DIR, false)),
			Some(Network::Bitcoin),
			Some(TEST_COOKIE),
		)
		.unwrap();
		assert_eq!(username, EXPECTED_USER);
		assert_eq!(password, EXPECTED_PASSWORD);
	}

	#[test]
	fn test_get_rpc_auth_from_cookie_fail() {
		let result = get_rpc_auth_from_cookie(
			Some((TEST_DATA_DIR, false)),
			Some(Network::Bitcoin),
			Some(TEST_COOKIE_BAD),
		);
		assert!(result.is_err());
	}

	#[test]
	fn test_parse_env_file_success() {
		let env_file_map = parse_env_file(Some(TEST_ENV_FILE)).unwrap();
		assert_eq!(env_file_map.get(BITCOIND_RPC_USER_KEY).unwrap(), EXPECTED_USER);
		assert_eq!(env_file_map.get(BITCOIND_RPC_PASSWORD_KEY).unwrap(), EXPECTED_PASSWORD);
	}

	#[test]
	fn test_parse_env_file_fail() {
		let env_file_map = parse_env_file(Some(TEST_ENV_FILE_BAD));
		assert!(env_file_map.is_err());

		// Make sure the test file doesn't exist
		assert!(!Path::new(TEST_ABSENT_FILE).exists());
		let env_file_map = parse_env_file(Some(TEST_ABSENT_FILE));
		assert!(env_file_map.is_err());
	}

	#[test]
	fn test_get_rpc_auth_from_env_file_success() {
		let (username, password) = get_rpc_auth_from_env_file(Some(TEST_ENV_FILE)).unwrap();
		assert_eq!(username, EXPECTED_USER);
		assert_eq!(password, EXPECTED_PASSWORD);
	}

	#[test]
	fn test_get_rpc_auth_from_env_file_fail() {
		let rpc_user_and_password = get_rpc_auth_from_env_file(Some(TEST_ABSENT_FILE));
		assert!(rpc_user_and_password.is_err());
	}

	#[test]
	fn test_get_rpc_auth_from_env_vars_success() {
		env::set_var(BITCOIND_RPC_USER_KEY, EXPECTED_USER);
		env::set_var(BITCOIND_RPC_PASSWORD_KEY, EXPECTED_PASSWORD);
		let (username, password) = get_rpc_auth_from_env_vars().unwrap();
		assert_eq!(username, EXPECTED_USER);
		assert_eq!(password, EXPECTED_PASSWORD);
	}
}
