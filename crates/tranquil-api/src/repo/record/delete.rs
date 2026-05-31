use crate::repo::record::write::{CommitInfo, prepare_repo_write};
use axum::{Json, extract::State};
use cid::Cid;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::str::FromStr;
use tranquil_pds::api::error::ApiError;
use tranquil_pds::auth::{Active, Auth, VerifyScope};
use tranquil_pds::cid_types::RecordCid;
use tranquil_pds::repo_ops::{
    FinalizeParams, RecordOp, begin_repo_write, finalize_repo_write, with_repair_retry,
};
use tranquil_pds::state::AppState;
use tranquil_pds::types::{AtIdentifier, AtUri, Did, Nsid, Rkey};
use uuid::Uuid;

#[derive(Deserialize)]
pub struct DeleteRecordInput {
    pub repo: AtIdentifier,
    pub collection: Nsid,
    pub rkey: Rkey,
    #[serde(rename = "swapRecord")]
    pub swap_record: Option<String>,
    #[serde(rename = "swapCommit")]
    pub swap_commit: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteRecordOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<CommitInfo>,
}

pub async fn delete_record(
    State(state): State<AppState>,
    auth: Auth<Active>,
    Json(input): Json<DeleteRecordInput>,
) -> Result<Json<DeleteRecordOutput>, ApiError> {
    let scope_proof = auth.verify_repo_delete(&input.collection)?;
    let repo_auth = prepare_repo_write(&state, &scope_proof, &input.repo).await?;
    let did = repo_auth.did;
    let user_id = repo_auth.user_id;
    let controller_did = repo_auth.controller_did;

    let out = with_repair_retry(&state, user_id, || {
        delete_record_inner(&state, &did, user_id, controller_did.as_ref(), &input)
    })
    .await?;
    Ok(Json(out))
}

async fn delete_record_inner(
    state: &AppState,
    did: &Did,
    user_id: Uuid,
    controller_did: Option<&Did>,
    input: &DeleteRecordInput,
) -> Result<DeleteRecordOutput, ApiError> {
    let (ctx, mst) = begin_repo_write(state, user_id, input.swap_commit.as_deref()).await?;

    let key = format!("{}/{}", input.collection, input.rkey);

    if let Some(swap_record_str) = &input.swap_record {
        let expected_cid = Cid::from_str(swap_record_str).ok();
        let actual_cid = mst
            .get(&key)
            .await
            .map_err(|e| ApiError::from_mst_error("read swap target from MST", &e))?;
        if expected_cid != actual_cid {
            return Err(ApiError::InvalidSwap(Some(
                "Record has been modified or does not exist".into(),
            )));
        }
    }

    let prev_record_cid = mst
        .get(&key)
        .await
        .map_err(|e| ApiError::from_mst_error("read prev record from MST", &e))?;
    let Some(prev_record_cid) = prev_record_cid else {
        return Ok(DeleteRecordOutput { commit: None });
    };

    let new_mst = mst
        .delete(&key)
        .await
        .map_err(|e| ApiError::from_mst_error("delete record from MST", &e))?;

    let op = RecordOp::Delete {
        collection: input.collection.clone(),
        rkey: input.rkey.clone(),
        prev: RecordCid::from(prev_record_cid),
    };

    let deleted_uri = AtUri::from_parts(did, &input.collection, &input.rkey);

    let commit_result = finalize_repo_write(
        state,
        ctx,
        new_mst,
        FinalizeParams {
            did,
            user_id,
            controller_did,
            delegation_detail: controller_did.map(|_| {
                json!({
                    "action": "delete",
                    "collection": input.collection,
                    "rkey": input.rkey
                })
            }),
            ops: vec![op],
            blob_cids: &[],
            backlinks_to_add: vec![],
            backlinks_to_remove: vec![deleted_uri],
        },
    )
    .await?;

    Ok(DeleteRecordOutput {
        commit: Some(CommitInfo {
            cid: commit_result.commit_cid.to_string(),
            rev: commit_result.rev,
        }),
    })
}
