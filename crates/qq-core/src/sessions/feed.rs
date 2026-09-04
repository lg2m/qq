//! The published-event outbox and per-workspace broadcast.
//!
//! Every committed event is serialized exactly once, inside the store
//! transaction that persists it. That encoding is kept as a [`PublishedEvent`]
//! and handed to subscribers over a bounded `broadcast` channel after the
//! transaction commits, so a subscriber in steady state performs no store read
//! and no parse per event, and the server writes the same bytes to the wire.
//! SQLite remains authoritative: a subscriber catches up from it whenever it
//! is behind the broadcast (initial attach, or after lagging).

use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{Arc, Mutex},
};

use qq_protocol::{SessionEventEnvelope, WorkspaceId};
use tokio::sync::broadcast;

/// Live events buffered per workspace for subscribers that keep up. A
/// subscriber that falls this far behind is redirected to SQLite catch-up, so
/// the bound never drops an event; it only bounds memory.
pub(super) const FEED_CAPACITY: usize = 1024;

/// One committed event with its canonical JSON encoding.
///
/// `json` is the exact string persisted in `events.envelope_json`, so a live
/// delivery and a replayed one are byte-identical.
#[derive(Debug)]
pub struct PublishedEvent {
    pub envelope: SessionEventEnvelope,
    pub json: Arc<str>,
}

impl PublishedEvent {
    /// The envelope, moving it out when this is the last reference and
    /// cloning otherwise. A single subscriber pays no clone.
    pub fn into_envelope(this: Arc<Self>) -> SessionEventEnvelope {
        match Arc::try_unwrap(this) {
            Ok(published) => published.envelope,
            Err(shared) => shared.envelope.clone(),
        }
    }
}

thread_local! {
    /// Events appended by the store job currently running on this thread.
    /// The database worker runs one job at a time, so between `take` calls
    /// this holds exactly one transaction's appends.
    static OUTBOX: RefCell<Vec<Arc<PublishedEvent>>> = const { RefCell::new(Vec::new()) };
}

/// Records one event the current transaction appended.
pub(super) fn stage(event: Arc<PublishedEvent>) {
    OUTBOX.with(|outbox| outbox.borrow_mut().push(event));
}

/// Drains everything the job that just ran appended. Called by the worker
/// after the job returns: on success the batch is published, on failure it is
/// discarded because the transaction rolled back.
pub(super) fn take_staged() -> Vec<Arc<PublishedEvent>> {
    OUTBOX.with(|outbox| std::mem::take(&mut *outbox.borrow_mut()))
}

/// Per-workspace broadcast of committed events.
#[derive(Default)]
pub(super) struct WorkspaceFeed {
    senders: Mutex<HashMap<WorkspaceId, broadcast::Sender<Arc<PublishedEvent>>>>,
}

impl WorkspaceFeed {
    /// Publishes one committed batch in sequence order. Publishing with no
    /// live receivers is not an error: the events are already durable and a
    /// later subscriber catches up from SQLite.
    pub(super) fn publish(&self, events: Vec<Arc<PublishedEvent>>) {
        if events.is_empty() {
            return;
        }
        let Ok(mut senders) = self.senders.lock() else {
            return;
        };
        for event in events {
            let sender = senders
                .entry(event.envelope.cursor.workspace_id)
                .or_insert_with(|| broadcast::channel(FEED_CAPACITY).0);
            let _ = sender.send(event);
        }
    }

    /// A live receiver for `workspace_id`. Events published before this call
    /// are not delivered; the subscriber reads them from SQLite.
    pub(super) fn subscribe(
        &self,
        workspace_id: WorkspaceId,
    ) -> Option<broadcast::Receiver<Arc<PublishedEvent>>> {
        let mut senders = self.senders.lock().ok()?;
        Some(
            senders
                .entry(workspace_id)
                .or_insert_with(|| broadcast::channel(FEED_CAPACITY).0)
                .subscribe(),
        )
    }
}

#[cfg(test)]
mod tests {
    use qq_protocol::{EventCursor, RunActivity, RunId, SessionEvent, SessionId, StoreId};

    use super::*;

    fn published(sequence: u64) -> Arc<PublishedEvent> {
        let envelope = SessionEventEnvelope {
            cursor: EventCursor {
                store_id: StoreId::from_bytes([1; 16]),
                workspace_id: WorkspaceId::from_bytes([7; 16]),
                sequence,
            },
            session_id: SessionId::from_bytes([2; 16]),
            run_id: None,
            caused_by: None,
            occurred_at_ms: 0,
            event: SessionEvent::RunActivityChanged {
                run_id: RunId::from_bytes([3; 16]),
                activity: RunActivity::WaitingForProvider,
            },
        };
        let json = Arc::from(serde_json::to_string(&envelope).expect("encodes"));
        Arc::new(PublishedEvent { envelope, json })
    }

    #[test]
    fn staged_events_are_taken_as_one_batch_and_the_outbox_is_left_empty() {
        assert!(take_staged().is_empty());
        let event = published(1);
        stage(Arc::clone(&event));
        stage(event);
        assert_eq!(take_staged().len(), 2);
        assert!(take_staged().is_empty());
    }

    #[tokio::test]
    async fn the_feed_delivers_in_order_and_a_late_subscriber_sees_nothing_earlier() {
        let feed = WorkspaceFeed::default();
        let workspace_id = published(1).envelope.cursor.workspace_id;
        feed.publish(vec![published(1)]);
        let mut receiver = feed.subscribe(workspace_id).expect("lock");
        feed.publish(vec![published(2), published(3)]);
        assert_eq!(receiver.recv().await.unwrap().envelope.cursor.sequence, 2);
        assert_eq!(receiver.recv().await.unwrap().envelope.cursor.sequence, 3);
        assert!(
            receiver.try_recv().is_err(),
            "nothing before the subscribe is replayed"
        );
    }

    #[test]
    fn into_envelope_moves_when_unique_and_clones_when_shared() {
        assert_eq!(
            PublishedEvent::into_envelope(published(9)).cursor.sequence,
            9
        );
        let shared = published(10);
        let other = Arc::clone(&shared);
        assert_eq!(PublishedEvent::into_envelope(shared).cursor.sequence, 10);
        assert_eq!(other.envelope.cursor.sequence, 10);
    }
}
