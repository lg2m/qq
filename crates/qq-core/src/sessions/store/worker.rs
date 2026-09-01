use std::{path::PathBuf, thread};

use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded, select};
use qq_protocol::StoreId;
use rusqlite::Connection;
use tokio::sync::{OwnedSemaphorePermit, oneshot, watch};

use super::{
    CONTROL_BURST_LIMIT, CONTROL_QUEUE_CAPACITY, OUTPUT_QUEUE_CAPACITY, schema::open_database,
};
use crate::sessions::SessionRuntimeError;

pub(super) type DatabaseJob = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

pub(super) enum WorkerMessage {
    Run {
        job: DatabaseJob,
        _output_permit: Option<OwnedSemaphorePermit>,
    },
}

pub(super) struct StartedWorker {
    pub(super) control: Sender<WorkerMessage>,
    pub(super) output: Sender<WorkerMessage>,
    pub(super) shutdown: Sender<()>,
    pub(super) closed: watch::Receiver<bool>,
    pub(super) worker: thread::JoinHandle<()>,
    pub(super) ready: oneshot::Receiver<Result<StoreId, SessionRuntimeError>>,
}

pub(super) fn start(path: PathBuf) -> Result<StartedWorker, SessionRuntimeError> {
    let (control, control_rx) = bounded(CONTROL_QUEUE_CAPACITY);
    let (output, output_rx) = bounded(OUTPUT_QUEUE_CAPACITY);
    let (shutdown, shutdown_rx) = bounded(1);
    let (ready_tx, ready) = oneshot::channel();
    let (closed_tx, closed) = watch::channel(false);
    let worker = thread::Builder::new()
        .name("qq-session-store".to_owned())
        .spawn(move || {
            match open_database(&path) {
                Ok((mut connection, store_id)) => {
                    let _ = ready_tx.send(Ok(store_id));
                    database_worker(&mut connection, &control_rx, &output_rx, &shutdown_rx);
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
            }
            closed_tx.send_replace(true);
        })
        .map_err(|_| SessionRuntimeError::Unavailable)?;
    Ok(StartedWorker {
        control,
        output,
        shutdown,
        closed,
        worker,
        ready,
    })
}

fn database_worker(
    connection: &mut Connection,
    control: &Receiver<WorkerMessage>,
    output: &Receiver<WorkerMessage>,
    shutdown: &Receiver<()>,
) {
    let mut control_burst = 0_usize;
    let mut shutdown_requested = false;
    loop {
        if !shutdown_requested {
            match shutdown.try_recv() {
                Ok(()) | Err(TryRecvError::Disconnected) => shutdown_requested = true,
                Err(TryRecvError::Empty) => {}
            }
        }
        let mut channel_disconnected = false;
        if control_burst >= CONTROL_BURST_LIMIT {
            match output.try_recv() {
                Ok(message) => {
                    run_worker_message(connection, message);
                    control_burst = 0;
                    continue;
                }
                Err(TryRecvError::Disconnected) => channel_disconnected = true,
                Err(TryRecvError::Empty) => control_burst = 0,
            }
        }
        match control.try_recv() {
            Ok(message) => {
                run_worker_message(connection, message);
                control_burst = control_burst.saturating_add(1);
                continue;
            }
            Err(TryRecvError::Disconnected) => channel_disconnected = true,
            Err(TryRecvError::Empty) => {}
        }
        match output.try_recv() {
            Ok(message) => {
                run_worker_message(connection, message);
                control_burst = 0;
                continue;
            }
            Err(TryRecvError::Disconnected) => channel_disconnected = true,
            Err(TryRecvError::Empty) => {}
        }
        // `Store::close` first rejects new admission, then signals here. Jobs
        // already accepted into either bounded queue remain authoritative and
        // are drained before the worker reports closed.
        if shutdown_requested || channel_disconnected {
            return;
        }
        select! {
            recv(shutdown) -> _ => shutdown_requested = true,
            recv(control) -> message => match message {
                Ok(message) => {
                    run_worker_message(connection, message);
                    control_burst = control_burst.saturating_add(1);
                }
                Err(_) => return,
            },
            recv(output) -> message => match message {
                Ok(message) => {
                    run_worker_message(connection, message);
                    control_burst = 0;
                }
                Err(_) => return,
            },
        }
    }
}

fn run_worker_message(connection: &mut Connection, message: WorkerMessage) {
    match message {
        WorkerMessage::Run { job, .. } => {
            job(connection);
        }
    }
}
