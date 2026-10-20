//! Bounded provider event stream. Providers own cancellation and signal cleanup.
use crate::protocol::RunEvent;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub struct RunStream {
    pub events: mpsc::Receiver<RunEvent>,
    pub finished: CancellationToken,
}
