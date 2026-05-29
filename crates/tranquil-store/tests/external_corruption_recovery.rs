mod common;

use std::fs;

use common::{block_data, test_cid, tiny_blockstore_config, with_runtime};
use tranquil_store::blockstore::{
    CompactionResult, DataFileId, TranquilBlockStore, hint_file_path,
};
use tranquil_store::{RealIO, SystemClock};

fn data_file_path(dir: &std::path::Path, file_id: DataFileId) -> std::path::PathBuf {
    dir.join(format!("{file_id}.tqb"))
}

fn populate_with_compaction_history(
    store: &TranquilBlockStore<RealIO, SystemClock>,
    live_cids: &[u32],
) {
    live_cids.iter().for_each(|&seed| {
        store
            .put_blocks_blocking(vec![(test_cid(seed), block_data(seed))])
            .unwrap();
    });

    (0..200u32).for_each(|round| {
        let churn = test_cid(50_000 + round);
        store
            .put_blocks_blocking(vec![(churn, block_data(50_000 + round))])
            .unwrap();
        store.apply_commit_blocking(vec![], vec![churn]).unwrap();

        if round % 4 == 0 {
            common::compact_lowest_liveness(store);
        }
    });
}

#[test]
fn deleting_indexed_data_file_externally_self_heals_on_compaction() {
    with_runtime(|| {
        let dir = tempfile::TempDir::new().unwrap();
        let live_cids: Vec<u32> = (0..12u32).collect();

        {
            let store = TranquilBlockStore::open(tiny_blockstore_config(dir.path())).unwrap();
            populate_with_compaction_history(&store, &live_cids);
            drop(store);
        }

        let data_dir = dir.path().join("data");
        let store = TranquilBlockStore::open(tiny_blockstore_config(dir.path())).unwrap();

        let liveness = store.compaction_liveness(0).unwrap();
        let victim_fid = liveness
            .iter()
            .filter(|(_, info)| info.live_blocks > 0)
            .map(|(&fid, _)| fid)
            .next()
            .expect("expected at least one file with live blocks");

        drop(store);

        let victim_path = data_file_path(&data_dir, victim_fid);
        assert!(
            victim_path.exists(),
            "victim data file should exist before deletion"
        );
        fs::remove_file(&victim_path).unwrap();

        let store = TranquilBlockStore::open(tiny_blockstore_config(dir.path())).unwrap();

        let liveness_before = store.compaction_liveness(0).unwrap();
        assert!(
            liveness_before.contains_key(&victim_fid),
            "index should still claim the deleted file before compaction self-heal"
        );

        let result = store.compact_file(victim_fid, 0).unwrap();
        match result {
            CompactionResult::Purged {
                file_id,
                phantom_blocks,
            } => {
                assert_eq!(file_id, victim_fid);
                assert!(
                    phantom_blocks > 0,
                    "expected to purge non-zero phantom entries"
                );
            }
            CompactionResult::Compacted(stats) => {
                panic!(
                    "expected purge for missing source file, got compaction with {stats:?} live={} dead={}",
                    stats.live_blocks, stats.dead_blocks
                );
            }
        }

        let liveness_after = store.compaction_liveness(0).unwrap();
        assert!(
            !liveness_after.contains_key(&victim_fid),
            "compaction-purge must remove all index entries pointing at the deleted file"
        );
    });
}

