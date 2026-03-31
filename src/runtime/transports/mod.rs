//! Network transports that expose an agent's stdio streams to remote clients.
//!
//! Each transport lives in a separate module because they target different
//! protocols (`tcp`, `uds`, WebSocket, HTTP/2) while reusing the shared prepared
//! command.
/// HTTP/2 full-duplex byte-stream transport.
pub mod h2;
/// Shared raw byte-stream connection handling for TCP and UDS style transports.
pub mod raw;
/// Raw TCP byte-stream transport.
pub mod tcp;
/// Raw Unix domain socket byte-stream transport.
#[cfg(unix)]
pub mod uds;
/// WebSocket transport: one ACP message per WebSocket text frame.
pub mod ws;
