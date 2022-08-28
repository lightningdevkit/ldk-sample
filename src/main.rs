pub mod bitcoind_client;
mod cli;
mod convert;
mod disk;
mod hex_utils;

use crate::bitcoind_client::BitcoindClient;
use crate::disk::FilesystemLogger;
use bitcoin::blockdata::constants::genesis_block;
use bitcoin::blockdata::transaction::Transaction;
use bitcoin::consensus::encode;
use bitcoin::network::constants::Network;
use bitcoin::secp256k1::{Secp256k1, SecretKey};
use bitcoin::BlockHash;
use bitcoin_bech32::WitnessProgram;
use lightning::chain;
use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning::chain::chainmonitor;
use lightning::chain::keysinterface::{InMemorySigner, KeysInterface, KeysManager, Recipient, KeyMaterial, SpendableOutputDescriptor, StaticPaymentOutputDescriptor, DelayedPaymentOutputDescriptor};
use lightning::chain::{BestBlock, Filter, Watch};
use lightning::ln::channelmanager;
use lightning::ln::channelmanager::{
	ChainParameters, ChannelManagerReadArgs, SimpleArcChannelManager,
};
use lightning::ln::peer_handler::{IgnoringMessageHandler, MessageHandler, SimpleArcPeerManager};
use lightning::ln::{PaymentHash, PaymentPreimage, PaymentSecret};
use lightning::routing::gossip;
use lightning::routing::gossip::{NodeId, P2PGossipSync};
use lightning::routing::scoring::ProbabilisticScorer;
use lightning::util::config::UserConfig;
use lightning::util::events::{Event, PaymentPurpose};
use lightning::util::ser::ReadableArgs;
use lightning_background_processor::BackgroundProcessor;
use lightning_block_sync::init;
use lightning_block_sync::poll;
use lightning_block_sync::SpvClient;
use lightning_block_sync::UnboundedCache;
use lightning_invoice::payment;
use lightning_invoice::utils::DefaultRouter;
use lightning_net_tokio::SocketDescriptor;
use lightning_persister::FilesystemPersister;
use lightning_rapid_gossip_sync::RapidGossipSync;
use rand::{thread_rng, Rng};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::fs::File;
use std::io;
use std::io::Write;
use std::ops::Deref;
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};


use bitcoin::blockdata::transaction::{ TxOut, TxIn, EcdsaSighashType};
use bitcoin::blockdata::script::{Script, Builder};
use bitcoin::blockdata::opcodes;
use bitcoin::util::bip32::{ExtendedPrivKey, ExtendedPubKey, ChildNumber};
use bitcoin::util::sighash;

use bitcoin::bech32::u5;
use bitcoin::hashes::{Hash, HashEngine};
use bitcoin::hashes::sha256::HashEngine as Sha256State;
use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::hash_types::WPubkeyHash;

use bitcoin::secp256k1::{ PublicKey};
use bitcoin::secp256k1::{ Signing};
use bitcoin::secp256k1::ecdsa::RecoverableSignature;
use bitcoin::{secp256k1, Witness};

use bitcoin::consensus::Encodable;
use bitcoin::consensus::encode::VarInt;

use std::io_extras::sink;
use lightning::ln::script::ShutdownScript;


use core::sync::atomic::{AtomicUsize};
use lightning::ln::msgs::{DecodeError};
use lightning::util::invoice::construct_invoice_preimage;

use bitcoin::secp256k1::{Message, ecdsa::Signature};

const MAX_VALUE_MSAT: u64 = 21_000_000_0000_0000_000;
pub(crate) enum HTLCStatus {
	Pending,
	Succeeded,
	Failed,
}

pub(crate) struct MillisatAmount(Option<u64>);

impl fmt::Display for MillisatAmount {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match self.0 {
			Some(amt) => write!(f, "{}", amt),
			None => write!(f, "unknown"),
		}
	}
}

pub(crate) struct PaymentInfo {
	preimage: Option<PaymentPreimage>,
	secret: Option<PaymentSecret>,
	status: HTLCStatus,
	amt_msat: MillisatAmount,
}

pub(crate) type PaymentInfoStorage = Arc<Mutex<HashMap<PaymentHash, PaymentInfo>>>;

type ChainMonitor = chainmonitor::ChainMonitor<
	InMemorySigner,
	Arc<dyn Filter + Send + Sync>,
	Arc<BitcoindClient>,
	Arc<BitcoindClient>,
	Arc<FilesystemLogger>,
	Arc<FilesystemPersister>,
>;

pub(crate) type PeerManager = SimpleArcPeerManager<
	SocketDescriptor,
	ChainMonitor,
	BitcoindClient,
	BitcoindClient,
	dyn chain::Access + Send + Sync,
	FilesystemLogger,
>;

pub(crate) type ChannelManager =
	SimpleArcChannelManager<ChainMonitor, BitcoindClient, BitcoindClient, FilesystemLogger>;

