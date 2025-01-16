use std::sync::Arc;

use bitcoin::secp256k1::{Secp256k1, SecretKey};
use lightning::sign::{InMemorySigner, NodeSigner, OutputSpender, SignerProvider};

use lightning::sign::{EntropySource, KeysManager};
use lightning_invoice::RawBolt11Invoice;

pub struct MyKeys {
	pub keys_manager: Arc<MyKeysManager>,
}

impl MyKeys {
	pub fn new(seed: [u8; 32], starting_time_secs: u64, starting_time_nanos: u32) -> Self {
		MyKeys {
			keys_manager: Arc::new(MyKeysManager::new(
				&seed,
				starting_time_secs,
				starting_time_nanos,
			)),
		}
	}

	pub fn with_channel_keys(
		seed: [u8; 32], starting_time_secs: u64, starting_time_nanos: u32, channels_keys: String,
	) -> Self {
		let keys = channels_keys.split('/').collect::<Vec<_>>();

		let mut manager = MyKeysManager::new(&seed, starting_time_secs, starting_time_nanos);
		manager.set_channel_keys(
			keys[0].to_string(),
			keys[1].to_string(),
			keys[2].to_string(),
			keys[3].to_string(),
			keys[4].to_string(),
			keys[5].to_string(),
		);
		MyKeys { keys_manager: Arc::new(manager) }
	}

	pub fn inner(&self) -> Arc<MyKeysManager> {
		self.keys_manager.clone()
	}
}

/// MyKeysManager is a custom keysmanager that allows the use of custom channel secrets.
pub struct MyKeysManager {
	pub(crate) inner: KeysManager,

	funding_key: Option<SecretKey>,
	revocation_base_secret: Option<SecretKey>,
	payment_base_secret: Option<SecretKey>,
	delayed_payment_base_secret: Option<SecretKey>,
	htlc_base_secret: Option<SecretKey>,
	shachain_seed: Option<[u8; 32]>,
}

impl MyKeysManager {
	pub fn new(seed: &[u8; 32], starting_time_secs: u64, starting_time_nanos: u32) -> Self {
		let inner = KeysManager::new(seed, starting_time_secs, starting_time_nanos);
		Self {
			inner,
			funding_key: None,
			revocation_base_secret: None,
			payment_base_secret: None,
			delayed_payment_base_secret: None,
			htlc_base_secret: None,
			shachain_seed: None,
		}
	}

	pub fn get_node_secret_key(&self) -> SecretKey {
		self.inner.get_node_secret_key()
	}

	pub fn set_channel_keys(
		&mut self, funding_key: String, revocation_base_secret: String,
		payment_base_secret: String, delayed_payment_base_secret: String, htlc_base_secret: String,
		_shachain_seed: String,
	) {
		use std::str::FromStr;

		self.funding_key = Some(SecretKey::from_str(&funding_key).unwrap());
		self.revocation_base_secret = Some(SecretKey::from_str(&revocation_base_secret).unwrap());
		self.payment_base_secret = Some(SecretKey::from_str(&payment_base_secret).unwrap());
		self.delayed_payment_base_secret =
			Some(SecretKey::from_str(&delayed_payment_base_secret).unwrap());
		self.htlc_base_secret = Some(SecretKey::from_str(&htlc_base_secret).unwrap());
		self.shachain_seed = Some(self.inner.get_secure_random_bytes())
	}
}

impl EntropySource for MyKeysManager {
	fn get_secure_random_bytes(&self) -> [u8; 32] {
		self.inner.get_secure_random_bytes()
	}
}

impl NodeSigner for MyKeysManager {
	fn ecdh(
		&self, recipient: lightning::sign::Recipient, other_key: &bitcoin::secp256k1::PublicKey,
		tweak: Option<&bitcoin::secp256k1::Scalar>,
	) -> Result<bitcoin::secp256k1::ecdh::SharedSecret, ()> {
		self.inner.ecdh(recipient, other_key, tweak)
	}

