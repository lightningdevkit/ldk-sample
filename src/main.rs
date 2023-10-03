mod args;
pub mod bitcoind_client;
mod chacha20;
mod cli;
mod convert;
mod disk;
mod hex_utils;
mod sweep;

use crate::bitcoind_client::BitcoindClient;
use crate::chacha20::ChaCha20;
use crate::disk::FilesystemLogger;
use bitcoin::bech32::u5;
use bitcoin::{BlockHash, PackedLockTime};
use bitcoin::blockdata::opcodes;
use bitcoin::blockdata::script::{Script, Builder};
use bitcoin::blockdata::transaction::{Transaction, TxOut, EcdsaSighashType};
use bitcoin::consensus::encode;
use bitcoin::hashes::{Hash, HashEngine};
use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::hashes::sha256d::Hash as Sha256dHash;
use bitcoin::hash_types::WPubkeyHash;
use bitcoin::network::constants::Network;
use bitcoin::psbt::PartiallySignedTransaction;
use bitcoin::PublicKey as OtherPublicKey;
use bitcoin::secp256k1::{ecdh::SharedSecret, ecdsa::RecoverableSignature, ecdsa::Signature, Message, PublicKey, Scalar, Secp256k1, SecretKey, Signing};
use bitcoin::{secp256k1, Witness};
use bitcoin::util::bip32::{ExtendedPrivKey, ExtendedPubKey, ChildNumber};
use bitcoin::util::sighash;
use bitcoin_bech32::WitnessProgram;
use core::ops::Deref;
use core::sync::atomic::AtomicUsize;
use disk::{INBOUND_PAYMENTS_FNAME, OUTBOUND_PAYMENTS_FNAME};
use lightning::sign::{InMemorySigner, EntropySource, Recipient, KeyMaterial, 
	SpendableOutputDescriptor, SignerProvider, NodeSigner};
use lightning::chain;
use lightning::chain::{chainmonitor, ChannelMonitorUpdateStatus};
use lightning::chain::{Filter, Watch};
use lightning::events::bump_transaction::{BumpTransactionEventHandler, Wallet};
use lightning::events::{Event, PaymentFailureReason, PaymentPurpose};
use lightning::ln::channelmanager;
use lightning::ln::channelmanager::{ChainParameters, ChannelManagerReadArgs};
use lightning::ln::msgs::{DecodeError, UnsignedGossipMessage};
use lightning::ln::peer_handler::{IgnoringMessageHandler, MessageHandler};
use lightning::ln::peer_handler;
use lightning::ln::{PaymentHash, PaymentPreimage, PaymentSecret};
use lightning::ln::script::ShutdownScript;
use lightning::onion_message;
use lightning::routing::gossip;
use lightning::routing::gossip::{NodeId, P2PGossipSync};
use lightning::routing::router::DefaultRouter;
use lightning::routing::scoring::ProbabilisticScorer;
use lightning::ln::channelmanager::RecentPaymentDetails;
use lightning::onion_message::DefaultMessageRouter;
use lightning::routing::scoring::ProbabilisticScoringFeeParameters;
use lightning::util::config::UserConfig;
use lightning::util::invoice::construct_invoice_preimage;
use lightning::util::persist::KVStorePersister;
use lightning::util::ser::{Readable, ReadableArgs, Writeable, Writer};
use lightning::{ impl_writeable_tlv_based, impl_writeable_tlv_based_enum};
use lightning_background_processor::{process_events_async, GossipSync};
use lightning_block_sync::init;
use lightning_block_sync::poll;
use lightning_block_sync::SpvClient;
use lightning_block_sync::UnboundedCache;
use lightning_net_tokio::SocketDescriptor;
use lightning_persister::FilesystemPersister;
use rand::{thread_rng, Rng};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::convert::TryInto;
use std::fmt;
use std::fs;
use std::fs::File;
use std::io;
use std::io::Write;
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};


const MAX_VALUE_MSAT: u64 = 21_000_000_0000_0000_000;
pub(crate) const PENDING_SPENDABLE_OUTPUT_DIR: &'static str = "pending_spendable_outputs";

#[derive(Copy, Clone)]
pub(crate) enum HTLCStatus {
	Pending,
	Succeeded,
	Failed,
}

impl_writeable_tlv_based_enum!(HTLCStatus,
	(0, Pending) => {},
	(1, Succeeded) => {},
	(2, Failed) => {};
);

pub(crate) struct MillisatAmount(Option<u64>);

impl fmt::Display for MillisatAmount {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match self.0 {
			Some(amt) => write!(f, "{}", amt),
			None => write!(f, "unknown"),
		}
	}
}

impl Readable for MillisatAmount {
	fn read<R: io::Read>(r: &mut R) -> Result<Self, DecodeError> {
		let amt: Option<u64> = Readable::read(r)?;
		Ok(MillisatAmount(amt))
	}
}

impl Writeable for MillisatAmount {
	fn write<W: Writer>(&self, w: &mut W) -> Result<(), std::io::Error> {
		self.0.write(w)
	}
}

pub(crate) struct PaymentInfo {
	preimage: Option<PaymentPreimage>,
	secret: Option<PaymentSecret>,
	status: HTLCStatus,
	amt_msat: MillisatAmount,
}

impl_writeable_tlv_based!(PaymentInfo, {
	(0, preimage, required),
	(2, secret, required),
	(4, status, required),
	(6, amt_msat, required),
});

pub(crate) struct PaymentInfoStorage {
	payments: HashMap<PaymentHash, PaymentInfo>,
}

impl_writeable_tlv_based!(PaymentInfoStorage, {
	(0, payments, required),
});

type ChainMonitor = chainmonitor::ChainMonitor<
	InMemorySigner,
	Arc<dyn Filter + Send + Sync>,
	Arc<BitcoindClient>,
	Arc<BitcoindClient>,
	Arc<FilesystemLogger>,
	Arc<FilesystemPersister>,
>;

pub type SimpleArcPeerManager<SD, M, T, F, C, L> = peer_handler::PeerManager<SD, Arc<SimpleArcChannelManager<M, T, F, L>>, Arc<P2PGossipSync<Arc<gossip::NetworkGraph<Arc<L>>>, Arc<C>, Arc<L>>>, Arc<SimpleArcOnionMessenger<L>>, Arc<L>, IgnoringMessageHandler, Arc<MyKeysManager>>;
pub(crate) type PeerManager = SimpleArcPeerManager<
	SocketDescriptor,
	ChainMonitor,
	BitcoindClient,
	BitcoindClient,
	BitcoindClient,
	FilesystemLogger,
>;
pub type SimpleArcChannelManager<M, T, F, L> = channelmanager::ChannelManager<
	Arc<M>,
	Arc<T>,
	Arc<MyKeysManager>,
	Arc<MyKeysManager>,
	Arc<MyKeysManager>,
	Arc<F>,
	Arc<DefaultRouter<
		Arc<gossip::NetworkGraph<Arc<L>>>,
		Arc<L>,
		Arc<Mutex<ProbabilisticScorer<Arc<gossip::NetworkGraph<Arc<L>>>, Arc<L>>>>,
		ProbabilisticScoringFeeParameters,
		ProbabilisticScorer<Arc<gossip::NetworkGraph<Arc<L>>>, Arc<L>>,
	>>,
	Arc<L>
>;
pub(crate) type ChannelManager =
	SimpleArcChannelManager<ChainMonitor, BitcoindClient, BitcoindClient, FilesystemLogger>;

pub(crate) type NetworkGraph = gossip::NetworkGraph<Arc<FilesystemLogger>>;

//////////////////////////////////////////////////Methods and structs required for MyKeysManager/////////////////////////////////////////////////////////////

