//! Network transports that expose an agent's stdio streams to remote clients.
//!
//! Each transport lives in a separate module because they target different
//! protocols (`tcp`, WebSocket, HTTP/2) while reusing the shared prepared command.
/// HTTP/2 full-duplex byte-stream transport.
pub mod h2;
/// Raw TCP byte-stream transport.
pub mod tcp;
/// WebSocket transport: one ACP message per WebSocket text frame.
pub mod ws;
