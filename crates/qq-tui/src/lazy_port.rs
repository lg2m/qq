//! A client port that comes online after the first frame. The composition
//! root hands the TUI a future that resolves to the real port once the server
//! is reserved or discovered; until then the TUI paints `Connecting` and
//! buffers a bounded number of requests instead of blocking startup on the
//! network.

use std::{collections::VecDeque, future::Future, pin::Pin, sync::Mutex};

use crate::{ClientFailure, ClientPort, ClientRequest, ClientUpdate};

/// Requests accepted before the inner port exists. More than this while the
/// server is still coming up is a stuck startup, not a burst to absorb.
const PENDING_REQUEST_LIMIT: usize = 64;

type Connecting<P> = Pin<Box<dyn Future<Output = Result<P, ClientFailure>> + Send>>;

enum State<P> {
    Connecting(Connecting<P>),
    Ready(P),
    /// The connect failed. `reported` flips once the failure has been
    /// delivered as an update; after that the stream reads as closed.
    Failed {
        error: ClientFailure,
        reported: bool,
    },
}

/// A `ClientPort` whose inner port arrives later. `recv` drives the connect
/// future first, so the loop's first `recv` completes the connection while
/// the initial frame is already on screen.
pub struct LazyPort<P> {
    state: State<P>,
    pending: Mutex<VecDeque<ClientRequest>>,
}

impl<P: ClientPort> LazyPort<P> {
    /// Wrap `connect`; nothing runs until the loop first awaits `recv`.
    pub fn new(connect: impl Future<Output = Result<P, ClientFailure>> + Send + 'static) -> Self {
        Self {
            state: State::Connecting(Box::pin(connect)),
            pending: Mutex::new(VecDeque::new()),
        }
    }

    /// Drive the connect future to completion, then replay buffered requests
    /// into the real port. Failures are reported once and then repeated on
    /// every send so the app can surface them.
    async fn ready(&mut self) -> Result<&mut P, ClientFailure> {
        if let State::Connecting(connect) = &mut self.state {
            match connect.await {
                Ok(port) => {
                    let pending =
                        std::mem::take(&mut *self.pending.lock().expect("pending request lock"));
                    for request in pending {
                        if let Err(error) = port.try_send(request) {
                            self.state = State::Failed {
                                error: error.clone(),
                                reported: false,
                            };
                            return Err(error);
                        }
                    }
                    self.state = State::Ready(port);
                }
                Err(error) => {
                    self.state = State::Failed {
                        error,
                        reported: false,
                    };
                }
            }
        }
        match &mut self.state {
            State::Ready(port) => Ok(port),
            State::Failed { error, .. } => Err(error.clone()),
            State::Connecting(_) => unreachable!("connect future was just driven"),
        }
    }
}

impl<P: ClientPort> ClientPort for LazyPort<P> {
    fn try_send(&self, request: ClientRequest) -> Result<(), ClientFailure> {
        match &self.state {
            State::Ready(port) => port.try_send(request),
            State::Failed { error, .. } => Err(error.clone()),
            State::Connecting(_) => {
                let mut pending = self.pending.lock().expect("pending request lock");
                if pending.len() >= PENDING_REQUEST_LIMIT {
                    return Err(ClientFailure::new(
                        "still connecting; too many requests are waiting",
                    ));
                }
                pending.push_back(request);
                Ok(())
            }
        }
    }

    async fn recv(&mut self) -> Option<ClientUpdate> {
        match self.ready().await {
            Ok(port) => port.recv().await,
            Err(error) => {
                // Report the failure once through the update stream; the
                // loop then treats the closed stream as the client stopping.
                if let State::Failed { reported, .. } = &mut self.state
                    && !*reported
                {
                    *reported = true;
                    return Some(ClientUpdate::SnapshotFailed(error));
                }
                None
            }
        }
    }
}
