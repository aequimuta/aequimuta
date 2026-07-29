use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, TcpStream};
use std::process::Command;
use std::time::Duration;

const TAILSCALE_EXECUTABLE: &str = "tailscale";
const EXECUTABLE_ERROR: &str = "error: tailscale executable is not available";
const CLIENT_ERROR: &str = "error: Tailscale daemon or client is not operational";
const CLIENT_STATE_ERROR: &str = "error: Tailscale client state is indeterminate";
const ENDPOINT_ERROR: &str = "error: current Tailscale node DNS name is unavailable";
const SERVE_STATUS_ERROR: &str = "error: failed to inspect Tailscale Serve state";
const SERVE_STATE_ERROR: &str = "error: Tailscale Serve state is indeterminate";
const SERVE_CONFLICT_ERROR: &str =
    "error: selected Tailscale Serve TCP port conflicts with existing state";
const POST_CONDITION_ERROR: &str = "error: Tailscale Serve TCP post-condition was not satisfied";

pub(crate) enum EnsureOutcome {
    Created { endpoint: String },
    AlreadySatisfied { endpoint: String },
}

pub(crate) enum ProviderState {
    Absent,
    AlreadySatisfied,
    Conflict,
    Indeterminate,
}

#[derive(Deserialize)]
struct TailscaleStatus {
    #[serde(rename = "BackendState")]
    backend_state: String,
    #[serde(rename = "Self")]
    local_node: Option<TailscaleNode>,
    #[serde(rename = "CurrentTailnet")]
    current_tailnet: Option<TailnetStatus>,
}

#[derive(Deserialize)]
struct TailscaleNode {
    #[serde(rename = "DNSName")]
    dns_name: String,
    #[serde(rename = "Online")]
    online: bool,
}

#[derive(Deserialize)]
struct TailnetStatus {
    #[serde(rename = "MagicDNSEnabled")]
    magic_dns_enabled: bool,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServeConfig {
    #[serde(rename = "TCP", default)]
    tcp: HashMap<String, TcpPortHandler>,
    #[serde(rename = "Web", default)]
    web: HashMap<String, Value>,
    #[serde(rename = "Services", default)]
    _services: HashMap<String, Value>,
    #[serde(rename = "AllowFunnel", default)]
    allow_funnel: HashMap<String, bool>,
    #[serde(rename = "Foreground", default)]
    foreground: HashMap<String, ServeConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TcpPortHandler {
    #[serde(rename = "HTTPS", default)]
    https: bool,
    #[serde(rename = "HTTP", default)]
    http: bool,
    #[serde(rename = "TCPForward", default)]
    tcp_forward: String,
    #[serde(rename = "TerminateTLS", default)]
    terminate_tls: String,
    #[serde(rename = "ProxyProtocol", default)]
    proxy_protocol: i64,
}

pub(crate) fn ensure(port: u16) -> Result<EnsureOutcome, String> {
    let endpoint = current_node_endpoint(port)?;
    ensure_local_backend_is_reachable(port)?;

    match inspect_provider_state(port)? {
        ProviderState::AlreadySatisfied => Ok(EnsureOutcome::AlreadySatisfied { endpoint }),
        ProviderState::Conflict => Err(SERVE_CONFLICT_ERROR.to_owned()),
        ProviderState::Indeterminate => Err(SERVE_STATE_ERROR.to_owned()),
        ProviderState::Absent => create_mapping(port, endpoint),
    }
}

pub(crate) fn observe(port: u16) -> Result<ProviderState, String> {
    let stdout = serve_status_stdout()?;

    parse_provider_state(&stdout, port).map_err(|()| SERVE_STATUS_ERROR.to_owned())
}

fn current_node_endpoint(port: u16) -> Result<String, String> {
    let output = match Command::new(TAILSCALE_EXECUTABLE)
        .args(["status", "--json"])
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(EXECUTABLE_ERROR.to_owned());
        }
        Err(_) => return Err(CLIENT_ERROR.to_owned()),
    };

