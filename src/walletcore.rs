use crate::walletcore_iface::*;

pub fn test_wallet_core() {
	println!("=== Calling wallet-core from Rust");

    let mnemonic = "confirm bleak useless tail chalk destroy horn step bulb genuine attract split";
    println!("mnemonic is valid: {}", mnemonic_is_valid(&TWString::from_str(mnemonic)));
}
