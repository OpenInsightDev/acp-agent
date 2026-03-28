use std::ffi::OsString;
use std::future::Future;
use std::time::Duration;

use acp_agent::runtime::prepare::{CommandSpec, PreparedCommand};

pub const TEST_TIMEOUT: Duration = Duration::from_secs(2);

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