    if !output.status.success() {
        return Err(CLIENT_ERROR.to_owned());
    }

    let status: TailscaleStatus =
        serde_json::from_slice(&output.stdout).map_err(|_| CLIENT_STATE_ERROR.to_owned())?;
    let local_node = status
        .local_node
        .ok_or_else(|| CLIENT_STATE_ERROR.to_owned())?;
    let current_tailnet = status
        .current_tailnet
        .ok_or_else(|| CLIENT_STATE_ERROR.to_owned())?;

    if status.backend_state != "Running" || !local_node.online {
        return Err(CLIENT_ERROR.to_owned());
    }

    if !current_tailnet.magic_dns_enabled {
        return Err(ENDPOINT_ERROR.to_owned());
    }

    let dns_name = local_node
        .dns_name
        .strip_suffix('.')
        .filter(|dns_name| dns_name_is_valid(dns_name))
        .ok_or_else(|| ENDPOINT_ERROR.to_owned())?;

    Ok(format!("tcp://{dns_name}:{port}"))
}

fn dns_name_is_valid(dns_name: &str) -> bool {
    !dns_name.is_empty()
        && dns_name.len() <= 253
        && dns_name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && matches!(
                    label.as_bytes().first(),
                    Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9')
                )
                && matches!(
                    label.as_bytes().last(),
                    Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9')
                )
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn ensure_local_backend_is_reachable(port: u16) -> Result<(), String> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));

    TcpStream::connect_timeout(&address, Duration::from_secs(1))
        .map(|_| ())
        .map_err(|_| format!("error: local TCP backend 127.0.0.1:{port} is not reachable"))
}

fn inspect_provider_state(port: u16) -> Result<ProviderState, String> {
    let stdout = serve_status_stdout()?;

    Ok(parse_provider_state(&stdout, port).unwrap_or(ProviderState::Indeterminate))
}

fn serve_status_stdout() -> Result<Vec<u8>, String> {
    let output = Command::new(TAILSCALE_EXECUTABLE)
        .args(["serve", "status", "--json"])
        .output()
        .map_err(|_| SERVE_STATUS_ERROR.to_owned())?;

    if !output.status.success() {
        return Err(SERVE_STATUS_ERROR.to_owned());
    }

    Ok(output.stdout)
}

fn parse_provider_state(stdout: &[u8], port: u16) -> Result<ProviderState, ()> {
    serde_json::from_slice::<Value>(stdout).map_err(|_| ())?;

    let config: Option<ServeConfig> = match serde_json::from_slice(stdout) {
        Ok(config) => config,
        Err(_) => return Ok(ProviderState::Indeterminate),
    };

    let config = config.unwrap_or_default();
    Ok(classify_provider_state(&config, port))
}

fn classify_provider_state(config: &ServeConfig, port: u16) -> ProviderState {
    let tcp_handler = match tcp_handler_at_port(config, port) {
        Ok(tcp_handler) => tcp_handler,
        Err(()) => return ProviderState::Indeterminate,
    };
    let has_web = match host_port_map_contains_port(&config.web, port) {
        Ok(has_web) => has_web,
        Err(()) => return ProviderState::Indeterminate,
    };
    let has_funnel = match funnel_map_contains_port(&config.allow_funnel, port) {
        Ok(has_funnel) => has_funnel,
        Err(()) => return ProviderState::Indeterminate,
    };
    let has_foreground = match foreground_contains_port(&config.foreground, port) {
        Ok(has_foreground) => has_foreground,
        Err(()) => return ProviderState::Indeterminate,
    };

    if has_web || has_funnel || has_foreground {
        return ProviderState::Conflict;
    }

    let Some(tcp_handler) = tcp_handler else {
        return ProviderState::Absent;
    };
    let desired_target = format!("127.0.0.1:{port}");

    if !tcp_handler.http
        && !tcp_handler.https
        && tcp_handler.tcp_forward == desired_target
        && tcp_handler.terminate_tls.is_empty()
        && tcp_handler.proxy_protocol == 0
    {
        ProviderState::AlreadySatisfied
    } else {
        ProviderState::Conflict
    }
}