	fn get_inbound_payment_key_material(&self) -> lightning::sign::KeyMaterial {
		self.inner.get_inbound_payment_key_material()
	}

	fn get_node_id(
		&self, recipient: lightning::sign::Recipient,
	) -> Result<bitcoin::secp256k1::PublicKey, ()> {
		self.inner.get_node_id(recipient)
	}

	fn sign_bolt12_invoice(
		&self, invoice: &lightning::offers::invoice::UnsignedBolt12Invoice,
	) -> Result<bitcoin::secp256k1::schnorr::Signature, ()> {
		self.inner.sign_bolt12_invoice(invoice)
	}

	fn sign_bolt12_invoice_request(
		&self, invoice_request: &lightning::offers::invoice_request::UnsignedInvoiceRequest,
	) -> Result<bitcoin::secp256k1::schnorr::Signature, ()> {
		self.inner.sign_bolt12_invoice_request(invoice_request)
	}

	fn sign_gossip_message(
		&self, msg: lightning::ln::msgs::UnsignedGossipMessage,
	) -> Result<bitcoin::secp256k1::ecdsa::Signature, ()> {
		self.inner.sign_gossip_message(msg)
	}

	fn sign_invoice(
		&self, invoice: &RawBolt11Invoice, recipient: lightning::sign::Recipient,
	) -> Result<bitcoin::secp256k1::ecdsa::RecoverableSignature, ()> {
		self.inner.sign_invoice(invoice, recipient)
	}
}

impl OutputSpender for MyKeysManager {
	fn spend_spendable_outputs<C: bitcoin::secp256k1::Signing>(
		&self, descriptors: &[&lightning::sign::SpendableOutputDescriptor],
		outputs: Vec<bitcoin::TxOut>, change_destination_script: bitcoin::ScriptBuf,
		feerate_sat_per_1000_weight: u32, locktime: Option<bitcoin::absolute::LockTime>,
		secp_ctx: &bitcoin::secp256k1::Secp256k1<C>,
	) -> Result<bitcoin::Transaction, ()> {
		self.inner.spend_spendable_outputs(
			descriptors,
			outputs,
			change_destination_script,
			feerate_sat_per_1000_weight,
			locktime,
			secp_ctx,
		)
	}
}

impl SignerProvider for MyKeysManager {
	type EcdsaSigner = InMemorySigner;

	fn derive_channel_signer(
		&self, channel_value_satoshis: u64, channel_keys_id: [u8; 32],
	) -> Self::EcdsaSigner {
		if self.funding_key.is_some() {
			let commitment_seed = [
				255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
				255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
			];
			return InMemorySigner::new(
				&Secp256k1::new(),
				self.funding_key.unwrap(),
				self.revocation_base_secret.unwrap(),
				self.payment_base_secret.unwrap(),
				self.delayed_payment_base_secret.unwrap(),
				self.htlc_base_secret.unwrap(),
				commitment_seed,
				channel_value_satoshis,
				channel_keys_id,
				self.shachain_seed.unwrap(),
			);
		}
		self.inner.derive_channel_signer(channel_value_satoshis, channel_keys_id)
	}

	fn generate_channel_keys_id(
		&self, inbound: bool, channel_value_satoshis: u64, user_channel_id: u128,
	) -> [u8; 32] {
		self.inner.generate_channel_keys_id(inbound, channel_value_satoshis, user_channel_id)
	}

	fn get_destination_script(&self, channel_keys_id: [u8; 32]) -> Result<bitcoin::ScriptBuf, ()> {
		self.inner.get_destination_script(channel_keys_id)
	}

	fn get_shutdown_scriptpubkey(&self) -> Result<lightning::ln::script::ShutdownScript, ()> {
		self.inner.get_shutdown_scriptpubkey()
	}

	fn read_chan_signer(
		&self, reader: &[u8],
	) -> Result<Self::EcdsaSigner, lightning::ln::msgs::DecodeError> {
		self.inner.read_chan_signer(reader)
	}
}
