//! A durable post-commit observer: a reconnecting cursor loop for products
//! that ingest events (memory, analytics, notifications) without touching the
//! authoritative stream.
//!
//! The observer owns its cursor. It subscribes from the cursor it last
//! acknowledged, delivers every committed event in sequence order to one
//! sink, and on any transport failure reconnects with bounded backoff from
//! the last acknowledged cursor, so falling behind, restarting, or losing the
//! connection never skips or duplicates an event. It cannot affect
//! persistence or delivery to other clients: the server pages committed rows
//! from SQLite for every subscriber independently.

use std::{future::Future, pin::Pin, time::Duration};

use futures_util::StreamExt;
use qq_protocol::{EventCursor, SessionEventEnvelope, WorkspaceId};

use crate::{ClientError, SessionClient};

/// Reconnect backoff bounds.
pub const OBSERVER_MIN_BACKOFF: Duration = Duration::from_millis(50);
pub const OBSERVER_MAX_BACKOFF: Duration = Duration::from_secs(5);

/// What the observer does after a sink returns or a stream fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObserverStep {
    /// Keep going.
    Continue,
    /// Stop the loop; `run` returns with the last acknowledged cursor.
    Stop,
}

pub type SinkFuture<'a> = Pin<Box<dyn Future<Output = ObserverStep> + Send + 'a>>;

/// Receives committed events in order. Returning `Stop` ends the observer;
/// the event that was being delivered is treated as not acknowledged.
pub trait EventSink: Send {
    fn deliver(&mut self, event: &SessionEventEnvelope) -> SinkFuture<'_>;

    /// Called before each reconnect attempt with the failure and the cursor
    /// the observer will resume from. Default: keep going.
    fn disconnected(&mut self, _error: &ClientError, _resume_from: &EventCursor) -> ObserverStep {
        ObserverStep::Continue
    }
}

/// Why `run` returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObserverExit {
    /// The sink asked to stop.
    Stopped,
    /// The server rejected the cursor: the store was replaced or the
    /// observer's cursor is from another store. The caller must resnapshot
    /// and pick a new cursor; resuming is not safe.
    CursorRejected,
    /// An event exceeded the wire bound; the stream cannot continue past it.
    EventTooLarge,
}

/// Runs the observer until the sink stops it or the cursor becomes
/// unusable. Returns the exit reason and the last acknowledged cursor.
pub async fn run<S: EventSink>(
    client: &SessionClient,
    workspace_id: WorkspaceId,
    mut cursor: EventCursor,
    sink: &mut S,
) -> (ObserverExit, EventCursor) {
    let mut backoff = OBSERVER_MIN_BACKOFF;
    loop {
        let stream = match client.events(workspace_id, cursor).await {
            Ok(stream) => stream,
            // The route validates the cursor before streaming: a 400 here is
            // the server refusing this cursor, not a transient failure.
            Err(
                ClientError::InvalidCursor
                | ClientError::ServerMessage { status: 400, .. }
                | ClientError::ServerResponse { status: 400 },
            ) => return (ObserverExit::CursorRejected, cursor),
            Err(error) => {
                if sink.disconnected(&error, &cursor) == ObserverStep::Stop {
                    return (ObserverExit::Stopped, cursor);
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(OBSERVER_MAX_BACKOFF);
                continue;
            }
        };
        let mut stream = stream;
        let mut failure = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(event) => {
                    backoff = OBSERVER_MIN_BACKOFF;
                    let next = event.cursor;
                    if sink.deliver(&event).await == ObserverStep::Stop {
                        return (ObserverExit::Stopped, cursor);
                    }
                    cursor = next;
                }
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        match failure {
            // A clean end (server shutdown or keep-alive lapse) reconnects.
            None => {}
            Some(ClientError::InvalidCursor) => return (ObserverExit::CursorRejected, cursor),
            Some(ClientError::EventTooLarge) => return (ObserverExit::EventTooLarge, cursor),
            Some(error) if sink.disconnected(&error, &cursor) == ObserverStep::Stop => {
                return (ObserverExit::Stopped, cursor);
            }
            Some(_) => {}
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(OBSERVER_MAX_BACKOFF);
    }
}
