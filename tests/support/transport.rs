use std::ffi::OsString;
use std::future::Future;
use std::net::TcpListener as StdTcpListener;
use std::time::Duration;

use acp_agent::runtime::prepare::{CommandSpec, PreparedCommand};

pub const TEST_TIMEOUT: Duration = Duration::from_secs(2);
#[allow(dead_code)]
pub const STREAM_BUFFER_CAPACITY: usize = 16;

#[cfg(unix)]
pub fn prepared_command_with_program(program: OsString, args: Vec<OsString>) -> PreparedCommand {
    PreparedCommand {
        spec: CommandSpec {
            program,
            args,
            env: Vec::new(),
            current_dir: None,
        },
        temp_dir: None,
    }
}

pub async fn timeout<F>(future: F) -> F::Output
where
    F: Future,
{
    tokio::time::timeout(TEST_TIMEOUT, future)
        .await
        .expect("timed out waiting for transport test operation")
}

pub fn reserve_local_port() -> u16 {
    let listener = StdTcpListener::bind(("127.0.0.1", 0)).expect("failed to reserve local port");
    listener
        .local_addr()
        .expect("reserved listener missing local addr")
        .port()
}
