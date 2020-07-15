//! Service task that tries to import blocks from the network into the database.
//!
//! The role of the block import task is to verify and append blocks to the head of the chain
//! stored in the database passed through [`Config::database`].
//!
//! The block import task receives blocks from other parts of the code (most likely the network)
//! through [`ToBlockImport::Import`] messages, verifies if they are correct by executing them, and
//! if so appends them to the head of the chain. Only blocks whose parent is the current head of
//! the chain are considered, and the others discarded.

use crate::{babe, block, block_import, database, executor, header, trie::calculate_root};

use alloc::{collections::BTreeMap, sync::Arc};
use core::pin::Pin;
use futures::{
    channel::{mpsc, oneshot},
    prelude::*,
};
use parity_scale_codec::Encode as _;
use parking_lot::Mutex;

/// Message that can be sent to the block import task by the other parts of the code.
pub enum ToBlockImport {
    /// Ask the block import task what the best block number is.
    BestBlockNumber {
        /// Channel where to send back the answer.
        send_back: oneshot::Sender<u64>,
    },
    /// Verify the correctness of a block and apply it on the storage.
    Import {
        /// Header of the block to try to import.
        scale_encoded_header: Vec<u8>,
        /// Body of the block to try to import.
        body: Vec<Vec<u8>>,
        /// Channel where to send back the outcome of the execution.
        send_back: oneshot::Sender<Result<ImportSuccess, ImportError>>,
    },
}

pub struct ImportSuccess {
    /// Header of the block that was passed as parameter.
    // TODO: return owned decoded header instead
    pub scale_encoded_header: Vec<u8>,
    /// Body of the block that was passed as parameter.
    pub body: Vec<Vec<u8>>,
    /// List of keys that have appeared, disappeared, or whose value has been modified during the
    /// execution of the block.
    pub modified_keys: Vec<Vec<u8>>,
}

/// Error that can happen when importing a block.
#[derive(Debug, derive_more::Display)]
pub enum ImportError {
    /// Error while decoding header.
    InvalidHeader(header::Error),
    /// The parent of the block isn't the current best block.
    #[display(fmt = "The parent of the block isn't the current best block.")]
    ParentIsntBest {
        /// Hash of the current best block.
        current_best_hash: [u8; 32],
    },
    /// The block verification has failed. The block is invalid and should be thrown away.
    VerificationFailed(block_import::Error),
}

/// Configuration for that task.
pub struct Config {
    /// Database where to import blocks to.
    pub database: Arc<database::Database>,
    /// Configuration for BABE, retreived from the genesis block.
    pub babe_genesis_config: babe::BabeGenesisConfiguration,
    /// How to spawn other background tasks.
    pub tasks_executor: Box<dyn Fn(Pin<Box<dyn Future<Output = ()> + Send>>) + Send>,
    /// Receiver for messages that the executor task will process.
    pub to_block_import: mpsc::Receiver<ToBlockImport>,
}

