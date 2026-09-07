use super::protocol::{ReadinessResult, ReadinessStatus, RegistrationResult};
use std::net::SocketAddr;

pub(super) fn public_address(address: SocketAddr) -> String {
    let host = match address.ip() {
        std::net::IpAddr::V4(ip) if ip.is_unspecified() => "127.0.0.1".to_string(),
        std::net::IpAddr::V6(ip) if ip.is_unspecified() => "[::1]".to_string(),
        ip => match ip {
            std::net::IpAddr::V4(ip) => ip.to_string(),
            std::net::IpAddr::V6(ip) => format!("[{ip}]"),
        },
    };
    format!("http://{host}:{}", address.port())
}

pub(super) fn readiness_result(
    readyz_endpoint: bool,
    snapshot: crate::serve::ReadinessSnapshot,
) -> ReadinessResult {
    let (status, detail) = if !readyz_endpoint {
        (ReadinessStatus::Disabled, None)
    } else if snapshot.last_attempt_failed {
        (
            ReadinessStatus::NotReady,
            snapshot.last_failure.map(|failure| failure.detail),
        )
    } else {
        (ReadinessStatus::Ready, None)
    };
    ReadinessResult {
        status,
        attempts: snapshot.attempts,
        failures: snapshot.failures,
        detail,
    }
}

pub(super) fn registration_result(
    name: &str,
    address: &SocketAddr,
    route_key: &super::routes::RouteKey,
    runtime: &crate::serve::RouteRuntime,
) -> RegistrationResult {
    RegistrationResult {
        name: name.to_string(),
        id: route_key.id.clone(),
        route: route_key.public_route.clone(),
        path: route_key.config.path.clone(),
        health_endpoint: route_key.config.health_endpoint,
        readyz_endpoint: route_key.config.readyz_endpoint,
        address: public_address(*address),
        max_processes: route_key.config.max_processes,
        readiness: readiness_result(
            route_key.config.readyz_endpoint,
            runtime.readiness_snapshot(),
        ),
    }
}
