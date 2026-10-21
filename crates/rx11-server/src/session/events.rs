//! Channel messages exchanged between the X11 listener (server side) and
//! the per-client relay task, routed through [`crate::session::SessionManager`].

use rx11_core::protocol::FlowControlAction;
use rx11_core::types::{ConnectionId, DisplayNumber};

/// X11 socket → relay task.
pub enum X11ConnToRelay {
    Connected {
        display: DisplayNumber,
        connection_id: ConnectionId,
    },
    Data {
        display: DisplayNumber,
        connection_id: ConnectionId,
        data: bytes::Bytes,
    },
    /// The leg into the application socket drained across a flow-control
    /// resume edge: the relay must re-advertise `Resume` for this channel.
    Congested {
        display: DisplayNumber,
        connection_id: ConnectionId,
        action: FlowControlAction,
    },
    Disconnected {
        display: DisplayNumber,
        connection_id: ConnectionId,
    },
}

/// Relay task → X11 socket.
pub enum X11RelayToConn {
    Data(bytes::Bytes),
    Close,
}