/// Runs the task itself.
pub async fn run_block_import_task(mut config: Config) {
    // The `WasmBlob` object corresponding to the head of the chain. Set to `None` if the runtime
    // code is modified.
    // Used to avoid recompiling it every single time.
    let mut wasm_blob_cache: Option<executor::WasmVmPrototype> = None;

    // Cache used to calculate the storage trie root.
    // This cache has to be kept up to date with the actual state of the storage.
    // We pass this value whenever we verify a block. The verification process returns an updated
    // version of this cache, suitable to be passed to verifying a direct child.
    let mut top_trie_root_calculation_cache = Some(calculate_root::CalculationCache::empty());

    // Cache of the storage at the head of the chain.
    let mut local_storage_cache = {
        let mut cache = BTreeMap::<Vec<u8>, Vec<u8>>::new();
        let best_block = config.database.best_block_hash().unwrap();
        let storage_keys = config.database.storage_top_trie_keys(best_block).unwrap();
        for key in storage_keys {
            let value = config
                .database
                .storage_top_trie_get(best_block, &key)
                .unwrap()
                .unwrap();
            cache.insert(key.to_vec(), value.to_vec());
        }
        cache
    };

    // Because we store blocks in the database asynchronously, we must make sure that each
    // database import starts after the previous block has finished being imported.
    // This variable contains a `oneshot::Receiver` that is triggered when the block at the
    // previous iteration has finished being imported.
    let mut previous_block_database_import_finished = None;

    // Cache of the best block header and hash.
    // Since we want to be able to import a block while the database is still importing its
    // parent, we maintain this information in cache.
    let mut best_block_hash = config.database.best_block_hash().unwrap();
    // TODO: should be an owned decoded block
    let mut best_block_header = config
        .database
        .block_scale_encoded_header(&best_block_hash)
        .unwrap()
        .unwrap()
        .to_vec();

    // Main loop of the task. Processes received messages.
    while let Some(event) = config.to_block_import.next().await {
        match event {
            ToBlockImport::BestBlockNumber { send_back } => {
                let _ = send_back.send(header::decode(&best_block_header).unwrap().number);
            }

            ToBlockImport::Import {
                scale_encoded_header,
                body,
                send_back,
            } => {
                let decoded_header = match header::decode(&scale_encoded_header) {
                    Ok(h) => h,
                    Err(err) => {
                        let _ = send_back.send(Err(ImportError::InvalidHeader(err)));
                        return;
                    }
                };

                // We only accept blocks whose parent is the current best block.
                if best_block_hash != *decoded_header.parent_hash {
                    let _ = send_back.send(Err(ImportError::ParentIsntBest {
                        current_best_hash: best_block_hash,
                    }));
                    continue;
                }

                // In order to avoid parsing/compiling the runtime code every single time, we
                // maintain a cache of the `WasmBlob` of the head of the chain.
                let runtime_wasm_blob = if let Some(vm) = wasm_blob_cache.take() {
                    vm
                } else {
                    let code = local_storage_cache.get(&b":code"[..]).unwrap();
                    executor::WasmVmPrototype::new(&code).unwrap()
                };

                // Now perform the actual block verification.
                // Note that this does **not** modify `local_storage_cache`.
                let import_result = {
                    // TODO: this mutex is stupid, the `crate::block_import` module should be reworked
                    // to be coroutine-like
                    let local_storage_cache = Arc::new(Mutex::new(&mut local_storage_cache));

                    block_import::verify_block(block_import::Config {
                        runtime: runtime_wasm_blob,
                        babe_genesis_configuration: &config.babe_genesis_config,
                        block_header: decoded_header,
                        block_body: body.iter().map(|e| &e[..]),
                        parent_block_header: header::decode(&best_block_header).unwrap(),
                        parent_storage_get: {
                            let local_storage_cache = local_storage_cache.clone();
                            move |key: Vec<u8>| {
                                let ret: Option<Vec<u8>> =
                                    local_storage_cache.lock().get(&key).cloned();
                                async move { ret }
                            }
                        },
                        parent_storage_keys_prefix: {
                            let local_storage_cache = local_storage_cache.clone();
                            move |prefix: Vec<u8>| {
                                let ret = local_storage_cache
                                    .lock()
                                    .range(prefix.clone()..)
                                    .take_while(|(k, _)| k.starts_with(&prefix))
                                    .map(|(k, _)| k.to_vec())
                                    .collect();
                                async move { ret }
                            }
                        },
                        parent_storage_next_key: {
                            let local_storage_cache = local_storage_cache.clone();
                            move |key: Vec<u8>| {
                                struct CustomBound(Vec<u8>);
                                impl core::ops::RangeBounds<Vec<u8>> for CustomBound {
                                    fn start_bound(&self) -> core::ops::Bound<&Vec<u8>> {
                                        core::ops::Bound::Excluded(&self.0)
                                    }
                                    fn end_bound(&self) -> core::ops::Bound<&Vec<u8>> {
                                        core::ops::Bound::Unbounded
                                    }
                                }
                                let ret = local_storage_cache
                                    .lock()
                                    .range(CustomBound(key))
                                    .next()
                                    .map(|(k, _)| k.to_vec());
                                async move { ret }
                            }
                        },
                        top_trie_root_calculation_cache: top_trie_root_calculation_cache.take(),
                    })
                    .await
                };

                // If the block verification failed, we can just discard everything as nothing
                // has been committed yet.
                let import_result = match import_result {
                    Ok(r) => r,
                    Err(err) => {
                        assert!(top_trie_root_calculation_cache.is_none());
                        let _ = send_back.send(Err(ImportError::VerificationFailed(err)));
                        continue;
                    }
                };

                // The block is correct. The import is going to be successful. 🎉
                // TODO: ^ unless something else wrote in the DB in the meanwhile

                // We now update the local values for the next iteration.
                // Put back the same runtime `wasm_blob_cache` unless changes have been made
                // to `:code`.
                top_trie_root_calculation_cache =
                    Some(import_result.top_trie_root_calculation_cache);
                if !import_result
                    .storage_top_trie_changes
                    .contains_key(&b":code"[..])
                {
                    wasm_blob_cache = Some(import_result.parent_runtime);
                }
                for (key, value) in &import_result.storage_top_trie_changes {
                    if let Some(value) = value {
                        local_storage_cache.insert(key.clone(), value.clone());
                    } else {
                        local_storage_cache.remove(key);
                    }
                }

                let current_best_hash = best_block_hash.clone();
                best_block_hash = header::hash_from_scale_encoded_header(&scale_encoded_header);
                best_block_header = scale_encoded_header.clone();

                // Now spawn a database task dedicated entirely to writing the block.
                (config.tasks_executor)({
                    let best_block_hash = best_block_hash.clone();
                    let database = config.database.clone();
                    let storage_top_trie_changes = import_result.storage_top_trie_changes;

                    let previous_block_db_import = previous_block_database_import_finished.take();
                    let (finished_tx, finished_rx) = oneshot::channel();
                    previous_block_database_import_finished = Some(finished_rx);

                    Box::pin(async move {
                        if let Some(previous_block_db_import) = previous_block_db_import {
                            let _ = previous_block_db_import.await;
                        }

                        let db_import_result = database.insert_new_best(
                            current_best_hash,
                            &scale_encoded_header,
                            body.iter().cloned(),
                            // TODO: we can't use `into_iter()` because the `Clone` trait isn't implemented; should be fixed in hashbrown
                            storage_top_trie_changes
                                .iter()
                                .map(|(k, v)| (k.clone(), v.clone())),
                        );

                        match db_import_result {
                            Ok(()) => {}
                            Err(database::InsertNewBestError::ObsoleteCurrentHead) => {
                                // TODO: look into the implications for the parent task
                                // We have already checked above whether the parent of the block to import
                                // was indeed the best block in the database. However the import can still
                                // fail if something else has modified the database's best block while we
                                // were busy verifying the block.
                                let current_best_hash = database.best_block_hash().unwrap();
                                let _ = send_back
                                    .send(Err(ImportError::ParentIsntBest { current_best_hash }));
                                return;
                            }
                            Err(database::InsertNewBestError::Access(err)) => {
                                panic!("Database internal error: {}", err);
                            }
                        }

                        // Block has been successfully imported! 🎉
                        let _ = send_back.send(Ok(ImportSuccess {
                            scale_encoded_header,
                            body,
                            modified_keys: storage_top_trie_changes.keys().cloned().collect(),
                        }));

                        let _ = finished_tx.send(());
                    })
                });
            }
        }
    }
}
