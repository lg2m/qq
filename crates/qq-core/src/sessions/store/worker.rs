use std::{path::PathBuf, thread};

use crossbeam_channel::{Receiver, Sender, bounded, select_biased};
use qq_protocol::StoreId;
use rusqlite::Connection;
use tokio::sync::oneshot;

use super::{CONTROL_QUEUE_CAPACITY, OUTPUT_QUEUE_CAPACITY, schema::open_database};
use crate::sessions::SessionRuntimeError;

pub(super) type DatabaseJob = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

pub(super) enum WorkerMessage {
    Run(DatabaseJob),
    Shutdown,
}

pub(super) struct StartedWorker {
    pub(super) control: Sender<WorkerMessage>,
    pub(super) output: Sender<WorkerMessage>,
    pub(super) worker: thread::JoinHandle<()>,
    pub(super) ready: oneshot::Receiver<Result<StoreId, SessionRuntimeError>>,
}

pub(super) fn start(path: PathBuf) -> Result<StartedWorker, SessionRuntimeError> {
    let (control, control_rx) = bounded(CONTROL_QUEUE_CAPACITY);
    let (output, output_rx) = bounded(OUTPUT_QUEUE_CAPACITY);
    let (ready_tx, ready) = oneshot::channel();
    let worker = thread::Builder::new()
        .name("qq-session-store".to_owned())
        .spawn(move || match open_database(&path) {
            Ok((mut connection, store_id)) => {
                let _ = ready_tx.send(Ok(store_id));
                database_worker(&mut connection, &control_rx, &output_rx);
            }
            Err(error) => {
                let _ = ready_tx.send(Err(error));
            }
        })
        .map_err(|_| SessionRuntimeError::Unavailable)?;
    Ok(StartedWorker {
        control,
        output,
        worker,
        ready,
    })
}

fn database_worker(
    connection: &mut Connection,
    control: &Receiver<WorkerMessage>,
    output: &Receiver<WorkerMessage>,
) {
    loop {
        select_biased! {
            recv(control) -> message => if !run_worker_message(connection, message) { return; },
            recv(output) -> message => if !run_worker_message(connection, message) { return; },
        }
    }
}

fn run_worker_message(
    connection: &mut Connection,
    message: Result<WorkerMessage, crossbeam_channel::RecvError>,
) -> bool {
    match message {
        Ok(WorkerMessage::Run(job)) => {
            job(connection);
            true
        }
        Ok(WorkerMessage::Shutdown) | Err(_) => false,
    }
}
