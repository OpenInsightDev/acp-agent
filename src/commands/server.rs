use std::io::Write;

use anyhow::{Context, Result};

use crate::server::{LogRecord, RegistrationRecord, ServerRecord};

pub(super) fn write_server_list<W: Write>(
    writer: &mut W,
    records: &[ServerRecord],
    json: bool,
) -> Result<()> {
    if json {
        write_json(writer, records, "server list")?;
    } else if records.is_empty() {
        writeln!(writer, "no named servers found")?;
    } else {
        writeln!(
            writer,
            "{:<20} {:<9} {:<8} {:<6} {:<10} ADDRESS",
            "NAME", "STATE", "PORT", "PID", "VERSION"
        )?;
        for record in records {
            writeln!(
                writer,
                "{:<20} {:<9} {:<8} {:<6} {:<10} {}",
                record.name,
                record.state,
                record
                    .port
                    .map_or_else(|| "-".into(), |port| port.to_string()),
                record.pid.map_or_else(|| "-".into(), |pid| pid.to_string()),
                record.version.as_deref().unwrap_or("-"),
                record.address.as_deref().unwrap_or("-"),
            )?;
        }
    }
    Ok(())
}

pub(super) fn write_status<W: Write>(
    writer: &mut W,
    record: &ServerRecord,
    json: bool,
) -> Result<()> {
    if json {
        return write_json(writer, record, "server status");
    }
    writeln!(writer, "name:    {}", record.name)?;
    writeln!(writer, "state:   {}", record.state)?;
    if let Some(host) = &record.listen_host {
        writeln!(writer, "host:    {host}")?;
    }
    if let Some(port) = record.port {
        writeln!(writer, "port:    {port}")?;
    }
    if let Some(address) = &record.address {
        writeln!(writer, "address: {address}")?;
    }
    if let Some(pid) = record.pid {
        writeln!(writer, "pid:     {pid}")?;
    }
    if let Some(version) = &record.version {
        writeln!(writer, "version: {version}")?;
    }
    match record.state.as_str() {
        "starting" => writeln!(
            writer,
            "note: the server is starting; retry this command shortly"
        )?,
        "stale" => writeln!(
            writer,
            "note: state exists but the daemon is not responding; start it again with `acp-agent server start --name {}`",
            record.name
        )?,
        "stopped" => writeln!(
            writer,
            "note: no state file found; start it with `acp-agent server start --name {}`",
            record.name
        )?,
        _ => {}
    }
    Ok(())
}

pub(super) fn write_registrations<W: Write>(
    writer: &mut W,
    name: &str,
    records: &[RegistrationRecord],
    json: bool,
) -> Result<()> {
    if json {
        write_json(writer, records, "registrations")?;
    } else if records.is_empty() {
        writeln!(writer, "server \"{name}\" has no registered agents")?;
    } else {
        writeln!(writer, "{:<24} {:<28} READINESS", "AGENT", "ROUTE")?;
        for record in records {
            let readiness = record.detail.as_ref().map_or_else(
                || record.readiness.clone(),
                |detail| format!("{}: {detail}", record.readiness),
            );
            writeln!(
                writer,
                "{:<24} {:<28} {}",
                record.id, record.route, readiness
            )?;
        }
    }
    Ok(())
}

pub(super) fn write_logs<W: Write>(writer: &mut W, record: &LogRecord, json: bool) -> Result<()> {
    if json {
        write_json(writer, record, "log tail")?;
    } else {
        for line in &record.lines {
            writeln!(writer, "{line}")?;
        }
    }
    Ok(())
}

fn write_json<W: Write, T: serde::Serialize + ?Sized>(
    writer: &mut W,
    value: &T,
    subject: &str,
) -> Result<()> {
    serde_json::to_writer_pretty(&mut *writer, value)
        .with_context(|| format!("failed to serialize {subject}"))?;
    writeln!(writer)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running_server() -> ServerRecord {
        ServerRecord {
            name: "work".into(),
            state: "running".into(),
            listen_host: Some("127.0.0.1".into()),
            port: Some(8010),
            address: Some("http://127.0.0.1:8010".into()),
            pid: Some(42),
            version: Some("0.1.0".into()),
        }
    }

    #[test]
    fn renders_server_records_as_table_and_json() {
        let record = running_server();
        let mut table = Vec::new();
        write_server_list(&mut table, std::slice::from_ref(&record), false).unwrap();
        let table = String::from_utf8(table).unwrap();
        assert!(table.starts_with("NAME"));
        assert!(table.contains("work"));
        assert!(table.contains("http://127.0.0.1:8010"));

        let mut json = Vec::new();
        write_server_list(&mut json, &[record], true).unwrap();
        let records: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(records[0]["name"], "work");
        assert_eq!(records[0]["state"], "running");
        assert_eq!(records[0]["port"], 8010);
    }

    #[test]
    fn renders_status_registration_details_and_logs() {
        let mut status = Vec::new();
        write_status(
            &mut status,
            &ServerRecord {
                name: "work".into(),
                state: "stopped".into(),
                listen_host: None,
                port: None,
                address: None,
                pid: None,
                version: None,
            },
            false,
        )
        .unwrap();
        let status = String::from_utf8(status).unwrap();
        assert!(status.contains("state:   stopped"));
        assert!(status.contains("server start --name work"));

        let mut registrations = Vec::new();
        write_registrations(
            &mut registrations,
            "work",
            &[RegistrationRecord {
                id: "demo".into(),
                route: "/demo".into(),
                readiness: "not_ready".into(),
                detail: Some("spawn failed".into()),
            }],
            false,
        )
        .unwrap();
        assert!(
            String::from_utf8(registrations)
                .unwrap()
                .contains("not_ready: spawn failed")
        );

        let mut logs = Vec::new();
        write_logs(
            &mut logs,
            &LogRecord {
                name: "work".into(),
                lines: vec!["first".into(), "second".into()],
            },
            true,
        )
        .unwrap();
        let log: serde_json::Value = serde_json::from_slice(&logs).unwrap();
        assert_eq!(log["name"], "work");
        assert_eq!(log["lines"], serde_json::json!(["first", "second"]));
    }
}
