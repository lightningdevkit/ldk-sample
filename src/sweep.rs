use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::{fs, io};

use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning::sign::{EntropySource, KeysManager, SpendableOutputDescriptor};
use lightning::util::logger::Logger;
use lightning::util::persist::KVStore;
use lightning::util::ser::{Readable, WithoutLength, Writeable};

use lightning_persister::fs_store::FilesystemStore;

use bitcoin::secp256k1::Secp256k1;
use bitcoin::{LockTime, PackedLockTime};
use rand::{thread_rng, Rng};

use crate::hex_utils;
use crate::BitcoindClient;
use crate::ChannelManager;
use crate::FilesystemLogger;

/// If we have any pending claimable outputs, we should slowly sweep them to our Bitcoin Core
/// wallet. We technically don't need to do this - they're ours to spend when we want and can just
/// use them to build new transactions instead, but we cannot feed them direclty into Bitcoin
/// Core's wallet so we have to sweep.
///
/// Note that this is unececssary for [`SpendableOutputDescriptor::StaticOutput`]s, which *do* have
/// an associated secret key we could simply import into Bitcoin Core's wallet, but for consistency
/// we don't do that here either.
pub(crate) async fn periodic_sweep(
	ldk_data_dir: String, keys_manager: Arc<KeysManager>, logger: Arc<FilesystemLogger>,
	persister: Arc<FilesystemStore>, bitcoind_client: Arc<BitcoindClient>,
	channel_manager: Arc<ChannelManager>,
) {
	// Regularly claim outputs which are exclusively spendable by us and send them to Bitcoin Core.
	// Note that if you more tightly integrate your wallet with LDK you may not need to do this -
	// these outputs can just be treated as normal outputs during coin selection.
	let pending_spendables_dir =
		format!("{}/{}", ldk_data_dir, crate::PENDING_SPENDABLE_OUTPUT_DIR);
	let processing_spendables_dir = format!("{}/processing_spendable_outputs", ldk_data_dir);
	let spendables_dir = format!("{}/spendable_outputs", ldk_data_dir);

	// We batch together claims of all spendable outputs generated each day, however only after
	// batching any claims of spendable outputs which were generated prior to restart. On a mobile
	// device we likely won't ever be online for more than a minute, so we have to ensure we sweep
	// any pending claims on startup, but for an always-online node you may wish to sweep even less
	// frequently than this (or move the interval await to the top of the loop)!
	//
	// There is no particular rush here, we just have to ensure funds are availably by the time we
	// need to send funds.
	let mut interval = tokio::time::interval(Duration::from_secs(60 * 60 * 24));

	loop {
		interval.tick().await; // Note that the first tick completes immediately
		if let Ok(dir_iter) = fs::read_dir(&pending_spendables_dir) {
			// Move any spendable descriptors from pending folder so that we don't have any
			// races with new files being added.
			for file_res in dir_iter {
				let file = file_res.unwrap();
				// Only move a file if its a 32-byte-hex'd filename, otherwise it might be a
				// temporary file.
				if file.file_name().len() == 64 {
					fs::create_dir_all(&processing_spendables_dir).unwrap();
					let mut holding_path = PathBuf::new();
					holding_path.push(&processing_spendables_dir);
					holding_path.push(&file.file_name());
					fs::rename(file.path(), holding_path).unwrap();
				}
			}
			// Now concatenate all the pending files we moved into one file in the
			// `spendable_outputs` directory and drop the processing directory.
			let mut outputs = Vec::new();
			if let Ok(processing_iter) = fs::read_dir(&processing_spendables_dir) {
				for file_res in processing_iter {
					outputs.append(&mut fs::read(file_res.unwrap().path()).unwrap());
				}
			}
			if !outputs.is_empty() {
				let key = hex_utils::hex_str(&keys_manager.get_secure_random_bytes());
				persister
					.write("spendable_outputs", "", &key, &WithoutLength(&outputs).encode())
					.unwrap();
				fs::remove_dir_all(&processing_spendables_dir).unwrap();
			}
		}
		// Iterate over all the sets of spendable outputs in `spendables_dir` and try to claim
		// them.
		// Note that here we try to claim each set of spendable outputs over and over again
		// forever, even long after its been claimed. While this isn't an issue per se, in practice
		// you may wish to track when the claiming transaction has confirmed and remove the
		// spendable outputs set. You may also wish to merge groups of unspent spendable outputs to
		// combine batches.
		if let Ok(dir_iter) = fs::read_dir(&spendables_dir) {
			for file_res in dir_iter {
				let mut outputs: Vec<SpendableOutputDescriptor> = Vec::new();
				let mut file = fs::File::open(file_res.unwrap().path()).unwrap();
				loop {
					// Check if there are any bytes left to read, and if so read a descriptor.
					match file.read_exact(&mut [0; 1]) {
						Ok(_) => {
							file.seek(SeekFrom::Current(-1)).unwrap();
						}
						Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
						Err(e) => Err(e).unwrap(),
					}
					outputs.push(Readable::read(&mut file).unwrap());
				}
				let destination_address = bitcoind_client.get_new_address().await;
				let output_descriptors = &outputs.iter().map(|a| a).collect::<Vec<_>>();
				let tx_feerate = bitcoind_client
					.get_est_sat_per_1000_weight(ConfirmationTarget::ChannelCloseMinimum);

				// We set nLockTime to the current height to discourage fee sniping.
				// Occasionally randomly pick a nLockTime even further back, so
				// that transactions that are delayed after signing for whatever reason,
				// e.g. high-latency mix networks and some CoinJoin implementations, have
				// better privacy.
				// Logic copied from core: https://github.com/bitcoin/bitcoin/blob/1d4846a8443be901b8a5deb0e357481af22838d0/src/wallet/spend.cpp#L936
				let mut cur_height = channel_manager.current_best_block().height();

				// 10% of the time
				if thread_rng().gen_range(0, 10) == 0 {
					// subtract random number between 0 and 100
					cur_height = cur_height.saturating_sub(thread_rng().gen_range(0, 100));
				}

				let locktime: PackedLockTime =
					LockTime::from_height(cur_height).map_or(PackedLockTime::ZERO, |l| l.into());

				if let Ok(spending_tx) = keys_manager.spend_spendable_outputs(
					output_descriptors,
					Vec::new(),
					destination_address.script_pubkey(),
					tx_feerate,
					Some(locktime),
					&Secp256k1::new(),
				) {
					// Note that, most likely, we've already sweeped this set of outputs
					// and they're already confirmed on-chain, so this broadcast will fail.
					bitcoind_client.broadcast_transactions(&[&spending_tx]);
				} else {
					lightning::log_error!(
						logger,
						"Failed to sweep spendable outputs! This may indicate the outputs are dust. Will try again in a day.");
				}
			}
		}
	}
}
