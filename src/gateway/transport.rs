//! Transport abstraction for Gateway → UI communication.
//!
//! Decouples the Gateway from the UI implementation, allowing
//! future replacement with StdioTransport for Node.js TUI interop.

use tokio::sync::mpsc;

use super::UiCommand;

/// Transport trait: sends UiCommand from Gateway to UI components.
///
/// Implementations:
/// - `ChannelTransport`: in-process mpsc channel (default)
/// - `StdioTransport` (future): stdout JSON-RPC for external TUI frontends
pub trait Transport: Send {
    /// Send a UiCommand to the UI. Returns true on success.
    fn send(&self, cmd: &UiCommand) -> bool;

    /// Close the transport, releasing any I/O resources.
    /// Called during shutdown to signal the receiving end that no more
    /// commands will be sent.
    #[allow(dead_code)]
    fn close(&self) {}
}

/// In-process channel transport (default).
///
/// Wraps an unbounded mpsc sender. The UI owns the receiver
/// and drains commands each render cycle via `drain_commands()`.
pub struct ChannelTransport {
    tx: mpsc::UnboundedSender<UiCommand>,
}

impl ChannelTransport {
    pub fn new(tx: mpsc::UnboundedSender<UiCommand>) -> Self {
        Self { tx }
    }
}

impl Transport for ChannelTransport {
    fn send(&self, cmd: &UiCommand) -> bool {
        self.tx.send(cmd.clone()).is_ok()
    }

    fn close(&self) {
        // Dropping the sender's clone is enough — mpsc detects disconnect
        // on the receiver side (TryRecvError::Disconnected).
        // ChannelTransport has no external I/O to flush.
    }
}