macro_rules! hash_to_message {
	($slice: expr) => {
		{
			#[cfg(not(fuzzing))]
			{
				::bitcoin::secp256k1::Message::from_slice($slice).unwrap()
			}
			#[cfg(fuzzing)]
			{
				match ::bitcoin::secp256k1::Message::from_slice($slice) {
					Ok(msg) => msg,
					Err(_) => ::bitcoin::secp256k1::Message::from_slice(&[1; 32]).unwrap()
				}
			}
		}
	}
}
pub fn sign<C: Signing>(ctx: &Secp256k1<C>, msg: &Message, sk: &SecretKey) -> Signature {
	#[cfg(feature = "grind_signatures")]
	let sig = ctx.sign_ecdsa_low_r(msg, sk);
	#[cfg(not(feature = "grind_signatures"))]
	let sig = ctx.sign_ecdsa(msg, sk);
	sig
}

pub fn be64_to_array(u: u64) -> [u8; 8] {
	let mut v = [0; 8];
	v[0] = ((u >> 8*7) & 0xff) as u8;
	v[1] = ((u >> 8*6) & 0xff) as u8;
	v[2] = ((u >> 8*5) & 0xff) as u8;
	v[3] = ((u >> 8*4) & 0xff) as u8;
	v[4] = ((u >> 8*3) & 0xff) as u8;
	v[5] = ((u >> 8*2) & 0xff) as u8;
	v[6] = ((u >> 8*1) & 0xff) as u8;
	v[7] = ((u >> 8*0) & 0xff) as u8;
	v
}
pub fn be32_to_array(u: u32) -> [u8; 4] {
	let mut v = [0; 4];
	v[0] = ((u >> 8*3) & 0xff) as u8;
	v[1] = ((u >> 8*2) & 0xff) as u8;
	v[2] = ((u >> 8*1) & 0xff) as u8;
	v[3] = ((u >> 8*0) & 0xff) as u8;
	v
}
pub fn slice_to_be64(v: &[u8]) -> u64 {
	((v[0] as u64) << 8*7) |
	((v[1] as u64) << 8*6) |
	((v[2] as u64) << 8*5) |
	((v[3] as u64) << 8*4) |
	((v[4] as u64) << 8*3) |
	((v[5] as u64) << 8*2) |
	((v[6] as u64) << 8*1) |
	((v[7] as u64) << 8*0)
}
pub(crate) struct AtomicCounter {
	// Usize needs to be at least 32 bits to avoid overflowing both low and high. If usize is 64
	// bits we will never realistically count into high:
	counter_low: AtomicUsize,
	counter_high: AtomicUsize,
}

impl AtomicCounter {
	pub(crate) fn new() -> Self {
		Self {
			counter_low: AtomicUsize::new(0),
			counter_high: AtomicUsize::new(0),
		}
	}
	pub(crate) fn get_increment(&self) -> u64 {
		let low = self.counter_low.fetch_add(1, Ordering::AcqRel) as u64;
		let high = if low == 0 {
			self.counter_high.fetch_add(1, Ordering::AcqRel) as u64
		} else {
			self.counter_high.load(Ordering::Acquire) as u64
		};
		(high << 32) | low
	}
}

pub fn sign_with_aux_rand<C: Signing, ES: Deref>(
	ctx: &Secp256k1<C>, msg: &Message, sk: &SecretKey, entropy_source: &ES
) -> Signature where ES::Target: EntropySource {
	#[cfg(feature = "grind_signatures")]
	let sig = loop {
		let sig = ctx.sign_ecdsa_with_noncedata(msg, sk, &entropy_source.get_secure_random_bytes());
		if sig.serialize_compact()[0] < 0x80 {
			break sig;
		}
	};
	#[cfg(all(not(feature = "grind_signatures"), not(feature = "_test_vectors")))]
	let sig = ctx.sign_ecdsa_with_noncedata(msg, sk, &entropy_source.get_secure_random_bytes());
	#[cfg(all(not(feature = "grind_signatures"), feature = "_test_vectors"))]
	let sig = sign(ctx, msg, sk);
	sig
}

fn convert_to_array<T>(v: Vec<T>) -> [T; 32] {
    let boxed_slice = v.into_boxed_slice();
    let boxed_array: Box<[T; 32]> = match boxed_slice.try_into() {
        Ok(ba) => ba,
        Err(o) => panic!("Expected a Vec of length {} but it was {}", 32, o.len()),
    };
    *boxed_array
}


////////////////////////////////////////////MyKeysManager////////////////////////////////

pub struct MyKeysManager {
	secp_ctx: Secp256k1<secp256k1::All>,
	node_secret: SecretKey,
	node_id: PublicKey,
	inbound_payment_key: KeyMaterial,
	destination_script: Script,
	shutdown_pubkey: PublicKey,
	channel_master_key: ExtendedPrivKey,
	channel_child_index: AtomicUsize,

	rand_bytes_unique_start: [u8; 32],
	rand_bytes_index: AtomicCounter,

	seed: [u8; 32],
	starting_time_secs: u64,
	starting_time_nanos: u32,
	channel_secrets: String,
}

impl MyKeysManager {
	/// Constructs a [`KeysManager`] using custom channel secrets
	pub fn new(seed: &[u8; 32], starting_time_secs: u64, starting_time_nanos: u32, priv_key: String, channel_secrets: String) -> Self {
		let secp_ctx = Secp256k1::new();
		// Note that when we aren't serializing the key, network doesn't matter
		match ExtendedPrivKey::new_master(Network::Testnet, seed) {
			Ok(master_key) => {
				let node_secret;
				if priv_key.len() != 0 {
                	node_secret= SecretKey::from_str(&priv_key).unwrap();
				}
				else {
					node_secret = master_key.ckd_priv(&secp_ctx, ChildNumber::from_hardened_idx(0).unwrap()).expect("Your RNG is busted").private_key;
				}

                let node_id = PublicKey::from_secret_key(&secp_ctx, &node_secret);
				let destination_script = match master_key.ckd_priv(&secp_ctx, ChildNumber::from_hardened_idx(1).unwrap()) {
					Ok(destination_key) => {
						let wpubkey_hash = WPubkeyHash::hash(&ExtendedPubKey::from_priv(&secp_ctx, &destination_key).to_pub().to_bytes());
						Builder::new().push_opcode(opcodes::all::OP_PUSHBYTES_0)
							.push_slice(&wpubkey_hash.into_inner())
							.into_script()
					},
					Err(_) => panic!("Your RNG is busted"),
				};
				let shutdown_pubkey = match master_key.ckd_priv(&secp_ctx, ChildNumber::from_hardened_idx(2).unwrap()) {
					Ok(shutdown_key) => ExtendedPubKey::from_priv(&secp_ctx, &shutdown_key).public_key,
					Err(_) => panic!("Your RNG is busted"),
				};
				let channel_master_key = master_key.ckd_priv(&secp_ctx, ChildNumber::from_hardened_idx(3).unwrap()).expect("Your RNG is busted");
				let inbound_payment_key: SecretKey = master_key.ckd_priv(&secp_ctx, ChildNumber::from_hardened_idx(5).unwrap()).expect("Your RNG is busted").private_key;
				let mut inbound_pmt_key_bytes = [0; 32];
				inbound_pmt_key_bytes.copy_from_slice(&inbound_payment_key[..]);

				let mut rand_bytes_engine = Sha256::engine();
				rand_bytes_engine.input(&starting_time_secs.to_be_bytes());
				rand_bytes_engine.input(&starting_time_nanos.to_be_bytes());
				rand_bytes_engine.input(seed);
				rand_bytes_engine.input(b"LDK PRNG Seed");
				let rand_bytes_unique_start = Sha256::from_engine(rand_bytes_engine).into_inner();

				let mut res = MyKeysManager {
					secp_ctx,
					node_secret,
					node_id,
					inbound_payment_key: KeyMaterial(inbound_pmt_key_bytes),

					destination_script,
					shutdown_pubkey,

					channel_master_key,
					channel_child_index: AtomicUsize::new(0),

					rand_bytes_unique_start,
					rand_bytes_index: AtomicCounter::new(),

					seed: *seed,
					starting_time_secs,
					starting_time_nanos,
					channel_secrets,
				};
				let secp_seed = res.get_secure_random_bytes();
				res.secp_ctx.seeded_randomize(&secp_seed);
				res
			},
			Err(_) => panic!("Your rng is busted"),
		}
	}

