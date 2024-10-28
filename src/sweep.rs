use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fs, io};

use lightning::sign::{EntropySource, KeysManager, SpendableOutputDescriptor};
use lightning::util::logger::Logger;
use lightning::util::persist::KVStore;
use lightning::util::ser::{Readable, WithoutLength, Writeable};

use lightning_persister::fs_store::FilesystemStore;

use crate::disk::FilesystemLogger;
use crate::hex_utils;
use crate::OutputSweeper;

const DEPRECATED_PENDING_SPENDABLE_OUTPUT_DIR: &'static str = "pending_spendable_outputs";

/// We updated to use LDK's OutputSweeper as part of upgrading to LDK 0.0.123, so migrate away from
/// the old sweep persistence.
pub(crate) async fn migrate_deprecated_spendable_outputs(
	ldk_data_dir: String, keys_manager: Arc<KeysManager>, logger: Arc<FilesystemLogger>,
	persister: Arc<FilesystemStore>, sweeper: Arc<OutputSweeper>,
) {
	lightning::log_info!(&*logger, "Beginning migration of deprecated spendable outputs");
	let pending_spendables_dir =
		format!("{}/{}", ldk_data_dir, DEPRECATED_PENDING_SPENDABLE_OUTPUT_DIR);
	let processing_spendables_dir = format!("{}/processing_spendable_outputs", ldk_data_dir);
	let spendables_dir = format!("{}/spendable_outputs", ldk_data_dir);

	if !Path::new(&pending_spendables_dir).exists()
		&& !Path::new(&processing_spendables_dir).exists()
		&& !Path::new(&spendables_dir).exists()
	{
		lightning::log_info!(&*logger, "No deprecated spendable outputs to migrate, returning");
		return;
	}

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

	let best_block = sweeper.current_best_block();

	let mut outputs: Vec<SpendableOutputDescriptor> = Vec::new();
	if let Ok(dir_iter) = fs::read_dir(&spendables_dir) {
		for file_res in dir_iter {
			let file = fs::File::open(file_res.unwrap().path()).unwrap();
			let mut reader = BufReader::new(file);
			loop {
				// Check if there are any bytes left to read, and if so read a descriptor.
				match reader.read_exact(&mut [0; 1]) {
					Ok(_) => {
						reader.seek(SeekFrom::Current(-1)).unwrap();
					},
					Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
					Err(e) => Err(e).unwrap(),
				}
				outputs.push(Readable::read(&mut reader).unwrap());
			}
		}
	}

	let spend_delay = Some(best_block.height + 2);
	sweeper.track_spendable_outputs(outputs.clone(), None, false, spend_delay).unwrap();

	fs::remove_dir_all(&spendables_dir).unwrap();
	fs::remove_dir_all(&pending_spendables_dir).unwrap();

	lightning::log_info!(
		&*logger,
		"Successfully migrated {} deprecated spendable outputs",
		outputs.len()
	);
}
