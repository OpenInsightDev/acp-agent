use std::io::Write;

use anyhow::{Context, Result};

use crate::server::{RegistrationRecord, ServerRecord};

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
            "{:<20} {:<9} {:<16} {:<6} ADDRESS",
            "NAME", "STATE", "HOST", "PORT"
        )?;
        for record in records {
            writeln!(
                writer,
                "{:<20} {:<9} {:<16} {:<6} {}",
                record.name, record.state, record.host, record.port, record.address,
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
    writeln!(writer, "host:    {}", record.host)?;
    writeln!(writer, "port:    {}", record.port)?;
    writeln!(writer, "address: {}", record.address)?;
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
    use super::{write_registrations, write_server_list, write_status};
    use crate::server::{RegistrationRecord, ServerRecord};

    fn running_server() -> ServerRecord {
        ServerRecord {
            name: "work".into(),
            state: "running".into(),
            host: "127.0.0.1".into(),
            port: 8010,
            address: "http://127.0.0.1:8010".into(),
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
    fn renders_status_and_registration_details() {
        let mut status = Vec::new();
        write_status(&mut status, &running_server(), false).unwrap();
        let status = String::from_utf8(status).unwrap();
        assert!(status.contains("state:   running"));
        assert!(status.contains("host:    127.0.0.1"));
        assert!(status.contains("port:    8010"));
        assert!(status.contains("address: http://127.0.0.1:8010"));

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
    }
}