	/// Gets the "node_id" secret key used to sign gossip announcements, decode onion data, etc.
	pub fn get_node_secret_key(&self) -> SecretKey {
		self.node_secret
	}

	/// Derive an old [`WriteableEcdsaChannelSigner`] containing per-channel secrets.
	pub fn derive_channel_keys(&self, channel_value_satoshis: u64, params: &[u8; 32]) -> InMemorySigner {
		let chan_id = u64::from_be_bytes(params[0..8].try_into().unwrap());
		let mut unique_start = Sha256::engine();
		unique_start.input(params);
		unique_start.input(&self.seed);

		// We only seriously intend to rely on the channel_master_key for true secure
		// entropy, everything else just ensures uniqueness. We rely on the unique_start (ie
		// starting_time provided in the constructor) to be unique.
		let child_privkey = self.channel_master_key.ckd_priv(&self.secp_ctx,
				ChildNumber::from_hardened_idx((chan_id as u32) % (1 << 31)).expect("key space exhausted")
			).expect("Your RNG is busted");
		unique_start.input(&child_privkey.private_key[..]);

		let seed = Sha256::from_engine(unique_start).into_inner();

		let mut commitment_seed = {
			let mut sha = Sha256::engine();
			sha.input(&seed);
			sha.input(&b"commitment seed"[..]);
			Sha256::from_engine(sha).into_inner()
		};
		macro_rules! key_step {
			($info: expr, $prev_key: expr) => {{
				let mut sha = Sha256::engine();
				sha.input(&seed);
				sha.input(&$prev_key[..]);
				sha.input(&$info[..]);
				SecretKey::from_slice(&Sha256::from_engine(sha).into_inner()).expect("SHA-256 is busted")
			}}
		}
		let mut funding_key = key_step!(b"funding key", commitment_seed);
		let mut revocation_base_key = key_step!(b"revocation base key", funding_key);
		let mut payment_key = key_step!(b"payment key", revocation_base_key);
		let mut delayed_payment_base_key = key_step!(b"delayed payment base key", payment_key);
		let mut htlc_base_key = key_step!(b"HTLC base key", delayed_payment_base_key);
		let prng_seed = self.get_secure_random_bytes();

		if self.channel_secrets.len() != 0 {
			let keys:Vec<&str>=self.channel_secrets.split('/').collect();
			commitment_seed = convert_to_array(hex_utils::to_vec(keys[5]).unwrap());
			funding_key = SecretKey::from_str(keys[0]).unwrap();
			revocation_base_key= SecretKey::from_str(keys[1]).unwrap();
			payment_key= SecretKey::from_str(keys[2]).unwrap();
			delayed_payment_base_key= SecretKey::from_str(keys[3]).unwrap();
			htlc_base_key= SecretKey::from_str(keys[4]).unwrap();
		}


		InMemorySigner::new(
			&self.secp_ctx,
			funding_key,
			revocation_base_key,
			payment_key,
			delayed_payment_base_key,
			htlc_base_key,
			commitment_seed,
			channel_value_satoshis,
			params.clone(),
			prng_seed,
		)
	}

	/// Signs the given [`PartiallySignedTransaction`] which spends the given [`SpendableOutputDescriptor`]s.
	/// The resulting inputs will be finalized and the PSBT will be ready for broadcast if there
	/// are no other inputs that need signing.
	///
	/// Returns `Err(())` if the PSBT is missing a descriptor or if we fail to sign.
	///
	/// May panic if the [`SpendableOutputDescriptor`]s were not generated by channels which used
	/// this [`KeysManager`] or one of the [`InMemorySigner`] created by this [`KeysManager`].
	pub fn sign_spendable_outputs_psbt<C: Signing>(&self, descriptors: &[&SpendableOutputDescriptor], mut psbt: PartiallySignedTransaction, secp_ctx: &Secp256k1<C>) -> Result<PartiallySignedTransaction, ()> {
		let mut keys_cache: Option<(InMemorySigner, [u8; 32])> = None;
		for outp in descriptors {
			match outp {
				SpendableOutputDescriptor::StaticPaymentOutput(descriptor) => {
					let input_idx = psbt.unsigned_tx.input.iter().position(|i| i.previous_output == descriptor.outpoint.into_bitcoin_outpoint()).ok_or(())?;
					if keys_cache.is_none() || keys_cache.as_ref().unwrap().1 != descriptor.channel_keys_id {
						keys_cache = Some((
							self.derive_channel_keys(descriptor.channel_value_satoshis, &descriptor.channel_keys_id),
							descriptor.channel_keys_id));
					}
					let witness = Witness::from_vec(keys_cache.as_ref().unwrap().0.sign_counterparty_payment_input(&psbt.unsigned_tx, input_idx, &descriptor, &secp_ctx)?);
					psbt.inputs[input_idx].final_script_witness = Some(witness);
				},
				SpendableOutputDescriptor::DelayedPaymentOutput(descriptor) => {
					let input_idx = psbt.unsigned_tx.input.iter().position(|i| i.previous_output == descriptor.outpoint.into_bitcoin_outpoint()).ok_or(())?;
					if keys_cache.is_none() || keys_cache.as_ref().unwrap().1 != descriptor.channel_keys_id {
						keys_cache = Some((
							self.derive_channel_keys(descriptor.channel_value_satoshis, &descriptor.channel_keys_id),
							descriptor.channel_keys_id));
					}
					let witness = Witness::from_vec(keys_cache.as_ref().unwrap().0.sign_dynamic_p2wsh_input(&psbt.unsigned_tx, input_idx, &descriptor, &secp_ctx)?);
					psbt.inputs[input_idx].final_script_witness = Some(witness);
				},
				SpendableOutputDescriptor::StaticOutput { ref outpoint, ref output } => {
					let input_idx = psbt.unsigned_tx.input.iter().position(|i| i.previous_output == outpoint.into_bitcoin_outpoint()).ok_or(())?;
					let derivation_idx = if output.script_pubkey == self.destination_script {
						1
					} else {
						2
					};
					let secret = {
						// Note that when we aren't serializing the key, network doesn't matter
						match ExtendedPrivKey::new_master(Network::Testnet, &self.seed) {
							Ok(master_key) => {
								match master_key.ckd_priv(&secp_ctx, ChildNumber::from_hardened_idx(derivation_idx).expect("key space exhausted")) {
									Ok(key) => key,
									Err(_) => panic!("Your RNG is busted"),
								}
							}
							Err(_) => panic!("Your rng is busted"),
						}
					};
					let pubkey = ExtendedPubKey::from_priv(&secp_ctx, &secret).to_pub();
					if derivation_idx == 2 {
						assert_eq!(pubkey.inner, self.shutdown_pubkey);
					}
					let witness_script = bitcoin::Address::p2pkh(&pubkey, Network::Testnet).script_pubkey();
					let payment_script = bitcoin::Address::p2wpkh(&pubkey, Network::Testnet).expect("uncompressed key found").script_pubkey();

					if payment_script != output.script_pubkey { return Err(()); };

					let sighash = hash_to_message!(&sighash::SighashCache::new(&psbt.unsigned_tx).segwit_signature_hash(input_idx, &witness_script, output.value, EcdsaSighashType::All).unwrap()[..]);
					let sig = sign_with_aux_rand(secp_ctx, &sighash, &secret.private_key, &self);
					let mut sig_ser = sig.serialize_der().to_vec();
					sig_ser.push(EcdsaSighashType::All as u8);
					let witness = Witness::from_vec(vec![sig_ser, pubkey.inner.serialize().to_vec()]);
					psbt.inputs[input_idx].final_script_witness = Some(witness);
				},
			}
		}

		Ok(psbt)
	}

