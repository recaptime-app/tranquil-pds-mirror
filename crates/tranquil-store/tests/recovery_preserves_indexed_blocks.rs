mod common;

use std::fs;
use std::path::Path;

use common::{block_data, test_cid, with_runtime};
use tranquil_store::blockstore::{
    BLOCK_HEADER_SIZE, BlockStoreConfig, CID_SIZE, GroupCommitConfig, TranquilBlockStore,
};

fn config(dir: &Path) -> BlockStoreConfig {
    BlockStoreConfig {
        data_dir: dir.join("data"),
        index_dir: dir.join("index"),
        max_file_size: 1 << 20,
        group_commit: GroupCommitConfig::default(),
        shard_count: 1,
    }
}

fn corrupt_nth_block_data(data_file: &Path, n: usize) {
    let mut bytes = fs::read(data_file).expect("read data file");
    let mut pos = BLOCK_HEADER_SIZE;
    let mut idx = 0usize;
    while pos + CID_SIZE + 4 <= bytes.len() {
        let len = u32::from_le_bytes(
            bytes[pos + CID_SIZE..pos + CID_SIZE + 4]
                .try_into()
                .unwrap(),
        ) as usize;
        let data_start = pos + CID_SIZE + 4;
        let rec_end = data_start + len + 4;
        if rec_end > bytes.len() {
            break;
        }
        if idx == n && len > 0 {
            bytes[data_start] ^= 0xFF;
            fs::write(data_file, &bytes).expect("write corrupted data file");
            return;
        }
        pos = rec_end;
        idx += 1;
    }
    panic!("could not locate block {n} to corrupt");
}

#[test]
fn recovery_preserves_indexed_blocks_past_a_mid_file_corruption() {
    with_runtime(|| {
        let dir = tempfile::TempDir::new().unwrap();
        let seeds: Vec<u32> = (1..=10).collect();

        {
            let store = TranquilBlockStore::open(config(dir.path())).unwrap();
            seeds.iter().for_each(|&s| {
                store
                    .put_blocks_blocking(vec![(test_cid(s), block_data(s))])
                    .unwrap();
            });
            store
                .repair_blocks(vec![(test_cid(999), block_data(999))])
                .unwrap();
            drop(store);
        }

        let data_file = dir.path().join("data").join("000001.tqb");
        corrupt_nth_block_data(&data_file, 4);

        let store = TranquilBlockStore::open(config(dir.path())).unwrap();

        [1u32, 2, 3, 4].iter().for_each(|&s| {
            assert!(
                store.get_block_sync(&test_cid(s)).unwrap().is_some(),
                "block {s} before the corruption was lost"
            );
        });
        [6u32, 7, 8, 9, 10].iter().for_each(|&s| {
            assert!(
                store.get_block_sync(&test_cid(s)).unwrap().is_some(),
                "block {s} after the corruption was lost to recovery truncation"
            );
        });
    });
}