#[test]
fn external_hint_orphan_cleaned_by_consistency_repair() {
    with_runtime(|| {
        let dir = tempfile::TempDir::new().unwrap();

        {
            let store = TranquilBlockStore::open(tiny_blockstore_config(dir.path())).unwrap();
            populate_with_compaction_history(&store, &(0..6u32).collect::<Vec<_>>());
            drop(store);
        }

        let data_dir = dir.path().join("data");

        let store = TranquilBlockStore::open(tiny_blockstore_config(dir.path())).unwrap();
        let any_existing_fid = store
            .list_data_files()
            .unwrap()
            .into_iter()
            .next()
            .expect("expected at least one data file after populate");
        drop(store);

        let orphan_hint_fid = DataFileId::new(any_existing_fid.raw().saturating_add(10_000));
        let orphan_path = hint_file_path(&data_dir, orphan_hint_fid);
        fs::write(&orphan_path, b"\x00").unwrap();

        let store = TranquilBlockStore::open(tiny_blockstore_config(dir.path())).unwrap();

        let metastore_dir = dir.path().join("metastore");
        std::fs::create_dir_all(&metastore_dir).unwrap();
        let metastore = tranquil_store::metastore::Metastore::open(
            &metastore_dir,
            tranquil_store::metastore::MetastoreConfig::default(),
        )
        .unwrap();

        let segments_dir = dir.path().join("eventlog").join("segments");
        std::fs::create_dir_all(&segments_dir).unwrap();
        let eventlog = tranquil_store::eventlog::EventLog::open(
            tranquil_store::eventlog::EventLogConfig {
                segments_dir,
                ..tranquil_store::eventlog::EventLogConfig::default()
            },
            tranquil_store::RealIO::new(),
        )
        .unwrap();

        let report =
            tranquil_store::consistency::verify_store_consistency(&store, &metastore, &eventlog);

        assert!(
            report.orphan_hint_files.contains(&orphan_hint_fid),
            "consistency check should flag the synthetic orphan hint file"
        );

        let repair = tranquil_store::consistency::repair_known_issues(&store, &report);
        assert!(
            repair.orphan_hints_removed >= 1,
            "repair should remove the orphan hint file"
        );
        assert!(
            !orphan_path.exists(),
            "orphan hint file should be unlinked after repair"
        );
    });
}

#[test]
fn consistency_check_flags_and_repairs_missing_indexed_file() {
    with_runtime(|| {
        let dir = tempfile::TempDir::new().unwrap();
        let live_cids: Vec<u32> = (0..10u32).collect();

        {
            let store = TranquilBlockStore::open(tiny_blockstore_config(dir.path())).unwrap();
            populate_with_compaction_history(&store, &live_cids);
            drop(store);
        }

        let data_dir = dir.path().join("data");
        let store = TranquilBlockStore::open(tiny_blockstore_config(dir.path())).unwrap();
        let victim_fid = store
            .compaction_liveness(0)
            .unwrap()
            .iter()
            .filter(|(_, info)| info.live_blocks > 0)
            .map(|(&fid, _)| fid)
            .next()
            .expect("expected at least one indexed file");
        drop(store);

        fs::remove_file(data_file_path(&data_dir, victim_fid)).unwrap();
        let _ = fs::remove_file(hint_file_path(&data_dir, victim_fid));

        let store = TranquilBlockStore::open(tiny_blockstore_config(dir.path())).unwrap();
        let metastore_dir = dir.path().join("metastore");
        std::fs::create_dir_all(&metastore_dir).unwrap();
        let metastore = tranquil_store::metastore::Metastore::open(
            &metastore_dir,
            tranquil_store::metastore::MetastoreConfig::default(),
        )
        .unwrap();

        let segments_dir = dir.path().join("eventlog").join("segments");
        std::fs::create_dir_all(&segments_dir).unwrap();
        let eventlog = tranquil_store::eventlog::EventLog::open(
            tranquil_store::eventlog::EventLogConfig {
                segments_dir,
                ..tranquil_store::eventlog::EventLogConfig::default()
            },
            tranquil_store::RealIO::new(),
        )
        .unwrap();

        let report =
            tranquil_store::consistency::verify_store_consistency(&store, &metastore, &eventlog);
        assert!(
            report.missing_indexed_files.contains(&victim_fid),
            "consistency check should flag the missing indexed file"
        );

        let repair = tranquil_store::consistency::repair_known_issues(&store, &report);
        assert!(
            repair.phantom_index_entries_purged > 0,
            "repair should purge phantom entries"
        );

        let post_repair_liveness = store.compaction_liveness(0).unwrap();
        assert!(
            !post_repair_liveness.contains_key(&victim_fid),
            "no index entries should remain for the missing file after repair"
        );
    });
}