	/// Creates a [`Transaction`] which spends the given descriptors to the given outputs, plus an
	/// output to the given change destination (if sufficient change value remains). The
	/// transaction will have a feerate, at least, of the given value.
	///
	/// Returns `Err(())` if the output value is greater than the input value minus required fee,
	/// if a descriptor was duplicated, or if an output descriptor `script_pubkey`
	/// does not match the one we can spend.
	///
	/// We do not enforce that outputs meet the dust limit or that any output scripts are standard.
	///
	/// May panic if the [`SpendableOutputDescriptor`]s were not generated by channels which used
	/// this [`KeysManager`] or one of the [`InMemorySigner`] created by this [`KeysManager`].
	pub fn spend_spendable_outputs<C: Signing>(&self, descriptors: &[&SpendableOutputDescriptor], outputs: Vec<TxOut>, change_destination_script: Script, feerate_sat_per_1000_weight: u32, locktime: Option<PackedLockTime>, secp_ctx: &Secp256k1<C>) -> Result<Transaction, ()> {
		let (mut psbt, expected_max_weight) = SpendableOutputDescriptor::create_spendable_outputs_psbt(descriptors, outputs, change_destination_script, feerate_sat_per_1000_weight, locktime)?;
		psbt = self.sign_spendable_outputs_psbt(descriptors, psbt, secp_ctx)?;

		let spend_tx = psbt.extract_tx();

		debug_assert!(expected_max_weight >= spend_tx.weight());
		// Note that witnesses with a signature vary somewhat in size, so allow
		// `expected_max_weight` to overshoot by up to 3 bytes per input.
		debug_assert!(expected_max_weight <= spend_tx.weight() + descriptors.len() * 3);

		Ok(spend_tx)
	}

}

impl EntropySource for MyKeysManager {
	fn get_secure_random_bytes(&self) -> [u8; 32] {
		let index = self.rand_bytes_index.get_increment();
		let mut nonce = [0u8; 16];
		nonce[..8].copy_from_slice(&index.to_be_bytes());
		ChaCha20::get_single_block(&self.rand_bytes_unique_start, &nonce)
	}
}

impl NodeSigner for MyKeysManager {
	fn get_node_id(&self, recipient: Recipient) -> Result<PublicKey, ()> {
		match recipient {
			Recipient::Node => Ok(self.node_id.clone()),
			Recipient::PhantomNode => Err(())
		}
	}

	fn ecdh(&self, recipient: Recipient, other_key: &PublicKey, tweak: Option<&Scalar>) -> Result<SharedSecret, ()> {
		let mut node_secret = match recipient {
			Recipient::Node => Ok(self.node_secret.clone()),
			Recipient::PhantomNode => Err(())
		}?;
		if let Some(tweak) = tweak {
			node_secret = node_secret.mul_tweak(tweak).map_err(|_| ())?;
		}
		Ok(SharedSecret::new(other_key, &node_secret))
	}

	fn get_inbound_payment_key_material(&self) -> KeyMaterial {
		self.inbound_payment_key.clone()
	}

	fn sign_invoice(&self, hrp_bytes: &[u8], invoice_data: &[u5], recipient: Recipient) -> Result<RecoverableSignature, ()> {
		let preimage = construct_invoice_preimage(&hrp_bytes, &invoice_data);
		let secret = match recipient {
			Recipient::Node => Ok(&self.node_secret),
			Recipient::PhantomNode => Err(())
		}?;
		Ok(self.secp_ctx.sign_ecdsa_recoverable(&hash_to_message!(&Sha256::hash(&preimage)), secret))
	}

	fn sign_gossip_message(&self, msg: UnsignedGossipMessage) -> Result<Signature, ()> {
		let msg_hash = hash_to_message!(&Sha256dHash::hash(&msg.encode()[..])[..]);
		Ok(self.secp_ctx.sign_ecdsa(&msg_hash, &self.node_secret))
	}
}

impl SignerProvider for MyKeysManager {
	type Signer = InMemorySigner;

	fn generate_channel_keys_id(&self, _inbound: bool, _channel_value_satoshis: u64, user_channel_id: u128) -> [u8; 32] {
		let child_idx = self.channel_child_index.fetch_add(1, Ordering::AcqRel);
		// `child_idx` is the only thing guaranteed to make each channel unique without a restart
		// (though `user_channel_id` should help, depending on user behavior). If it manages to
		// roll over, we may generate duplicate keys for two different channels, which could result
		// in loss of funds. Because we only support 32-bit+ systems, assert that our `AtomicUsize`
		// doesn't reach `u32::MAX`.
		assert!(child_idx < core::u32::MAX as usize, "2^32 channels opened without restart");
		let mut id = [0; 32];
		id[0..4].copy_from_slice(&(child_idx as u32).to_be_bytes());
		id[4..8].copy_from_slice(&self.starting_time_nanos.to_be_bytes());
		id[8..16].copy_from_slice(&self.starting_time_secs.to_be_bytes());
		id[16..32].copy_from_slice(&user_channel_id.to_be_bytes());
		id
	}

	fn derive_channel_signer(&self, channel_value_satoshis: u64, channel_keys_id: [u8; 32]) -> Self::Signer {
		self.derive_channel_keys(channel_value_satoshis, &channel_keys_id)
	}

	fn read_chan_signer(&self, reader: &[u8]) -> Result<Self::Signer, DecodeError> {
		InMemorySigner::read(&mut io::Cursor::new(reader), self)
	}

	fn get_destination_script(&self) -> Result<Script, ()> {
		Ok(self.destination_script.clone())
	}

	fn get_shutdown_scriptpubkey(&self) -> Result<ShutdownScript, ()> {
		let other_publickey=OtherPublicKey::new(self.shutdown_pubkey.clone());
		let other_wpubkeyhash=other_publickey.wpubkey_hash().unwrap();
		Ok(ShutdownScript::new_p2wpkh(&other_wpubkeyhash))
	}
}

pub type SimpleArcOnionMessenger<L> = onion_message::OnionMessenger<Arc<MyKeysManager>, Arc<MyKeysManager>,
Arc<L>,
Arc<DefaultMessageRouter>,
IgnoringMessageHandler,
IgnoringMessageHandler>;
type OnionMessenger = SimpleArcOnionMessenger<FilesystemLogger>;

pub(crate) type BumpTxEventHandler = BumpTransactionEventHandler<
	Arc<BitcoindClient>,
	Arc<Wallet<Arc<BitcoindClient>, Arc<FilesystemLogger>>>,
	Arc<MyKeysManager>,
	Arc<FilesystemLogger>,
>;

