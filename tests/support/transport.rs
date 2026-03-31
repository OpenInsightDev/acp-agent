use std::ffi::OsString;

use acp_agent::runtime::prepare::{CommandSpec, PreparedCommand};

pub fn prepared_command(program: &str, args: &[&str]) -> PreparedCommand {
    PreparedCommand {
        spec: CommandSpec {
            program: OsString::from(program),
            args: args.iter().map(|arg| OsString::from(*arg)).collect(),
            env: Vec::new(),
            current_dir: None,
        },
        temp_dir: None,
    }
}

pub async fn timeout<F>(future: F, context: &str) -> F::Output
where
    F: std::future::Future,
{
    tokio::time::timeout(std::time::Duration::from_secs(2), future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {context}"))
}
