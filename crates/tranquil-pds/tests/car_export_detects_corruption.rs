use std::sync::Arc;

use cid::Cid;
use jacquard_repo::mst::Mst;
use jacquard_repo::storage::BlockStore;
use tranquil_pds::api::error::ApiError;
use tranquil_pds::repo::AnyBlockStore;
use tranquil_pds::scheduled::generate_repo_car;
use tranquil_store::blockstore::{BlockStoreConfig, GroupCommitConfig, TranquilBlockStore};

const RECORD_COUNT: usize = 200;

fn open_store(dir: &std::path::Path) -> AnyBlockStore {
    let cfg = BlockStoreConfig {
        data_dir: dir.join("data"),
        index_dir: dir.join("index"),
        max_file_size: 64 * 1024,
        group_commit: GroupCommitConfig::default(),
        shard_count: 1,
    };
    AnyBlockStore::TranquilStore(TranquilBlockStore::open(cfg).expect("open block store"))
}

async fn build_tree(any: &AnyBlockStore) -> Cid {
    let mut mst = Mst::new(Arc::new(any.clone()));
    for i in 0..RECORD_COUNT {
        let key = format!("app.bsky.feed.post/{i:0>6}");
        let cid = any
            .put(format!("record body {i}").as_bytes())
            .await
            .expect("put record");
        mst.add_mut(&key, cid).await.expect("mst add");
    }
    mst.persist().await.expect("persist mst")
}

fn shred_data_files(data_dir: &std::path::Path) {
    let mut shredded = false;
    for entry in std::fs::read_dir(data_dir).expect("read data dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("tqb") {
            continue;
        }
        let mut bytes = std::fs::read(&path).expect("read data file");
        let mut off = 5usize;
        while off + 48 < bytes.len() {
            bytes[off..off + 48].iter_mut().for_each(|b| *b = 0xFF);
            off += 192;
            shredded = true;
        }
        std::fs::write(&path, &bytes).expect("write corrupted data file");
    }
    assert!(shredded, "no .tqb data file was corrupted");
}

#[tokio::test]
async fn car_export_error_is_classified_as_repo_corruption() {
    let dir = tempfile::tempdir().expect("tempdir");
    let any = open_store(dir.path());
    let root = build_tree(&any).await;

    generate_repo_car(&any, &root)
        .await
        .expect("pristine CAR must generate");

    shred_data_files(&dir.path().join("data"));

    let err = generate_repo_car(&any, &root)
        .await
        .expect_err("corrupt CAR export must error");
    let chain = format!("{err:#}");
    assert!(
        ApiError::detail_is_repo_corruption(&chain),
        "CAR export error must carry the corruption marker so the sync path can schedule self-heal; got: {chain}"
    );
}