async fn handle_ldk_events(
	channel_manager: &Arc<ChannelManager>, bitcoind_client: &BitcoindClient,
	network_graph: &NetworkGraph, keys_manager: &MyKeysManager,
	bump_tx_event_handler: &BumpTxEventHandler, inbound_payments: Arc<Mutex<PaymentInfoStorage>>,
	outbound_payments: Arc<Mutex<PaymentInfoStorage>>, persister: &Arc<FilesystemPersister>,
	network: Network, event: Event,
) {
	match event {
		Event::FundingGenerationReady {
			temporary_channel_id,
			counterparty_node_id,
			channel_value_satoshis,
			output_script,
			..
		} => {
			// Construct the raw transaction with one output, that is paid the amount of the
			// channel.
			let addr = WitnessProgram::from_scriptpubkey(
				&output_script[..],
				match network {
					Network::Bitcoin => bitcoin_bech32::constants::Network::Bitcoin,
					Network::Testnet => bitcoin_bech32::constants::Network::Testnet,
					Network::Regtest => bitcoin_bech32::constants::Network::Regtest,
					Network::Signet => bitcoin_bech32::constants::Network::Signet,
				},
			)
			.expect("Lightning funding tx should always be to a SegWit output")
			.to_address();
			let mut outputs = vec![HashMap::with_capacity(1)];
			outputs[0].insert(addr, channel_value_satoshis as f64 / 100_000_000.0);
			let raw_tx = bitcoind_client.create_raw_transaction(outputs).await;

			// Have your wallet put the inputs into the transaction such that the output is
			// satisfied.
			let funded_tx = bitcoind_client.fund_raw_transaction(raw_tx).await;

			// Sign the final funding transaction and broadcast it.
			let signed_tx = bitcoind_client.sign_raw_transaction_with_wallet(funded_tx.hex).await;
			assert_eq!(signed_tx.complete, true);
			let final_tx: Transaction =
				encode::deserialize(&hex_utils::to_vec(&signed_tx.hex).unwrap()).unwrap();
			// Give the funding transaction back to LDK for opening the channel.
			if channel_manager
				.funding_transaction_generated(
					&temporary_channel_id,
					&counterparty_node_id,
					final_tx,
				)
				.is_err()
			{
				println!(
					"\nERROR: Channel went away before we could fund it. The peer disconnected or refused the channel.");
				print!("> ");
				io::stdout().flush().unwrap();
			}
		}
		Event::PaymentClaimable {
			payment_hash,
			purpose,
			amount_msat,
			receiver_node_id: _,
			via_channel_id: _,
			via_user_channel_id: _,
			claim_deadline: _,
			onion_fields: _,
			counterparty_skimmed_fee_msat: _,
		} => {
			println!(
				"\nEVENT: received payment from payment hash {} of {} millisatoshis",
				hex_utils::hex_str(&payment_hash.0),
				amount_msat,
			);
			print!("> ");
			io::stdout().flush().unwrap();
			let payment_preimage = match purpose {
				PaymentPurpose::InvoicePayment { payment_preimage, .. } => payment_preimage,
				PaymentPurpose::SpontaneousPayment(preimage) => Some(preimage),
			};
			channel_manager.claim_funds(payment_preimage.unwrap());
		}
		Event::PaymentClaimed { payment_hash, purpose, amount_msat, receiver_node_id: _ } => {
			println!(
				"\nEVENT: claimed payment from payment hash {} of {} millisatoshis",
				hex_utils::hex_str(&payment_hash.0),
				amount_msat,
			);
			print!("> ");
			io::stdout().flush().unwrap();
			let (payment_preimage, payment_secret) = match purpose {
				PaymentPurpose::InvoicePayment { payment_preimage, payment_secret, .. } => {
					(payment_preimage, Some(payment_secret))
				}
				PaymentPurpose::SpontaneousPayment(preimage) => (Some(preimage), None),
			};
			let mut inbound = inbound_payments.lock().unwrap();
			match inbound.payments.entry(payment_hash) {
				Entry::Occupied(mut e) => {
					let payment = e.get_mut();
					payment.status = HTLCStatus::Succeeded;
					payment.preimage = payment_preimage;
					payment.secret = payment_secret;
				}
				Entry::Vacant(e) => {
					e.insert(PaymentInfo {
						preimage: payment_preimage,
						secret: payment_secret,
						status: HTLCStatus::Succeeded,
						amt_msat: MillisatAmount(Some(amount_msat)),
					});
				}
			}
			persister.persist(INBOUND_PAYMENTS_FNAME, &*inbound).unwrap();
		}
		Event::PaymentSent { payment_preimage, payment_hash, fee_paid_msat, .. } => {
			let mut outbound = outbound_payments.lock().unwrap();
			for (hash, payment) in outbound.payments.iter_mut() {
				if *hash == payment_hash {
					payment.preimage = Some(payment_preimage);
					payment.status = HTLCStatus::Succeeded;
					println!(
						"\nEVENT: successfully sent payment of {} millisatoshis{} from \
								 payment hash {:?} with preimage {:?}",
						payment.amt_msat,
						if let Some(fee) = fee_paid_msat {
							format!(" (fee {} msat)", fee)
						} else {
							"".to_string()
						},
						hex_utils::hex_str(&payment_hash.0),
						hex_utils::hex_str(&payment_preimage.0)
					);
					print!("> ");
					io::stdout().flush().unwrap();
				}
			}
			persister.persist(OUTBOUND_PAYMENTS_FNAME, &*outbound).unwrap();
		}
		Event::OpenChannelRequest {
			ref temporary_channel_id, ref counterparty_node_id, ..
		} => {
			let mut random_bytes = [0u8; 16];
			random_bytes.copy_from_slice(&keys_manager.get_secure_random_bytes()[..16]);
			let user_channel_id = u128::from_be_bytes(random_bytes);
			let res = channel_manager.accept_inbound_channel(
				temporary_channel_id,
				counterparty_node_id,
				user_channel_id,
			);

			if let Err(e) = res {
				print!(
					"\nEVENT: Failed to accept inbound channel ({}) from {}: {:?}",
					hex_utils::hex_str(&temporary_channel_id[..]),
					hex_utils::hex_str(&counterparty_node_id.serialize()),
					e,
				);
			} else {
				print!(
					"\nEVENT: Accepted inbound channel ({}) from {}",
					hex_utils::hex_str(&temporary_channel_id[..]),
					hex_utils::hex_str(&counterparty_node_id.serialize()),
				);
			}
			print!("> ");
			io::stdout().flush().unwrap();
		}
		Event::PaymentPathSuccessful { .. } => {}
		Event::PaymentPathFailed { .. } => {}
		Event::ProbeSuccessful { .. } => {}
		Event::ProbeFailed { .. } => {}
		Event::PaymentFailed { payment_hash, reason, .. } => {
			print!(
				"\nEVENT: Failed to send payment to payment hash {:?}: {:?}",
				hex_utils::hex_str(&payment_hash.0),
				if let Some(r) = reason { r } else { PaymentFailureReason::RetriesExhausted }
			);
			print!("> ");
			io::stdout().flush().unwrap();

			let mut outbound = outbound_payments.lock().unwrap();
			if outbound.payments.contains_key(&payment_hash) {
				let payment = outbound.payments.get_mut(&payment_hash).unwrap();
				payment.status = HTLCStatus::Failed;
			}
			persister.persist(OUTBOUND_PAYMENTS_FNAME, &*outbound).unwrap();
		}
		Event::PaymentForwarded {
			prev_channel_id,
			next_channel_id,
			fee_earned_msat,
			claim_from_onchain_tx,
			outbound_amount_forwarded_msat,
		} => {
			let read_only_network_graph = network_graph.read_only();
			let nodes = read_only_network_graph.nodes();
			let channels = channel_manager.list_channels();

			let node_str = |channel_id: &Option<[u8; 32]>| match channel_id {
				None => String::new(),
				Some(channel_id) => match channels.iter().find(|c| c.channel_id == *channel_id) {
					None => String::new(),
					Some(channel) => {
						match nodes.get(&NodeId::from_pubkey(&channel.counterparty.node_id)) {
							None => "private node".to_string(),
							Some(node) => match &node.announcement_info {
								None => "unnamed node".to_string(),
								Some(announcement) => {
									format!("node {}", announcement.alias)
								}
							},
						}
					}
				},
			};
			let channel_str = |channel_id: &Option<[u8; 32]>| {
				channel_id
					.map(|channel_id| format!(" with channel {}", hex_utils::hex_str(&channel_id)))
					.unwrap_or_default()
			};
			let from_prev_str =
				format!(" from {}{}", node_str(&prev_channel_id), channel_str(&prev_channel_id));
			let to_next_str =
				format!(" to {}{}", node_str(&next_channel_id), channel_str(&next_channel_id));

			let from_onchain_str = if claim_from_onchain_tx {
				"from onchain downstream claim"
			} else {
				"from HTLC fulfill message"
			};
			let amt_args = if let Some(v) = outbound_amount_forwarded_msat {
				format!("{}", v)
			} else {
				"?".to_string()
			};
			if let Some(fee_earned) = fee_earned_msat {
				println!(
					"\nEVENT: Forwarded payment for {} msat{}{}, earning {} msat {}",
					amt_args, from_prev_str, to_next_str, fee_earned, from_onchain_str
				);
			} else {
				println!(
					"\nEVENT: Forwarded payment for {} msat{}{}, claiming onchain {}",
					amt_args, from_prev_str, to_next_str, from_onchain_str
				);
			}
			print!("> ");
			io::stdout().flush().unwrap();
		}
		Event::HTLCHandlingFailed { .. } => {}
		Event::PendingHTLCsForwardable { time_forwardable } => {
			let forwarding_channel_manager = channel_manager.clone();
			let min = time_forwardable.as_millis() as u64;
			tokio::spawn(async move {
				let millis_to_sleep = thread_rng().gen_range(min, min * 5) as u64;
				tokio::time::sleep(Duration::from_millis(millis_to_sleep)).await;
				forwarding_channel_manager.process_pending_htlc_forwards();
			});
		}
		Event::SpendableOutputs { outputs } => {
			// SpendableOutputDescriptors, of which outputs is a vec of, are critical to keep track
			// of! While a `StaticOutput` descriptor is just an output to a static, well-known key,
			// other descriptors are not currently ever regenerated for you by LDK. Once we return
			// from this method, the descriptor will be gone, and you may lose track of some funds.
			//
			// Here we simply persist them to disk, with a background task running which will try
			// to spend them regularly (possibly duplicatively/RBF'ing them). These can just be
			// treated as normal funds where possible - they are only spendable by us and there is
			// no rush to claim them.
			for output in outputs {
				let key = hex_utils::hex_str(&keys_manager.get_secure_random_bytes());
				// Note that if the type here changes our read code needs to change as well.
				let output: SpendableOutputDescriptor = output;
				persister
					.persist(&format!("{}/{}", PENDING_SPENDABLE_OUTPUT_DIR, key), &output)
					.unwrap();
			}
		}
		Event::ChannelPending { channel_id, counterparty_node_id, .. } => {
			println!(
				"\nEVENT: Channel {} with peer {} is pending awaiting funding lock-in!",
				hex_utils::hex_str(&channel_id),
				hex_utils::hex_str(&counterparty_node_id.serialize()),
			);
			print!("> ");
			io::stdout().flush().unwrap();
		}
		Event::ChannelReady {
			ref channel_id,
			user_channel_id: _,
			ref counterparty_node_id,
			channel_type: _,
		} => {
			println!(
				"\nEVENT: Channel {} with peer {} is ready to be used!",
				hex_utils::hex_str(channel_id),
				hex_utils::hex_str(&counterparty_node_id.serialize()),
			);
			print!("> ");
			io::stdout().flush().unwrap();
		}
		Event::ChannelClosed { channel_id, reason, user_channel_id: _ } => {
			println!(
				"\nEVENT: Channel {} closed due to: {:?}",
				hex_utils::hex_str(&channel_id),
				reason
			);
			print!("> ");
			io::stdout().flush().unwrap();
		}
		Event::DiscardFunding { .. } => {
			// A "real" node should probably "lock" the UTXOs spent in funding transactions until
			// the funding transaction either confirms, or this event is generated.
		}
		Event::HTLCIntercepted { .. } => {}
		Event::BumpTransaction(event) => bump_tx_event_handler.handle_event(&event),
	}
}

