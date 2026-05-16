mod common;

use std::sync::Arc;

use common::with_runtime;
use tranquil_store::blockstore::{BlockStoreConfig, GroupCommitConfig, TranquilBlockStore};
use tranquil_store::gauntlet::{
    Gauntlet, IndexBackedByDisk, Invariant, InvariantCtx, InvariantSet, Oracle, Scenario, Seed,
    config_for,
};

#[test]
fn index_backed_by_disk_invariant_catches_phantom_after_external_delete() {
    with_runtime(|| {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = BlockStoreConfig {
            data_dir: dir.path().join("data"),
            index_dir: dir.path().join("index"),
            max_file_size: 4096,
            group_commit: GroupCommitConfig::default(),
            shard_count: 1,
        };
        let store = Arc::new(TranquilBlockStore::open(cfg).expect("open store"));

        let cids: Vec<[u8; 36]> = (0..20u32)
            .map(|seed| {
                let le = seed.to_le_bytes();
                std::array::from_fn(|i| match i {
                    0 => 0x01,
                    1 => 0x71,
                    2 => 0x12,
                    3 => 0x20,
                    4..8 => le[i - 4],
                    _ => (seed as u8).wrapping_add(i as u8),
                })
            })
            .collect();

        cids.iter().for_each(|cid| {
            store
                .put_blocks_blocking(vec![(*cid, vec![0xAA; 96])])
                .expect("put block");
        });

        let victim_fid = store
            .compaction_liveness(0)
            .unwrap()
            .iter()
            .filter(|(_, info)| info.live_blocks > 0)
            .map(|(&fid, _)| fid)
            .next()
            .expect("expected at least one indexed file");

        let victim_path = dir.path().join("data").join(format!("{victim_fid}.tqb"));
        std::fs::remove_file(&victim_path).unwrap();

        let oracle = Oracle::new();
        let ctx = InvariantCtx {
            store: &store,
            oracle: &oracle,
            root: None,
            eventlog: None,
        };
        let runtime = tokio::runtime::Handle::current();
        let result = runtime.block_on(IndexBackedByDisk.check(&ctx));

        let violation = result.expect_err("phantom index entry must trigger violation");
        assert_eq!(violation.invariant, "IndexBackedByDisk");
        assert!(
            violation.detail.contains(&victim_fid.to_string()),
            "violation detail must reference the deleted file_id: {}",
            violation.detail
        );
    });
}

#[tokio::test]
async fn external_corruption_scenario_survives_many_seeds() {
    let failures: Vec<String> = futures::future::join_all((0..5).map(Seed).map(|seed| async move {
        let cfg = config_for(Scenario::ExternalCorruption, seed);
        let report = Gauntlet::new(cfg).expect("build gauntlet").run().await;
        (seed, report)
    }))
    .await
    .into_iter()
    .filter(|(_, r)| !r.is_clean())
    .map(|(seed, r)| {
        format!(
            "seed {}: {} violations\n  {}",
            seed.0,
            r.violations.len(),
            r.violations
                .iter()
                .map(|v| format!("{}: {}", v.invariant, v.detail))
                .collect::<Vec<_>>()
                .join("\n  ")
        )
    })
    .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n---\n"));
}

#[tokio::test]
#[ignore = "long running, validates iris-class regression over many seeds"]
async fn iris_class_regression_30_seeds() {
    let failures: Vec<String> =
        futures::future::join_all((0..30).map(Seed).map(|seed| async move {
            let mut cfg = config_for(Scenario::SmokePR, seed);
            cfg.invariants = cfg.invariants | InvariantSet::INDEX_BACKED_BY_DISK;
            let report = Gauntlet::new(cfg).expect("build gauntlet").run().await;
            (seed, report)
        }))
        .await
        .into_iter()
        .filter(|(_, r)| !r.is_clean())
        .map(|(seed, r)| {
            format!(
                "seed {}: {} violations\n  {}",
                seed.0,
                r.violations.len(),
                r.violations
                    .iter()
                    .map(|v| format!("{}: {}", v.invariant, v.detail))
                    .collect::<Vec<_>>()
                    .join("\n  ")
            )
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n---\n"));
}