pub(crate) type InvoicePayer<E> = payment::InvoicePayer<
	Arc<ChannelManager>,
	Router,
	Arc<Mutex<ProbabilisticScorer<Arc<NetworkGraph>, Arc<FilesystemLogger>>>>,
	Arc<FilesystemLogger>,
	E,
>;

type Router = DefaultRouter<Arc<NetworkGraph>, Arc<FilesystemLogger>>;

type GossipSync<P, G, A, L> =
	lightning_background_processor::GossipSync<P, Arc<RapidGossipSync<G, L>>, G, A, L>;

pub(crate) type NetworkGraph = gossip::NetworkGraph<Arc<FilesystemLogger>>;

struct NodeAlias<'a>(&'a [u8; 32]);

impl fmt::Display for NodeAlias<'_> {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		let alias = self
			.0
			.iter()
			.map(|b| *b as char)
			.take_while(|c| *c != '\0')
			.filter(|c| c.is_ascii_graphic() || *c == ' ')
			.collect::<String>();
		write!(f, "{}", alias)
	}
}
///////////////////////////////////////////////////////////////////////////////////////////////////////////////
/// 
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
pub(crate) fn maybe_add_change_output(tx: &mut Transaction, input_value: u64, witness_max_weight: usize, feerate_sat_per_1000_weight: u32, change_destination_script: Script) -> Result<usize, ()> {
	if input_value > MAX_VALUE_MSAT / 1000 { return Err(()); }

	const WITNESS_FLAG_BYTES: i64 = 2;

	let mut output_value = 0;
	for output in tx.output.iter() {
		output_value += output.value;
		if output_value >= input_value { return Err(()); }
	}

	let dust_value = change_destination_script.dust_value();
	let mut change_output = TxOut {
		script_pubkey: change_destination_script,
		value: 0,
	};
	let change_len = change_output.consensus_encode(&mut sink()).unwrap();
	let starting_weight = tx.weight() + WITNESS_FLAG_BYTES as usize + witness_max_weight;
	let mut weight_with_change: i64 = starting_weight as i64 + change_len as i64 * 4;
	// Include any extra bytes required to push an extra output.
	weight_with_change += (VarInt(tx.output.len() as u64 + 1).len() - VarInt(tx.output.len() as u64).len()) as i64 * 4;
	// When calculating weight, add two for the flag bytes
	let change_value: i64 = (input_value - output_value) as i64 - weight_with_change * feerate_sat_per_1000_weight as i64 / 1000;
	if change_value >= dust_value.as_sat() as i64 {
		change_output.value = change_value as u64;
		tx.output.push(change_output);
		Ok(weight_with_change as usize)
	} else if (input_value - output_value) as i64 - (starting_weight as i64) * feerate_sat_per_1000_weight as i64 / 1000 < 0 {
		Err(())
	} else {
		Ok(starting_weight)
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
pub struct MyKeysManager {
	secp_ctx: Secp256k1<secp256k1::All>,
	node_secret: SecretKey,
	inbound_payment_key: KeyMaterial,
	destination_script: Script,
	shutdown_pubkey: PublicKey,
	channel_master_key: ExtendedPrivKey,
	channel_child_index: AtomicUsize,

	rand_bytes_master_key: ExtendedPrivKey,
	rand_bytes_child_index: AtomicUsize,
	rand_bytes_unique_start: Sha256State,

	seed: [u8; 32],
	starting_time_secs: u64,
	starting_time_nanos: u32,
}

impl MyKeysManager {
	/// Constructs a KeysManager from a 32-byte seed. If the seed is in some way biased (eg your
	/// CSRNG is busted) this may panic (but more importantly, you will possibly lose funds).
	/// starting_time isn't strictly required to actually be a time, but it must absolutely,
	/// without a doubt, be unique to this instance. ie if you start multiple times with the same
	/// seed, starting_time must be unique to each run. Thus, the easiest way to achieve this is to
	/// simply use the current time (with very high precision).
	///
	/// The seed MUST be backed up safely prior to use so that the keys can be re-created, however,
	/// obviously, starting_time should be unique every time you reload the library - it is only
	/// used to generate new ephemeral key data (which will be stored by the individual channel if
	/// necessary).
	///
	/// Note that the seed is required to recover certain on-chain funds independent of
	/// ChannelMonitor data, though a current copy of ChannelMonitor data is also required for any
	/// channel, and some on-chain during-closing funds.
	///
	/// Note that until the 0.1 release there is no guarantee of backward compatibility between
	/// versions. Once the library is more fully supported, the docs will be updated to include a
	/// detailed description of the guarantee.
	pub fn new(seed: &[u8; 32], starting_time_secs: u64, starting_time_nanos: u32) -> Self {
		let secp_ctx = Secp256k1::new();
		// Note that when we aren't serializing the key, network doesn't matter
		match ExtendedPrivKey::new_master(Network::Testnet, seed) {
			Ok(master_key) => {
				let node_secret = master_key.ckd_priv(&secp_ctx, ChildNumber::from_hardened_idx(0).unwrap()).expect("Your RNG is busted").private_key;
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
				let rand_bytes_master_key = master_key.ckd_priv(&secp_ctx, ChildNumber::from_hardened_idx(4).unwrap()).expect("Your RNG is busted");
				let inbound_payment_key: SecretKey = master_key.ckd_priv(&secp_ctx, ChildNumber::from_hardened_idx(5).unwrap()).expect("Your RNG is busted").private_key;
				let mut inbound_pmt_key_bytes = [0; 32];
				inbound_pmt_key_bytes.copy_from_slice(&inbound_payment_key[..]);

				let mut rand_bytes_unique_start = Sha256::engine();
				rand_bytes_unique_start.input(&be64_to_array(starting_time_secs));
				rand_bytes_unique_start.input(&be32_to_array(starting_time_nanos));
				rand_bytes_unique_start.input(seed);
				let node_secret2= SecretKey::from_str("0000000000000000000000000000000000000000000000000000000000000001").unwrap();
				let mut res = MyKeysManager {
					secp_ctx,
					node_secret: node_secret2,
					inbound_payment_key: KeyMaterial(inbound_pmt_key_bytes),

					destination_script,
					shutdown_pubkey,

					channel_master_key,
					channel_child_index: AtomicUsize::new(0),

					rand_bytes_master_key,
					rand_bytes_child_index: AtomicUsize::new(0),
					rand_bytes_unique_start,

					seed: *seed,
					starting_time_secs,
					starting_time_nanos,
				};
				let secp_seed = res.get_secure_random_bytes();
				res.secp_ctx.seeded_randomize(&secp_seed);
				res
			},
			Err(_) => panic!("Your rng is busted"),
		}
	}
	/// Derive an old Sign containing per-channel secrets based on a key derivation parameters.
	///
	/// Key derivation parameters are accessible through a per-channel secrets
	/// Sign::channel_keys_id and is provided inside DynamicOuputP2WSH in case of
	/// onchain output detection for which a corresponding delayed_payment_key must be derived.
	pub fn derive_channel_keys(&self, channel_value_satoshis: u64, params: &[u8; 32]) -> InMemorySigner {
		let chan_id = slice_to_be64(&params[0..8]);
		assert!(chan_id <= core::u32::MAX as u64); // Otherwise the params field wasn't created by us
		let mut unique_start = Sha256::engine();
		unique_start.input(params);
		unique_start.input(&self.seed);

		// We only seriously intend to rely on the channel_master_key for true secure
		// entropy, everything else just ensures uniqueness. We rely on the unique_start (ie
		// starting_time provided in the constructor) to be unique.
		let child_privkey = self.channel_master_key.ckd_priv(&self.secp_ctx, ChildNumber::from_hardened_idx(chan_id as u32).expect("key space exhausted")).expect("Your RNG is busted");
		unique_start.input(&child_privkey.private_key[..]);

		let seed = Sha256::from_engine(unique_start).into_inner();

		let commitment_seed = {
			let mut sha = Sha256::engine();
			sha.input(&seed);
			sha.input(&b"commitment seed"[..]);
			Sha256::from_engine(sha).into_inner()
		};
		let funding_key= SecretKey::from_str("0000000000000000000000000000000000000000000000000000000000000010").unwrap();
		let revocation_base_key= SecretKey::from_str("0000000000000000000000000000000000000000000000000000000000000011").unwrap();
		let payment_key= SecretKey::from_str("0000000000000000000000000000000000000000000000000000000000000012").unwrap();
		let delayed_payment_base_key= SecretKey::from_str("0000000000000000000000000000000000000000000000000000000000000013").unwrap();
		let htlc_base_key= SecretKey::from_str("0000000000000000000000000000000000000000000000000000000000000014").unwrap();
		InMemorySigner::new(
			&self.secp_ctx,
			self.node_secret,
			funding_key,
			revocation_base_key,
			payment_key,
			delayed_payment_base_key,
			htlc_base_key,
			commitment_seed,
			channel_value_satoshis,
			params.clone()
		)
	}

	/// Creates a Transaction which spends the given descriptors to the given outputs, plus an
	/// output to the given change destination (if sufficient change value remains). The
	/// transaction will have a feerate, at least, of the given value.
	///
	/// Returns `Err(())` if the output value is greater than the input value minus required fee,
	/// if a descriptor was duplicated, or if an output descriptor `script_pubkey`
	/// does not match the one we can spend.
	///
	/// We do not enforce that outputs meet the dust limit or that any output scripts are standard.
	///
	/// May panic if the `SpendableOutputDescriptor`s were not generated by Channels which used
	/// this KeysManager or one of the `InMemorySigner` created by this KeysManager.
	pub fn spend_spendable_outputs<C: Signing>(&self, descriptors: &[&SpendableOutputDescriptor], outputs: Vec<TxOut>, change_destination_script: Script, feerate_sat_per_1000_weight: u32, secp_ctx: &Secp256k1<C>) -> Result<Transaction, ()> {
		let mut input = Vec::new();
		let mut input_value = 0;
		let mut witness_weight = 0;
		let mut output_set = HashSet::with_capacity(descriptors.len());
		for outp in descriptors {
			match outp {
				SpendableOutputDescriptor::StaticPaymentOutput(descriptor) => {
					input.push(TxIn {
						previous_output: descriptor.outpoint.into_bitcoin_outpoint(),
						script_sig: Script::new(),
						sequence: 0,
						witness: Witness::new(),
					});
					witness_weight += StaticPaymentOutputDescriptor::MAX_WITNESS_LENGTH;
					input_value += descriptor.output.value;
					if !output_set.insert(descriptor.outpoint) { return Err(()); }
				},
				SpendableOutputDescriptor::DelayedPaymentOutput(descriptor) => {
					input.push(TxIn {
						previous_output: descriptor.outpoint.into_bitcoin_outpoint(),
						script_sig: Script::new(),
						sequence: descriptor.to_self_delay as u32,
						witness: Witness::new(),
					});
					witness_weight += DelayedPaymentOutputDescriptor::MAX_WITNESS_LENGTH;
					input_value += descriptor.output.value;
					if !output_set.insert(descriptor.outpoint) { return Err(()); }
				},
				SpendableOutputDescriptor::StaticOutput { ref outpoint, ref output } => {
					input.push(TxIn {
						previous_output: outpoint.into_bitcoin_outpoint(),
						script_sig: Script::new(),
						sequence: 0,
						witness: Witness::new(),
					});
					witness_weight += 1 + 73 + 34;
					input_value += output.value;
					if !output_set.insert(*outpoint) { return Err(()); }
				}
			}
			if input_value > MAX_VALUE_MSAT / 1000 { return Err(()); }
		}
		let mut spend_tx = Transaction {
			version: 2,
			lock_time: 0,
			input,
			output: outputs,
		};
		let expected_max_weight =
			maybe_add_change_output(&mut spend_tx, input_value, witness_weight, feerate_sat_per_1000_weight, change_destination_script)?;

		let mut keys_cache: Option<(InMemorySigner, [u8; 32])> = None;
		let mut input_idx = 0;
		for outp in descriptors {
			match outp {
				SpendableOutputDescriptor::StaticPaymentOutput(descriptor) => {
					if keys_cache.is_none() || keys_cache.as_ref().unwrap().1 != descriptor.channel_keys_id {
						keys_cache = Some((
							self.derive_channel_keys(descriptor.channel_value_satoshis, &descriptor.channel_keys_id),
							descriptor.channel_keys_id));
					}
					spend_tx.input[input_idx].witness = Witness::from_vec(keys_cache.as_ref().unwrap().0.sign_counterparty_payment_input(&spend_tx, input_idx, &descriptor, &secp_ctx)?);
				},
				SpendableOutputDescriptor::DelayedPaymentOutput(descriptor) => {
					if keys_cache.is_none() || keys_cache.as_ref().unwrap().1 != descriptor.channel_keys_id {
						keys_cache = Some((
							self.derive_channel_keys(descriptor.channel_value_satoshis, &descriptor.channel_keys_id),
							descriptor.channel_keys_id));
					}
					spend_tx.input[input_idx].witness = Witness::from_vec(keys_cache.as_ref().unwrap().0.sign_dynamic_p2wsh_input(&spend_tx, input_idx, &descriptor, &secp_ctx)?);
				},
				SpendableOutputDescriptor::StaticOutput { ref output, .. } => {
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

					let sighash = hash_to_message!(&sighash::SighashCache::new(&spend_tx).segwit_signature_hash(input_idx, &witness_script, output.value, EcdsaSighashType::All).unwrap()[..]);
					let sig = sign(secp_ctx, &sighash, &secret.private_key);
					let mut sig_ser = sig.serialize_der().to_vec();
					sig_ser.push(EcdsaSighashType::All as u8);
					spend_tx.input[input_idx].witness.push(sig_ser);
					spend_tx.input[input_idx].witness.push(pubkey.inner.serialize().to_vec());
				},
			}
			input_idx += 1;
		}

		debug_assert!(expected_max_weight >= spend_tx.weight());
		// Note that witnesses with a signature vary somewhat in size, so allow
		// `expected_max_weight` to overshoot by up to 3 bytes per input.
		debug_assert!(expected_max_weight <= spend_tx.weight() + descriptors.len() * 3);

		Ok(spend_tx)
	}

}

impl KeysInterface for MyKeysManager {
	type Signer = InMemorySigner;

	fn get_node_secret(&self, recipient: Recipient) -> Result<SecretKey, ()> {
		match recipient {
			Recipient::Node => Ok(self.node_secret.clone()),
			Recipient::PhantomNode => Err(())
		}
	}

	fn get_inbound_payment_key_material(&self) -> KeyMaterial {
		self.inbound_payment_key.clone()
	}

	fn get_destination_script(&self) -> Script {
		self.destination_script.clone()
	}


	fn get_shutdown_scriptpubkey(&self) -> ShutdownScript {
		ShutdownScript::new_p2wpkh_from_pubkey(self.shutdown_pubkey.clone())
	}

	fn get_channel_signer(&self, _inbound: bool, channel_value_satoshis: u64) -> Self::Signer {
		let child_ix = self.channel_child_index.fetch_add(1, Ordering::AcqRel);
		assert!(child_ix <= core::u32::MAX as usize);
		let mut id = [0; 32];
		id[0..8].copy_from_slice(&be64_to_array(child_ix as u64));
		id[8..16].copy_from_slice(&be64_to_array(self.starting_time_nanos as u64));
		id[16..24].copy_from_slice(&be64_to_array(self.starting_time_secs));
		self.derive_channel_keys(channel_value_satoshis, &id)
	}

	fn get_secure_random_bytes(&self) -> [u8; 32] {
		let mut sha = self.rand_bytes_unique_start.clone();

		let child_ix = self.rand_bytes_child_index.fetch_add(1, Ordering::AcqRel);
		let child_privkey = self.rand_bytes_master_key.ckd_priv(&self.secp_ctx, ChildNumber::from_hardened_idx(child_ix as u32).expect("key space exhausted")).expect("Your RNG is busted");
		sha.input(&child_privkey.private_key[..]);

		sha.input(b"Unique Secure Random Bytes Salt");
		Sha256::from_engine(sha).into_inner()
	}

	fn read_chan_signer(&self, reader: &[u8]) -> Result<Self::Signer, DecodeError> {
		InMemorySigner::read(&mut io::Cursor::new(reader), self.node_secret.clone())
	}

	fn sign_invoice(&self, hrp_bytes: &[u8], invoice_data: &[u5], recipient: Recipient) -> Result<RecoverableSignature, ()> {
		let preimage = construct_invoice_preimage(&hrp_bytes, &invoice_data);
		let secret = match recipient {
			Recipient::Node => self.get_node_secret(Recipient::Node)?,
			Recipient::PhantomNode => return Err(()),
		};
		Ok(self.secp_ctx.sign_ecdsa_recoverable(&hash_to_message!(&Sha256::hash(&preimage)), &secret))
	}
}
////////////////////////////////////////////////////////////////////////////

async fn handle_ldk_events(
	channel_manager: &Arc<ChannelManager>, bitcoind_client: &BitcoindClient,
	network_graph: &NetworkGraph, keys_manager: &MyKeysManager,
	inbound_payments: &PaymentInfoStorage, outbound_payments: &PaymentInfoStorage,
	network: Network, event: &Event,
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
			outputs[0].insert(addr, *channel_value_satoshis as f64 / 100_000_000.0);
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
					counterparty_node_id,
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
		Event::PaymentReceived { payment_hash, purpose, amount_msat } => {
			println!(
				"\nEVENT: received payment from payment hash {} of {} millisatoshis",
				hex_utils::hex_str(&payment_hash.0),
				amount_msat,
			);
			print!("> ");
			io::stdout().flush().unwrap();
			let payment_preimage = match purpose {
				PaymentPurpose::InvoicePayment { payment_preimage, .. } => *payment_preimage,
				PaymentPurpose::SpontaneousPayment(preimage) => Some(*preimage),
			};
			channel_manager.claim_funds(payment_preimage.unwrap());
		}
		Event::PaymentClaimed { payment_hash, purpose, amount_msat } => {
			println!(
				"\nEVENT: claimed payment from payment hash {} of {} millisatoshis",
				hex_utils::hex_str(&payment_hash.0),
				amount_msat,
			);
			print!("> ");
			io::stdout().flush().unwrap();
			let (payment_preimage, payment_secret) = match purpose {
				PaymentPurpose::InvoicePayment { payment_preimage, payment_secret, .. } => {
					(*payment_preimage, Some(*payment_secret))
				}
				PaymentPurpose::SpontaneousPayment(preimage) => (Some(*preimage), None),
			};
			let mut payments = inbound_payments.lock().unwrap();
			match payments.entry(*payment_hash) {
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
						amt_msat: MillisatAmount(Some(*amount_msat)),
					});
				}
			}
		}
		Event::PaymentSent { payment_preimage, payment_hash, fee_paid_msat, .. } => {
			let mut payments = outbound_payments.lock().unwrap();
			for (hash, payment) in payments.iter_mut() {
				if *hash == *payment_hash {
					payment.preimage = Some(*payment_preimage);
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
		}
		Event::OpenChannelRequest { .. } => {
			// Unreachable, we don't set manually_accept_inbound_channels
		}
		Event::PaymentPathSuccessful { .. } => {}
		Event::PaymentPathFailed { .. } => {}
		Event::PaymentFailed { payment_hash, .. } => {
			print!(
				"\nEVENT: Failed to send payment to payment hash {:?}: exhausted payment retry attempts",
				hex_utils::hex_str(&payment_hash.0)
			);
			print!("> ");
			io::stdout().flush().unwrap();

			let mut payments = outbound_payments.lock().unwrap();
			if payments.contains_key(&payment_hash) {
				let payment = payments.get_mut(&payment_hash).unwrap();
				payment.status = HTLCStatus::Failed;
			}
		}
		Event::PaymentForwarded {
			prev_channel_id,
			next_channel_id,
			fee_earned_msat,
			claim_from_onchain_tx,
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
							None => " from private node".to_string(),
							Some(node) => match &node.announcement_info {
								None => " from unnamed node".to_string(),
								Some(announcement) => {
									format!(" from node {}", NodeAlias(&announcement.alias))
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
				format!("{}{}", node_str(prev_channel_id), channel_str(prev_channel_id));
			let to_next_str =
				format!("{}{}", node_str(next_channel_id), channel_str(next_channel_id));

			let from_onchain_str = if *claim_from_onchain_tx {
				"from onchain downstream claim"
			} else {
				"from HTLC fulfill message"
			};
			if let Some(fee_earned) = fee_earned_msat {
				println!(
					"\nEVENT: Forwarded payment{}{}, earning {} msat {}",
					from_prev_str, to_next_str, fee_earned, from_onchain_str
				);
			} else {
				println!(
					"\nEVENT: Forwarded payment{}{}, claiming onchain {}",
					from_prev_str, to_next_str, from_onchain_str
				);
			}
			print!("> ");
			io::stdout().flush().unwrap();
		}
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
			let destination_address = bitcoind_client.get_new_address().await;
			let output_descriptors = &outputs.iter().map(|a| a).collect::<Vec<_>>();
			let tx_feerate =
				bitcoind_client.get_est_sat_per_1000_weight(ConfirmationTarget::Normal);
			let spending_tx = keys_manager
				.spend_spendable_outputs(
					output_descriptors,
					Vec::new(),
					destination_address.script_pubkey(),
					tx_feerate,
					&Secp256k1::new(),
				)
				.unwrap();
			bitcoind_client.broadcast_transaction(&spending_tx);
		}
		Event::ChannelClosed { channel_id, reason, user_channel_id: _ } => {
			println!(
				"\nEVENT: Channel {} closed due to: {:?}",
				hex_utils::hex_str(channel_id),
				reason
			);
			print!("> ");
			io::stdout().flush().unwrap();
		}
		Event::DiscardFunding { .. } => {
			// A "real" node should probably "lock" the UTXOs spent in funding transactions until
			// the funding transaction either confirms, or this event is generated.
		}
	}
}

async fn start_ldk() {
	let args = match cli::parse_startup_args() {
		Ok(user_args) => user_args,
		Err(()) => return,
	};

	// Initialize the LDK data directory if necessary.
	let ldk_data_dir = format!("{}/.ldk", args.ldk_storage_dir_path);
	fs::create_dir_all(ldk_data_dir.clone()).unwrap();

	// Initialize our bitcoind client.
	let bitcoind_client = match BitcoindClient::new(
		args.bitcoind_rpc_host.clone(),
		args.bitcoind_rpc_port,
		args.bitcoind_rpc_username.clone(),
		args.bitcoind_rpc_password.clone(),
		tokio::runtime::Handle::current(),
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

	// ## Setup
	// Step 1: Initialize the FeeEstimator

	// BitcoindClient implements the FeeEstimator trait, so it'll act as our fee estimator.
	let fee_estimator = bitcoind_client.clone();

	// Step 2: Initialize the Logger
	let logger = Arc::new(FilesystemLogger::new(ldk_data_dir.clone()));

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
				f.write_all(&key).expect("Failed to write node keys seed to disk");
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
	let keys_manager = Arc::new(MyKeysManager::new(&keys_seed, 0, 0));
	//let te= Arc::new(KeysManager::new(&keys_seed, 0, 0));
	//let st= te.derive_channel_keys(100000,)
	// Step 7: Read ChannelMonitor state from disk
	let mut channelmonitors = persister.read_channelmonitors(keys_manager.clone()).unwrap();

	// Step 8: Initialize the ChannelManager
	let mut user_config = UserConfig::default();
	user_config.peer_channel_config_limits.force_announced_channel_preference = false;
	let mut restarting_node = true;
	let (channel_manager_blockhash, channel_manager) = {
		if let Ok(mut f) = fs::File::open(format!("{}/manager", ldk_data_dir.clone())) {
			let mut channel_monitor_mut_references = Vec::new();
			for (_, channel_monitor) in channelmonitors.iter_mut() {
				channel_monitor_mut_references.push(channel_monitor);
			}
			let read_args = ChannelManagerReadArgs::new(
				keys_manager.clone(),
				fee_estimator.clone(),
				chain_monitor.clone(),
				broadcaster.clone(),
				logger.clone(),
				user_config,
				channel_monitor_mut_references,
			);
			<(BlockHash, ChannelManager)>::read(&mut f, read_args).unwrap()
		} else {
			// We're starting a fresh node.
			restarting_node = false;
			let getinfo_resp = bitcoind_client.get_blockchain_info().await;

			let chain_params = ChainParameters {
				network: args.network,
				best_block: BestBlock::new(
					getinfo_resp.latest_blockhash,
					getinfo_resp.latest_height as u32,
				),
			};
			let fresh_channel_manager = channelmanager::ChannelManager::new(
				fee_estimator.clone(),
				chain_monitor.clone(),
				broadcaster.clone(),
				logger.clone(),
				keys_manager.clone(),
				user_config,
				chain_params,
			);
			(getinfo_resp.latest_blockhash, fresh_channel_manager)
		}
	};

	// Step 9: Sync ChannelMonitors and ChannelManager to chain tip
	let mut chain_listener_channel_monitors = Vec::new();
	let mut cache = UnboundedCache::new();
	let mut chain_tip: Option<poll::ValidatedBlockHeader> = None;
	if restarting_node {
		let mut chain_listeners =
			vec![(channel_manager_blockhash, &channel_manager as &dyn chain::Listen)];

		for (blockhash, channel_monitor) in channelmonitors.drain(..) {
			let outpoint = channel_monitor.get_funding_txo().0;
			chain_listener_channel_monitors.push((
				blockhash,
				(channel_monitor, broadcaster.clone(), fee_estimator.clone(), logger.clone()),
				outpoint,
			));
		}

		for monitor_listener_info in chain_listener_channel_monitors.iter_mut() {
			chain_listeners
				.push((monitor_listener_info.0, &monitor_listener_info.1 as &dyn chain::Listen));
		}
		chain_tip = Some(
			init::synchronize_listeners(
				&mut bitcoind_client.deref(),
				args.network,
				&mut cache,
				chain_listeners,
			)
			.await
			.unwrap(),
		);
	}

	// Step 10: Give ChannelMonitors to ChainMonitor
	for item in chain_listener_channel_monitors.drain(..) {
		let channel_monitor = item.1 .0;
		let funding_outpoint = item.2;
		chain_monitor.watch_channel(funding_outpoint, channel_monitor).unwrap();
	}

	// Step 11: Optional: Initialize the P2PGossipSync
	let genesis = genesis_block(args.network).header.block_hash();
	let network_graph_path = format!("{}/network_graph", ldk_data_dir.clone());
	let network_graph =
		Arc::new(disk::read_network(Path::new(&network_graph_path), genesis, logger.clone()));
	let gossip_sync = Arc::new(P2PGossipSync::new(
		Arc::clone(&network_graph),
		None::<Arc<dyn chain::Access + Send + Sync>>,
		logger.clone(),
	));

	// Step 12: Initialize the PeerManager
	let channel_manager: Arc<ChannelManager> = Arc::new(channel_manager);
	let mut ephemeral_bytes = [0; 32];
	rand::thread_rng().fill_bytes(&mut ephemeral_bytes);
	let lightning_msg_handler = MessageHandler {
		chan_handler: channel_manager.clone(),
		route_handler: gossip_sync.clone(),
	};
	let peer_manager: Arc<PeerManager> = Arc::new(PeerManager::new(
		lightning_msg_handler,
		keys_manager.get_node_secret(Recipient::Node).unwrap(),
		&ephemeral_bytes,
		logger.clone(),
		Arc::new(IgnoringMessageHandler {}),
	));

	// ## Running LDK
	// Step 13: Initialize networking

	let peer_manager_connection_handler = peer_manager.clone();
	let listening_port = args.ldk_peer_listening_port;
	let stop_listen_connect = Arc::new(AtomicBool::new(false));
	let stop_listen = Arc::clone(&stop_listen_connect);
	tokio::spawn(async move {
		let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", listening_port))
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

	// Step 14: Connect and Disconnect Blocks
	if chain_tip.is_none() {
		chain_tip =
			Some(init::validate_best_block_header(&mut bitcoind_client.deref()).await.unwrap());
	}
	let channel_manager_listener = channel_manager.clone();
	let chain_monitor_listener = chain_monitor.clone();
	let bitcoind_block_source = bitcoind_client.clone();
	let network = args.network;
	tokio::spawn(async move {
		let mut derefed = bitcoind_block_source.deref();
		let chain_poller = poll::ChainPoller::new(&mut derefed, network);
		let chain_listener = (chain_monitor_listener, channel_manager_listener);
		let mut spv_client =
			SpvClient::new(chain_tip.unwrap(), chain_poller, &mut cache, &chain_listener);
		loop {
			spv_client.poll_best_tip().await.unwrap();
			tokio::time::sleep(Duration::from_secs(1)).await;
		}
	});

	// Step 15: Handle LDK Events
	let channel_manager_event_listener = channel_manager.clone();
	let keys_manager_listener = keys_manager.clone();
	// TODO: persist payment info to disk
	let inbound_payments: PaymentInfoStorage = Arc::new(Mutex::new(HashMap::new()));
	let outbound_payments: PaymentInfoStorage = Arc::new(Mutex::new(HashMap::new()));
	let inbound_pmts_for_events = inbound_payments.clone();
	let outbound_pmts_for_events = outbound_payments.clone();
	let network = args.network;
	let bitcoind_rpc = bitcoind_client.clone();
	let network_graph_events = network_graph.clone();
	let handle = tokio::runtime::Handle::current();
	let event_handler = move |event: &Event| {
		handle.block_on(handle_ldk_events(
			&channel_manager_event_listener,
			&bitcoind_rpc,
			&network_graph_events,
			&keys_manager_listener,
			&inbound_pmts_for_events,
			&outbound_pmts_for_events,
			network,
			event,
		));
	};

	// Step 16: Initialize routing ProbabilisticScorer
	let scorer_path = format!("{}/scorer", ldk_data_dir.clone());
	let scorer = Arc::new(Mutex::new(disk::read_scorer(
		Path::new(&scorer_path),
		Arc::clone(&network_graph),
		Arc::clone(&logger),
	)));

	// Step 17: Create InvoicePayer
	let router = DefaultRouter::new(
		network_graph.clone(),
		logger.clone(),
		keys_manager.get_secure_random_bytes(),
	);
	let invoice_payer = Arc::new(InvoicePayer::new(
		channel_manager.clone(),
		router,
		scorer.clone(),
		logger.clone(),
		event_handler,
		payment::Retry::Timeout(Duration::from_secs(10)),
	));

	// Step 18: Persist ChannelManager and NetworkGraph
	let persister = Arc::new(FilesystemPersister::new(ldk_data_dir.clone()));

	// Step 19: Background Processing
	let background_processor = BackgroundProcessor::start(
		persister,
		invoice_payer.clone(),
		chain_monitor.clone(),
		channel_manager.clone(),
		GossipSync::P2P(gossip_sync.clone()),
		peer_manager.clone(),
		logger.clone(),
		Some(scorer.clone()),
	);

	// Regularly reconnect to channel peers.
	let connect_cm = Arc::clone(&channel_manager);
	let connect_pm = Arc::clone(&peer_manager);
	let peer_data_path = format!("{}/channel_peer_data", ldk_data_dir.clone());
	let stop_connect = Arc::clone(&stop_listen_connect);
	tokio::spawn(async move {
		let mut interval = tokio::time::interval(Duration::from_secs(1));
		loop {
			interval.tick().await;
			match disk::read_channel_peer_data(Path::new(&peer_data_path)) {
				Ok(info) => {
					let peers = connect_pm.get_peer_node_ids();
					for node_id in connect_cm
						.list_channels()
						.iter()
						.map(|chan| chan.counterparty.node_id)
						.filter(|id| !peers.contains(id))
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
	// some public channels, and is only useful if we have public listen address(es) to announce.
	// In a production environment, this should occur only after the announcement of new channels
	// to avoid churn in the global network graph.
	let chan_manager = Arc::clone(&channel_manager);
	let network = args.network;
	if !args.ldk_announced_listen_addr.is_empty() {
		tokio::spawn(async move {
			let mut interval = tokio::time::interval(Duration::from_secs(60));
			loop {
				interval.tick().await;
				chan_manager.broadcast_node_announcement(
					[0; 3],
					args.ldk_announced_node_name,
					args.ldk_announced_listen_addr.clone(),
				);
			}
		});
	}

	// Start the CLI.
	cli::poll_for_user_input(
		Arc::clone(&invoice_payer),
		Arc::clone(&peer_manager),
		Arc::clone(&channel_manager),
		Arc::clone(&keys_manager),
		Arc::clone(&network_graph),
		inbound_payments,
		outbound_payments,
		ldk_data_dir.clone(),
		network,
	)
	.await;

	// Disconnect our peers and stop accepting new connections. This ensures we don't continue
	// updating our channel data after we've stopped the background processor.
	stop_listen_connect.store(true, Ordering::Release);
	peer_manager.disconnect_all_peers();

	// Stop the background processor.
	background_processor.stop().unwrap();
}

#[tokio::main]
pub async fn main() {
	start_ldk().await;
}