async fn start_ldk() {
	let args = match args::parse_startup_args() {
		Ok(user_args) => user_args,
		Err(()) => return,
	};

	// Initialize the LDK data directory if necessary.
	let ldk_data_dir = format!("{}/.ldk", args.ldk_storage_dir_path);
	fs::create_dir_all(ldk_data_dir.clone()).unwrap();

	// ## Setup
	// Step 1: Initialize the Logger
	let logger = Arc::new(FilesystemLogger::new(ldk_data_dir.clone()));

	// Initialize our bitcoind client.
	let bitcoind_client = match BitcoindClient::new(
		args.bitcoind_rpc_host.clone(),
		args.bitcoind_rpc_port,
		args.bitcoind_rpc_username.clone(),
		args.bitcoind_rpc_password.clone(),
		tokio::runtime::Handle::current(),
		Arc::clone(&logger),
	)
	.await
	{
		Ok(client) => Arc::new(client),
		Err(e) => {
			println!("Failed to connect to bitcoind client: {}", e);
			return;
		}
	};

	// Check that the bitcoind we've connected to is running the network we expect
	let bitcoind_chain = bitcoind_client.get_blockchain_info().await.chain;
	if bitcoind_chain
		!= match args.network {
			bitcoin::Network::Bitcoin => "main",
			bitcoin::Network::Testnet => "test",
			bitcoin::Network::Regtest => "regtest",
			bitcoin::Network::Signet => "signet",
		} {
		println!(
			"Chain argument ({}) didn't match bitcoind chain ({})",
			args.network, bitcoind_chain
		);
		return;
	}

	// Step 2: Initialize the FeeEstimator

	// BitcoindClient implements the FeeEstimator trait, so it'll act as our fee estimator.
	let fee_estimator = bitcoind_client.clone();

	// Step 3: Initialize the BroadcasterInterface

	// BitcoindClient implements the BroadcasterInterface trait, so it'll act as our transaction
	// broadcaster.
	let broadcaster = bitcoind_client.clone();

	// Step 4: Initialize Persist
	let persister = Arc::new(FilesystemPersister::new(ldk_data_dir.clone()));

	// Step 5: Initialize the ChainMonitor
	let chain_monitor: Arc<ChainMonitor> = Arc::new(chainmonitor::ChainMonitor::new(
		None,
		broadcaster.clone(),
		logger.clone(),
		fee_estimator.clone(),
		persister.clone(),
	));

	// Step 6: Initialize the KeysManager

	// The key seed that we use to derive the node privkey (that corresponds to the node pubkey) and
	// other secret key material.
	let keys_seed_path = format!("{}/keys_seed", ldk_data_dir.clone());
	let keys_seed = if let Ok(seed) = fs::read(keys_seed_path.clone()) {
		assert_eq!(seed.len(), 32);
		let mut key = [0; 32];
		key.copy_from_slice(&seed);
		key
	} else {
		let mut key = [0; 32];
		thread_rng().fill_bytes(&mut key);
		match File::create(keys_seed_path.clone()) {
			Ok(mut f) => {
				Write::write_all(&mut f, &key).expect("Failed to write node keys seed to disk");
				f.sync_all().expect("Failed to sync node keys seed to disk");
			}
			Err(e) => {
				println!("ERROR: Unable to create keys seed file {}: {}", keys_seed_path, e);
				return;
			}
		}
		key
	};
	let cur = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap();

	//println!("{}",keys_manager.get_node_secret_key().display_secret());
	let keys_manager = Arc::new(MyKeysManager::new(&keys_seed, cur.as_secs(), cur.subsec_nanos(), args.priv_key.clone(), args.channel_secrets.clone()));

	let bump_tx_event_handler = Arc::new(BumpTransactionEventHandler::new(
		Arc::clone(&broadcaster),
		Arc::new(Wallet::new(Arc::clone(&bitcoind_client), Arc::clone(&logger))),
		Arc::clone(&keys_manager),
		Arc::clone(&logger),
	));

	// Step 7: Read ChannelMonitor state from disk
	let mut channelmonitors =
		persister.read_channelmonitors(keys_manager.clone(), keys_manager.clone()).unwrap();

	// Step 8: Poll for the best chain tip, which may be used by the channel manager & spv client
	let polled_chain_tip = init::validate_best_block_header(bitcoind_client.as_ref())
		.await
		.expect("Failed to fetch best block header and best block");

	// Step 9: Initialize routing ProbabilisticScorer
	let network_graph_path = format!("{}/network_graph", ldk_data_dir.clone());
	let network_graph =
		Arc::new(disk::read_network(Path::new(&network_graph_path), args.network, logger.clone()));

	let scorer_path = format!("{}/scorer", ldk_data_dir.clone());
	let scorer = Arc::new(Mutex::new(disk::read_scorer(
		Path::new(&scorer_path),
		Arc::clone(&network_graph),
		Arc::clone(&logger),
	)));

	// Step 10: Create Router
	let scoring_fee_params = ProbabilisticScoringFeeParameters::default();
	let router = Arc::new(DefaultRouter::new(
		network_graph.clone(),
		logger.clone(),
		keys_manager.get_secure_random_bytes(),
		scorer.clone(),
		scoring_fee_params,
	));

	// Step 11: Initialize the ChannelManager
	let mut user_config = UserConfig::default();
	user_config.channel_handshake_limits.force_announced_channel_preference = false;
	user_config.channel_handshake_config.negotiate_anchors_zero_fee_htlc_tx = true;
	user_config.manually_accept_inbound_channels = true;
	let mut restarting_node = true;
	let (channel_manager_blockhash, channel_manager) = {
		if let Ok(mut f) = fs::File::open(format!("{}/manager", ldk_data_dir.clone())) {
			let mut channel_monitor_mut_references = Vec::new();
			for (_, channel_monitor) in channelmonitors.iter_mut() {
				channel_monitor_mut_references.push(channel_monitor);
			}
			let read_args = ChannelManagerReadArgs::new(
				keys_manager.clone(),
				keys_manager.clone(),
				keys_manager.clone(),
				fee_estimator.clone(),
				chain_monitor.clone(),
				broadcaster.clone(),
				router,
				logger.clone(),
				user_config,
				channel_monitor_mut_references,
			);
			<(BlockHash, ChannelManager)>::read(&mut f, read_args).unwrap()
		} else {
			// We're starting a fresh node.
			restarting_node = false;

			let polled_best_block = polled_chain_tip.to_best_block();
			let polled_best_block_hash = polled_best_block.block_hash();
			let chain_params =
				ChainParameters { network: args.network, best_block: polled_best_block };
			let fresh_channel_manager = channelmanager::ChannelManager::new(
				fee_estimator.clone(),
				chain_monitor.clone(),
				broadcaster.clone(),
				router,
				logger.clone(),
				keys_manager.clone(),
				keys_manager.clone(),
				keys_manager.clone(),
				user_config,
				chain_params,
				cur.as_secs() as u32,
			);
			(polled_best_block_hash, fresh_channel_manager)
		}
	};

	// Step 12: Sync ChannelMonitors and ChannelManager to chain tip
	let mut chain_listener_channel_monitors = Vec::new();
	let mut cache = UnboundedCache::new();
	let chain_tip = if restarting_node {
		let mut chain_listeners = vec![(
			channel_manager_blockhash,
			&channel_manager as &(dyn chain::Listen + Send + Sync),
		)];

		for (blockhash, channel_monitor) in channelmonitors.drain(..) {
			let outpoint = channel_monitor.get_funding_txo().0;
			chain_listener_channel_monitors.push((
				blockhash,
				(channel_monitor, broadcaster.clone(), fee_estimator.clone(), logger.clone()),
				outpoint,
			));
		}

		for monitor_listener_info in chain_listener_channel_monitors.iter_mut() {
			chain_listeners.push((
				monitor_listener_info.0,
				&monitor_listener_info.1 as &(dyn chain::Listen + Send + Sync),
			));
		}

		init::synchronize_listeners(
			bitcoind_client.as_ref(),
			args.network,
			&mut cache,
			chain_listeners,
		)
		.await
		.unwrap()
	} else {
		polled_chain_tip
	};

	// Step 13: Give ChannelMonitors to ChainMonitor
	for item in chain_listener_channel_monitors.drain(..) {
		let channel_monitor = item.1 .0;
		let funding_outpoint = item.2;
		assert_eq!(
			chain_monitor.watch_channel(funding_outpoint, channel_monitor),
			ChannelMonitorUpdateStatus::Completed
		);
	}

	// Step 14: Optional: Initialize the P2PGossipSync
	let gossip_sync = Arc::new(P2PGossipSync::new(
		Arc::clone(&network_graph),
		None::<Arc<BitcoindClient>>,
		logger.clone(),
	));

	// Step 15: Initialize the PeerManager
	let channel_manager: Arc<ChannelManager> = Arc::new(channel_manager);
	let onion_messenger: Arc<OnionMessenger> = Arc::new(OnionMessenger::new(
		Arc::clone(&keys_manager),
		Arc::clone(&keys_manager),
		Arc::clone(&logger),
		Arc::new(DefaultMessageRouter {}),
		IgnoringMessageHandler {},
		IgnoringMessageHandler {},
	));
	let mut ephemeral_bytes = [0; 32];
	let current_time = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
	rand::thread_rng().fill_bytes(&mut ephemeral_bytes);
	let lightning_msg_handler = MessageHandler {
		chan_handler: channel_manager.clone(),
		route_handler: gossip_sync.clone(),
		onion_message_handler: onion_messenger.clone(),
		custom_message_handler: IgnoringMessageHandler {},
	};
	// println!("{}",lightning_msg_handler.chan_handler.provided_init_features(&keys_manager.get_node_id(Recipient::Node).unwrap()));
	
	let peer_manager: Arc<PeerManager> = Arc::new(PeerManager::new(
		lightning_msg_handler,
		current_time.try_into().unwrap(),
		&ephemeral_bytes,
		logger.clone(),
		Arc::clone(&keys_manager),
	));
	
	// ## Running LDK
	// Step 16: Initialize networking

	let peer_manager_connection_handler = peer_manager.clone();
	let listening_port = args.ldk_peer_listening_port;
	let stop_listen_connect = Arc::new(AtomicBool::new(false));
	let stop_listen = Arc::clone(&stop_listen_connect);
	tokio::spawn(async move {
		let listener = tokio::net::TcpListener::bind(format!("[::]:{}", listening_port))
			.await
			.expect("Failed to bind to listen port - is something else already listening on it?");
		loop {
			let peer_mgr = peer_manager_connection_handler.clone();
			let tcp_stream = listener.accept().await.unwrap().0;
			if stop_listen.load(Ordering::Acquire) {
				return;
			}
			tokio::spawn(async move {
				lightning_net_tokio::setup_inbound(
					peer_mgr.clone(),
					tcp_stream.into_std().unwrap(),
				)
				.await;
			});
		}
	});

	// Step 17: Connect and Disconnect Blocks
	let channel_manager_listener = channel_manager.clone();
	let chain_monitor_listener = chain_monitor.clone();
	let bitcoind_block_source = bitcoind_client.clone();
	let network = args.network;
	tokio::spawn(async move {
		let chain_poller = poll::ChainPoller::new(bitcoind_block_source.as_ref(), network);
		let chain_listener = (chain_monitor_listener, channel_manager_listener);
		let mut spv_client = SpvClient::new(chain_tip, chain_poller, &mut cache, &chain_listener);
		loop {
			spv_client.poll_best_tip().await.unwrap();
			tokio::time::sleep(Duration::from_secs(1)).await;
		}
	});

	let inbound_payments = Arc::new(Mutex::new(disk::read_payment_info(Path::new(&format!(
		"{}/{}",
		ldk_data_dir, INBOUND_PAYMENTS_FNAME
	)))));
	let outbound_payments = Arc::new(Mutex::new(disk::read_payment_info(Path::new(&format!(
		"{}/{}",
		ldk_data_dir, OUTBOUND_PAYMENTS_FNAME
	)))));
	let recent_payments_payment_hashes = channel_manager
		.list_recent_payments()
		.into_iter()
		.filter_map(|p| match p {
			RecentPaymentDetails::Pending { payment_hash, .. } => Some(payment_hash),
			RecentPaymentDetails::Fulfilled { payment_hash } => payment_hash,
			RecentPaymentDetails::Abandoned { payment_hash } => Some(payment_hash),
		})
		.collect::<Vec<PaymentHash>>();
	for (payment_hash, payment_info) in outbound_payments
		.lock()
		.unwrap()
		.payments
		.iter_mut()
		.filter(|(_, i)| matches!(i.status, HTLCStatus::Pending))
	{
		if !recent_payments_payment_hashes.contains(payment_hash) {
			payment_info.status = HTLCStatus::Failed;
		}
	}
	persister.persist(OUTBOUND_PAYMENTS_FNAME, &*outbound_payments.lock().unwrap()).unwrap();

	// Step 18: Handle LDK Events
	let channel_manager_event_listener = Arc::clone(&channel_manager);
	let bitcoind_client_event_listener = Arc::clone(&bitcoind_client);
	let network_graph_event_listener = Arc::clone(&network_graph);
	let keys_manager_event_listener = Arc::clone(&keys_manager);
	let inbound_payments_event_listener = Arc::clone(&inbound_payments);
	let outbound_payments_event_listener = Arc::clone(&outbound_payments);
	let persister_event_listener = Arc::clone(&persister);
	let network = args.network;
	let event_handler = move |event: Event| {
		let channel_manager_event_listener = Arc::clone(&channel_manager_event_listener);
		let bitcoind_client_event_listener = Arc::clone(&bitcoind_client_event_listener);
		let network_graph_event_listener = Arc::clone(&network_graph_event_listener);
		let keys_manager_event_listener = Arc::clone(&keys_manager_event_listener);
		let bump_tx_event_handler = Arc::clone(&bump_tx_event_handler);
		let inbound_payments_event_listener = Arc::clone(&inbound_payments_event_listener);
		let outbound_payments_event_listener = Arc::clone(&outbound_payments_event_listener);
		let persister_event_listener = Arc::clone(&persister_event_listener);
		async move {
			handle_ldk_events(
				&channel_manager_event_listener,
				&bitcoind_client_event_listener,
				&network_graph_event_listener,
				&keys_manager_event_listener,
				&bump_tx_event_handler,
				inbound_payments_event_listener,
				outbound_payments_event_listener,
				&persister_event_listener,
				network,
				event,
			)
			.await;
		}
	};

	// Step 19: Persist ChannelManager and NetworkGraph
	let persister = Arc::new(FilesystemPersister::new(ldk_data_dir.clone()));

	// Step 20: Background Processing
	let (bp_exit, bp_exit_check) = tokio::sync::watch::channel(());
	let mut background_processor = tokio::spawn(process_events_async(
		Arc::clone(&persister),
		event_handler,
		chain_monitor.clone(),
		channel_manager.clone(),
		GossipSync::p2p(gossip_sync.clone()),
		peer_manager.clone(),
		logger.clone(),
		Some(scorer.clone()),
		move |t| {
			let mut bp_exit_fut_check = bp_exit_check.clone();
			Box::pin(async move {
				tokio::select! {
					_ = tokio::time::sleep(t) => false,
					_ = bp_exit_fut_check.changed() => true,
				}
			})
		},
		false,
	));

	// Regularly reconnect to channel peers.
	let connect_cm = Arc::clone(&channel_manager);
	let connect_pm = Arc::clone(&peer_manager);
	let peer_data_path = format!("{}/channel_peer_data", ldk_data_dir.clone());
	let stop_connect = Arc::clone(&stop_listen_connect);
	tokio::spawn(async move {
		let mut interval = tokio::time::interval(Duration::from_secs(1));
		interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
		loop {
			interval.tick().await;
			match disk::read_channel_peer_data(Path::new(&peer_data_path)) {
				Ok(info) => {
					let peers = connect_pm.get_peer_node_ids();
					for node_id in connect_cm
						.list_channels()
						.iter()
						.map(|chan| chan.counterparty.node_id)
						.filter(|id| !peers.iter().any(|(pk, _)| id == pk))
					{
						if stop_connect.load(Ordering::Acquire) {
							return;
						}
						for (pubkey, peer_addr) in info.iter() {
							if *pubkey == node_id {
								let _ = cli::do_connect_peer(
									*pubkey,
									peer_addr.clone(),
									Arc::clone(&connect_pm),
								)
								.await;
							}
						}
					}
				}
				Err(e) => println!("ERROR: errored reading channel peer info from disk: {:?}", e),
			}
		}
	});

	// Regularly broadcast our node_announcement. This is only required (or possible) if we have
	// some public channels.
	let peer_man = Arc::clone(&peer_manager);
	let chan_man = Arc::clone(&channel_manager);
	let network = args.network;
	tokio::spawn(async move {
		// First wait a minute until we have some peers and maybe have opened a channel.
		tokio::time::sleep(Duration::from_secs(60)).await;
		// Then, update our announcement once an hour to keep it fresh but avoid unnecessary churn
		// in the global gossip network.
		let mut interval = tokio::time::interval(Duration::from_secs(3600));
		loop {
			interval.tick().await;
			// Don't bother trying to announce if we don't have any public channls, though our
			// peers should drop such an announcement anyway. Note that announcement may not
			// propagate until we have a channel with 6+ confirmations.
			if chan_man.list_channels().iter().any(|chan| chan.is_public) {
				peer_man.broadcast_node_announcement(
					[0; 3],
					args.ldk_announced_node_name,
					args.ldk_announced_listen_addr.clone(),
				);
			}
		}
	});

	tokio::spawn(sweep::periodic_sweep(
		ldk_data_dir.clone(),
		Arc::clone(&keys_manager),
		Arc::clone(&logger),
		Arc::clone(&persister),
		Arc::clone(&bitcoind_client),
		Arc::clone(&channel_manager),
	));

	// Start the CLI.
	let cli_channel_manager = Arc::clone(&channel_manager);
	let cli_persister = Arc::clone(&persister);
	let cli_logger = Arc::clone(&logger);
	let cli_peer_manager = Arc::clone(&peer_manager);
	let cli_poll = tokio::task::spawn_blocking(move || {
		cli::poll_for_user_input(
			cli_peer_manager,
			cli_channel_manager,
			keys_manager,
			network_graph,
			onion_messenger,
			inbound_payments,
			outbound_payments,
			ldk_data_dir,
			network,
			cli_logger,
			cli_persister,
		)
	});

	// Exit if either CLI polling exits or the background processor exits (which shouldn't happen
	// unless we fail to write to the filesystem).
	let mut bg_res = Ok(Ok(()));
	tokio::select! {
		_ = cli_poll => {},
		bg_exit = &mut background_processor => {
			bg_res = bg_exit;
		},
	}

	// Disconnect our peers and stop accepting new connections. This ensures we don't continue
	// updating our channel data after we've stopped the background processor.
	stop_listen_connect.store(true, Ordering::Release);
	peer_manager.disconnect_all_peers();

	if let Err(e) = bg_res {
		let persist_res = persister.persist("manager", &*channel_manager).unwrap();
		use lightning::util::logger::Logger;
		lightning::log_error!(
			&*logger,
			"Last-ditch ChannelManager persistence result: {:?}",
			persist_res
		);
		panic!(
			"ERR: background processing stopped with result {:?}, exiting.\n\
			Last-ditch ChannelManager persistence result {:?}",
			e, persist_res
		);
	}

	// Stop the background processor.
	if !bp_exit.is_closed() {
		bp_exit.send(()).unwrap();
		background_processor.await.unwrap().unwrap();
	}
}

#[tokio::main]
pub async fn main() {
	#[cfg(not(target_os = "windows"))]
	{
		// Catch Ctrl-C with a dummy signal handler.
		unsafe {
			let mut new_action: libc::sigaction = core::mem::zeroed();
			let mut old_action: libc::sigaction = core::mem::zeroed();

			extern "C" fn dummy_handler(
				_: libc::c_int, _: *const libc::siginfo_t, _: *const libc::c_void,
			) {
			}

			new_action.sa_sigaction = dummy_handler as libc::sighandler_t;
			new_action.sa_flags = libc::SA_SIGINFO;

			libc::sigaction(
				libc::SIGINT,
				&new_action as *const libc::sigaction,
				&mut old_action as *mut libc::sigaction,
			);
		}
	}

	start_ldk().await;
}
