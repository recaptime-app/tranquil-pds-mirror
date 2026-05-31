mod common;

use cid::Cid;
use common::{base_url, client, create_account_and_login, store_data_dir};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::str::FromStr;
use tranquil_store::blockstore::{BLOCK_HEADER_SIZE, CID_SIZE};

#[ctor::ctor]
fn force_store_backend() {
    unsafe {
        std::env::set_var("TRANQUIL_TEST_BACKEND", "store");
    }
}

const COLLECTION: &str = "app.bsky.feed.post";

fn post_record(i: usize) -> Value {
    json!({
        "$type": COLLECTION,
        "text": format!("self-heal record {i}"),
        "createdAt": "2024-01-01T00:00:00.000Z"
    })
}

async fn apply_creates(token: &str, did: &str, start: usize, count: usize) {
    let writes: Vec<Value> = (start..start + count)
        .map(|i| {
            json!({
                "$type": "com.atproto.repo.applyWrites#create",
                "collection": COLLECTION,
                "rkey": format!("selfheal{i:05}"),
                "value": post_record(i)
            })
        })
        .collect();
    let res = client()
        .post(format!(
            "{}/xrpc/com.atproto.repo.applyWrites",
            base_url().await
        ))
        .bearer_auth(token)
        .json(&json!({ "repo": did, "validate": false, "writes": writes }))
        .send()
        .await
        .expect("applyWrites send");
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "applyWrites failed: {:?}",
        res.text().await
    );
}

async fn latest_commit_cid(did: &str) -> Cid {
    let res = client()
        .get(format!(
            "{}/xrpc/com.atproto.sync.getLatestCommit?did={did}",
            base_url().await
        ))
        .send()
        .await
        .expect("getLatestCommit send");
    assert_eq!(res.status(), StatusCode::OK, "getLatestCommit failed");
    let body: Value = res.json().await.expect("getLatestCommit json");
    Cid::from_str(body["cid"].as_str().expect("commit cid")).expect("parse commit cid")
}

fn collect_tqb(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_tqb(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("tqb") {
            out.push(path);
        }
    }
}

fn corrupt_every_block_except(data_dir: &std::path::Path, keep: &[u8]) -> usize {
    let mut corrupted = 0usize;
    let mut files = Vec::new();
    collect_tqb(data_dir, &mut files);
    for path in files {
        let mut bytes = std::fs::read(&path).expect("read tqb");
        let mut pos = BLOCK_HEADER_SIZE;
        while pos + CID_SIZE + 4 <= bytes.len() {
            let cid = &bytes[pos..pos + CID_SIZE];
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
            if cid != keep && len > 0 {
                bytes[data_start] ^= 0xFF;
                corrupted += 1;
            }
            pos = rec_end;
        }
        std::fs::write(&path, &bytes).expect("write corrupted tqb");
    }
    corrupted
}

#[tokio::test]
async fn write_self_heals_after_mst_node_corruption() {
    let client = client();
    let (token, did) = create_account_and_login(&client).await;

    apply_creates(&token, &did, 0, 150).await;
    apply_creates(&token, &did, 150, 150).await;

    let commit_cid = latest_commit_cid(&did).await;
    let commit_bytes = commit_cid.to_bytes();

    let data_dir = store_data_dir().expect("store backend data dir");
    let corrupted = corrupt_every_block_except(&data_dir, &commit_bytes);
    assert!(corrupted > 0, "expected to corrupt committed blocks");

    let res = client
        .post(format!(
            "{}/xrpc/com.atproto.repo.createRecord",
            base_url().await
        ))
        .bearer_auth(&token)
        .json(&json!({
            "repo": did,
            "collection": COLLECTION,
            "validate": false,
            "record": post_record(9999)
        }))
        .send()
        .await
        .expect("createRecord send");

    assert_eq!(
        res.status(),
        StatusCode::OK,
        "write should self-heal corrupted MST and succeed: {:?}",
        res.text().await
    );
}
