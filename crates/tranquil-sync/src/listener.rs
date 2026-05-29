use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use tracing::{error, info, warn};
use tranquil_db_traits::SequenceNumber;
use tranquil_pds::state::AppState;
use tranquil_pds::sync::firehose::SequencedEvent;

static LAST_BROADCAST_SEQ: AtomicI64 = AtomicI64::new(0);

const DRAIN_BATCH_SIZE: i64 = 1000;
const POLL_INTERVAL: Duration = Duration::from_secs(1);

pub async fn start_sequencer_listener(state: AppState) {
    let initial_seq = state
        .repos
        .repo
        .get_max_seq()
        .await
        .unwrap_or(SequenceNumber::ZERO);
    LAST_BROADCAST_SEQ.store(initial_seq.as_i64(), Ordering::SeqCst);
    info!(
        initial_seq = initial_seq.as_i64(),
        "Initialized sequencer listener"
    );
    tokio::spawn(async move {
        info!("Starting sequencer listener background task");
        loop {
            if let Err(e) = listen_loop(state.clone()).await {
                error!("Sequencer listener failed: {}. Restarting in 5s...", e);
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    });
}

async fn listen_loop(state: AppState) -> anyhow::Result<()> {
    let mut receiver = state
        .repos
        .event_notifier
        .subscribe()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to subscribe to events: {:?}", e))?;
    info!("Connected to database and listening for repo updates");

    let mut last_seq = LAST_BROADCAST_SEQ.load(Ordering::SeqCst);
    sequence_and_broadcast(&state, &mut last_seq).await;

    loop {
        tokio::select! {
            received = receiver.recv() => {
                if received.is_none() {
                    return Err(anyhow::anyhow!("Event receiver disconnected"));
                }
            }
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
        }
        sequence_and_broadcast(&state, &mut last_seq).await;
    }
}

async fn sequence_and_broadcast(state: &AppState, last_seq: &mut i64) {
    if let Err(e) = state.repos.repo.assign_pending_sequences().await {
        warn!("Failed to assign pending firehose sequences: {:?}", e);
    }
    loop {
        let events = match state
            .repos
            .repo
            .get_events_since_seq(SequenceNumber::from_raw(*last_seq), Some(DRAIN_BATCH_SIZE))
            .await
        {
            Ok(events) => events,
            Err(e) => {
                warn!("Sequencer broadcast query failed: {:?}", e);
                return;
            }
        };
        if events.is_empty() {
            return;
        }
        let batch_len = events.len();
        for event in events {
            let seq = event.seq.as_i64();
            let firehose_event = to_firehose_event(event);
            let _ = state.firehose_tx.send(firehose_event);
            *last_seq = seq;
            LAST_BROADCAST_SEQ.store(seq, Ordering::SeqCst);
        }
        if (batch_len as i64) < DRAIN_BATCH_SIZE {
            return;
        }
    }
}

fn to_firehose_event(event: tranquil_db_traits::SequencedEvent) -> SequencedEvent {
    SequencedEvent {
        seq: event.seq,
        did: event.did,
        created_at: event.created_at,
        event_type: event.event_type,
        commit_cid: event.commit_cid,
        prev_cid: event.prev_cid,
        prev_data_cid: event.prev_data_cid,
        ops: event.ops,
        blobs: event.blobs,
        blocks: event.blocks,
        handle: event.handle,
        active: event.active,
        status: event.status,
        rev: event.rev,
    }
}
