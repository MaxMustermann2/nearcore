use std::collections::HashMap;

use borsh::{BorshDeserialize, BorshSerialize};
use crossbeam::channel;
use itertools::Itertools;
use near_primitives::hash::CryptoHash;
use near_primitives::shard_layout::ShardUId;
use tracing::{debug, info};

use crate::metrics::flat_state_metrics::inlining_migration::{
    FLAT_STATE_PAUSED_DURATION, INLINED_COUNT, INLINED_TOTAL_VALUES_SIZE, PROCESSED_COUNT,
    PROCESSED_TOTAL_VALUES_SIZE, SKIPPED_COUNT,
};
use crate::{DBCol, Store, TrieDBStorage, TrieStorage};

use super::store_helper::decode_flat_state_db_key;
use super::types::INLINE_DISK_VALUE_THRESHOLD;
use super::{FlatStateValue, FlatStorageManager};

struct ReadValueRequest {
    shard_uid: ShardUId,
    value_hash: CryptoHash,
}

struct ReadValueResponse {
    value_hash: CryptoHash,
    value_bytes: Option<Vec<u8>>,
}

/// An abstraction that enables reading values from State in parallel using
/// multiple threads.
struct StateValueReader {
    pending_requests: usize,
    value_request_send: channel::Sender<ReadValueRequest>,
    value_response_recv: channel::Receiver<ReadValueResponse>,
    join_handles: Vec<std::thread::JoinHandle<()>>,
}

impl StateValueReader {
    fn new(store: Store, num_threads: usize) -> Self {
        let (value_request_send, value_request_recv) = channel::unbounded();
        let (value_response_send, value_response_recv) = channel::unbounded();
        let mut join_handles = Vec::new();
        for _ in 0..num_threads {
            join_handles.push(Self::spawn_read_value_thread(
                store.clone(),
                value_request_recv.clone(),
                value_response_send.clone(),
            ));
        }
        Self { pending_requests: 0, value_request_send, value_response_recv, join_handles }
    }

    fn submit(&mut self, shard_uid: ShardUId, value_hash: CryptoHash) {
        let req = ReadValueRequest { shard_uid, value_hash };
        self.value_request_send.send(req).expect("send should not fail here");
        self.pending_requests += 1;
    }

    fn receive_all(&mut self) -> HashMap<CryptoHash, Vec<u8>> {
        let mut ret = HashMap::new();
        while self.pending_requests > 0 {
            let resp = self.value_response_recv.recv().expect("recv should not fail here");
            if let Some(value) = resp.value_bytes {
                ret.insert(resp.value_hash, value);
            }
            self.pending_requests -= 1;
        }
        ret
    }

    fn spawn_read_value_thread(
        store: Store,
        recv: channel::Receiver<ReadValueRequest>,
        send: channel::Sender<ReadValueResponse>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            while let Ok(req) = recv.recv() {
                let trie_storage = TrieDBStorage::new(store.clone(), req.shard_uid);
                let bytes = match trie_storage.retrieve_raw_bytes(&req.value_hash) {
                    Ok(bytes) => Some(bytes.to_vec()),
                    Err(err) => {
                        log_skipped("failed to read value from State", err);
                        None
                    }
                };
                send.send(ReadValueResponse { value_hash: req.value_hash, value_bytes: bytes })
                    .expect("send should not fail here");
            }
        })
    }

    /// Note that we cannot use standard `drop` because it takes `&mut self`
    /// as an argument which prevents manual drop of `self.value_request_send`
    fn close(self) {
        std::mem::drop(self.value_request_send);
        for join_handle in self.join_handles {
            join_handle.join().expect("join should not fail here");
        }
    }
}