fn tcp_handler_at_port(
    config: &ServeConfig,
    selected_port: u16,
) -> Result<Option<&TcpPortHandler>, ()> {
    for port in config.tcp.keys() {
        let parsed_port = port.parse::<u16>().map_err(|_| ())?;

        if parsed_port.to_string() != *port {
            return Err(());
        }
    }

    Ok(config.tcp.get(&selected_port.to_string()))
}

fn host_port_map_contains_port<V>(
    entries: &HashMap<String, V>,
    selected_port: u16,
) -> Result<bool, ()> {
    let mut contains_port = false;

    for host_port in entries.keys() {
        if parse_host_port(host_port)? == selected_port {
            contains_port = true;
        }
    }

    Ok(contains_port)
}

fn funnel_map_contains_port(
    entries: &HashMap<String, bool>,
    selected_port: u16,
) -> Result<bool, ()> {
    let mut contains_port = false;

    for (host_port, enabled) in entries {
        if parse_host_port(host_port)? == selected_port && *enabled {
            contains_port = true;
        }
    }

    Ok(contains_port)
}

fn parse_host_port(host_port: &str) -> Result<u16, ()> {
    let (host, port) = host_port.rsplit_once(':').ok_or(())?;

    if host.is_empty() {
        return Err(());
    }

    let parsed_port = port.parse::<u16>().map_err(|_| ())?;

    if parsed_port.to_string() != port {
        return Err(());
    }

    Ok(parsed_port)
}

fn foreground_contains_port(
    foreground: &HashMap<String, ServeConfig>,
    selected_port: u16,
) -> Result<bool, ()> {
    for config in foreground.values() {
        if tcp_handler_at_port(config, selected_port)?.is_some()
            || host_port_map_contains_port(&config.web, selected_port)?
            || funnel_map_contains_port(&config.allow_funnel, selected_port)?
            || foreground_contains_port(&config.foreground, selected_port)?
        {
            return Ok(true);
        }
    }

    Ok(false)
}

fn create_mapping(port: u16, endpoint: String) -> Result<EnsureOutcome, String> {
    let tcp_flag = format!("--tcp={port}");
    let target = format!("tcp://127.0.0.1:{port}");
    let mutation = Command::new(TAILSCALE_EXECUTABLE)
        .arg("serve")
        .arg("--bg")
        .arg(&tcp_flag)
        .arg(&target)
        .output();

    match mutation {
        Ok(output) if output.status.success() => verify_created_mapping(port, endpoint),
        Ok(_) | Err(_) => Err(mutation_failure(port)),
    }
}

fn verify_created_mapping(port: u16, endpoint: String) -> Result<EnsureOutcome, String> {
    match inspect_provider_state(port) {
        Ok(ProviderState::AlreadySatisfied) => Ok(EnsureOutcome::Created { endpoint }),
        Ok(ProviderState::Absent | ProviderState::Conflict | ProviderState::Indeterminate)
        | Err(_) => Err(POST_CONDITION_ERROR.to_owned()),
    }
}

fn mutation_failure(port: u16) -> String {
    match inspect_provider_state(port) {
        Ok(ProviderState::Absent) => {
            "error: Tailscale Serve mutation failed and the target remains absent".to_owned()
        }
        Ok(ProviderState::AlreadySatisfied) => concat!(
            "error: Tailscale Serve mutation failed; the desired mapping is present ",
            "but operation completion is uncertain"
        )
        .to_owned(),
        Ok(ProviderState::Conflict) => {
            "error: Tailscale Serve mutation failed and the target is in conflict".to_owned()
        }
        Ok(ProviderState::Indeterminate) | Err(_) => concat!(
            "error: Tailscale Serve mutation failed and the resulting state ",
            "is indeterminate"
        )
        .to_owned(),
    }
}
