use crate::walletcore_iface::*;

pub fn is_mnemonic_valid(mnemonic: &str) -> bool {
    return mnemonic_is_valid(&TWString::from_str(mnemonic));
}

pub fn priv_key_from_mnemonic(mnemonic: &str) -> Option<Vec<u8>> {
    let wallet = hd_wallet_create_with_mnemonic(&TWString::from_str(mnemonic), &TWString::from_str(""));
    let key = hd_wallet_get_key_for_coin(&wallet, 0);
    Some(private_key_data(&key).to_vec())
}

pub fn derive_address_from_pk(priv_key: &Vec<u8>) -> String {
    let priv_key_obj = private_key_create_with_data(&TWData::from_vec(&priv_key));
    let pub_key = private_key_get_public_key_secp256k1(&priv_key_obj, true);
    let any_addr = any_address_create_with_public_key(&pub_key, 0);
    let addr_twstring = any_address_description(&any_addr);
    addr_twstring.to_string()
}