/// Inlines all FlatState values having length below `INLINE_DISK_VALUE_THRESHOLD`.
/// Migration is safe to be executed in parallel with block processing, which
/// is achieved by temporary preventing FlatState updates with
/// `FlatStorageManager::set_flat_state_updates_mode`.
///
/// * `read_state_threads` - number of threads for reading values from `State` in parallel.
/// * `batch_size` - number of values to be processed for inlining in one batch.
pub fn inline_flat_state_values(
    store: Store,
    flat_storage_manager: &FlatStorageManager,
    read_state_threads: usize,
    batch_size: usize,
) {
    info!(target: "store", %read_state_threads, %batch_size, "Starting FlatState value inlining migration");
    let migration_start = std::time::Instant::now();
    let mut value_reader = StateValueReader::new(store.clone(), read_state_threads);
    let mut inlined_total_count = 0;
    for (batch_index, batch) in
        store.iter(DBCol::FlatState).chunks(batch_size).into_iter().enumerate()
    {
        let (mut min_key, mut max_key) = (None, None);
        for entry in batch {
            PROCESSED_COUNT.inc();
            let (key, value) = match entry {
                Ok(v) => v,
                Err(err) => {
                    log_skipped("rocksdb iterator error", err);
                    continue;
                }
            };
            let shard_uid = match decode_flat_state_db_key(&key) {
                Ok((shard_uid, _)) => shard_uid,
                Err(err) => {
                    log_skipped("failed to decode FlatState key", err);
                    continue;
                }
            };
            let fs_value = match FlatStateValue::try_from_slice(&value) {
                Ok(fs_value) => fs_value,
                Err(err) => {
                    log_skipped("failed to deserialise FlatState value", err);
                    continue;
                }
            };
            let value_size = match &fs_value {
                FlatStateValue::Ref(value_ref) => value_ref.length as u64,
                FlatStateValue::Inlined(bytes) => bytes.len() as u64,
            };
            PROCESSED_TOTAL_VALUES_SIZE.inc_by(value_size);
            if let FlatStateValue::Ref(value_ref) = fs_value {
                if value_ref.length as usize <= INLINE_DISK_VALUE_THRESHOLD {
                    if min_key.is_none() {
                        min_key = Some(key.to_vec());
                    }
                    max_key = Some(key.to_vec());
                    INLINED_TOTAL_VALUES_SIZE.inc_by(value_size);
                    value_reader.submit(shard_uid, value_ref.hash);
                }
            }
        }
        let hash_to_value = value_reader.receive_all();
        let mut inlined_batch_count = 0;
        let mut batch_duration = std::time::Duration::ZERO;
        if !hash_to_value.is_empty() {
            // Here we need to re-read the latest FlatState values in `min_key..=max_key` range
            // while updates are disabled. This way we prevent updating the values that
            // were updated since migration start.
            let batch_inlining_start = std::time::Instant::now();
            flat_storage_manager.set_flat_state_updates_mode(false);
            let mut store_update = store.store_update();
            // rockdb API accepts the exclusive end of the range, so we append
            // `0u8` here to make sure `max_key` is included in the range
            let upper_bound_key = max_key.map(|mut v| {
                v.push(0u8);
                v
            });
            for (key, value) in store
                .iter_range(DBCol::FlatState, min_key.as_deref(), upper_bound_key.as_deref())
                .flat_map(|v| v)
            {
                if let Ok(FlatStateValue::Ref(value_ref)) = FlatStateValue::try_from_slice(&value) {
                    if let Some(value) = hash_to_value.get(&value_ref.hash) {
                        store_update.set(
                            DBCol::FlatState,
                            &key,
                            &FlatStateValue::inlined(value)
                                .try_to_vec()
                                .expect("borsh should not fail here"),
                        );
                        inlined_batch_count += 1;
                        INLINED_COUNT.inc();
                    }
                }
            }
            store_update.commit().expect("failed to commit inlined values");
            flat_storage_manager.set_flat_state_updates_mode(true);
            inlined_total_count += inlined_batch_count;
            batch_duration = batch_inlining_start.elapsed();
            FLAT_STATE_PAUSED_DURATION.observe(batch_duration.as_secs_f64());
        }
        debug!(target: "store", %batch_index, %inlined_batch_count, %inlined_total_count, ?batch_duration, "Processed flat state value inlining batch");
    }
    value_reader.close();
    let migration_elapsed = migration_start.elapsed();
    info!(target: "store", %inlined_total_count, ?migration_elapsed, "Finished FlatState value inlining migration");
}

fn log_skipped(reason: &str, err: impl std::error::Error) {
    debug!(target: "store", %reason, %err, "Skipped value during FlatState inlining");
    SKIPPED_COUNT.inc();
}

#[cfg(test)]
mod tests {
    use borsh::{BorshDeserialize, BorshSerialize};
    use near_primitives::hash::hash;
    use near_primitives::shard_layout::ShardLayout;

    use crate::flat::store_helper::encode_flat_state_db_key;
    use crate::flat::types::INLINE_DISK_VALUE_THRESHOLD;
    use crate::flat::{FlatStateValue, FlatStorageManager};
    use crate::{DBCol, NodeStorage, TrieCachingStorage};

    use super::inline_flat_state_values;

    #[test]
    fn full_migration() {
        let store = NodeStorage::test_opener().1.open().unwrap().get_hot_store();
        let shard_uid = ShardLayout::v0_single_shard().get_shard_uids()[0];
        let values =
            [vec![0], vec![1], vec![2; INLINE_DISK_VALUE_THRESHOLD + 1], vec![3], vec![4], vec![5]];
        {
            let mut store_update = store.store_update();
            for (i, value) in values.iter().enumerate() {
                let trie_key =
                    TrieCachingStorage::get_key_from_shard_uid_and_hash(shard_uid, &hash(&value));
                store_update.increment_refcount(DBCol::State, &trie_key, &value);
                let fs_key = encode_flat_state_db_key(shard_uid, &[i as u8]);
                let fs_value = FlatStateValue::value_ref(&value).try_to_vec().unwrap();
                store_update.set(DBCol::FlatState, &fs_key, &fs_value);
            }
            store_update.commit().unwrap();
        }
        inline_flat_state_values(store.clone(), &FlatStorageManager::new(store.clone()), 2, 4);
        assert_eq!(
            store
                .iter(DBCol::FlatState)
                .flat_map(|r| r.map(|(_, v)| FlatStateValue::try_from_slice(&v).unwrap()))
                .collect::<Vec<_>>(),
            vec![
                FlatStateValue::inlined(&values[0]),
                FlatStateValue::inlined(&values[1]),
                FlatStateValue::value_ref(&values[2]),
                FlatStateValue::inlined(&values[3]),
                FlatStateValue::inlined(&values[4]),
                FlatStateValue::inlined(&values[5]),
            ]
        );
    }
}
