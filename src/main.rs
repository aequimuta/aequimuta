mod local_tcp_backend;
mod openssh_reverse_tcp;
mod tailscale_serve_tcp;

use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::process::ExitCode;

use tailscale_serve_tcp::EnsureOutcome;

const DECLARATION_PATH: &str = "aequimuta.toml";
const PUBLISHING_PATH: &str = "aequimuta.publish.toml";
const OPENSSH_REVERSE_TCP_PUBLISHER: &str = "openssh-reverse-tcp";
const TAILSCALE_SERVE_TCP_PUBLISHER: &str = "tailscale-serve-tcp";
const INITIAL_DECLARATION: &[u8] = b"# Aequimuta service declarations\n";
const DECLARATION_READ_ERROR: &str = "error: failed to read aequimuta.toml";
const DECLARATION_UTF8_ERROR: &str = "error: aequimuta.toml is not valid UTF-8";
const INVALID_DECLARATION_ERROR: &str = "error: aequimuta.toml is not a valid declaration";
const PUBLISHING_READ_ERROR: &str = "error: failed to read aequimuta.publish.toml";
const PUBLISHING_UTF8_ERROR: &str = "error: aequimuta.publish.toml is not valid UTF-8";
const INVALID_PUBLISHING_ERROR: &str =
    "error: aequimuta.publish.toml is not a valid publishing intent";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Declaration {
    #[serde(default)]
    services: Vec<Service>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Service {
    name: String,
    port: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishingIntent {
    #[serde(default)]
    publications: Vec<Publication>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Publication {
    service: String,
    publisher: String,
}

struct DoctorReport {
    blocking_failures: usize,
}

impl DoctorReport {
    fn new() -> Self {
        Self {
            blocking_failures: 0,
        }
    }

    fn pass(&self, message: impl std::fmt::Display) {
        println!("PASS  {message}");
    }

    fn fail(&mut self, message: impl std::fmt::Display) {
        println!("FAIL  {message}");
        self.blocking_failures += 1;
    }

    fn info(&self, message: impl std::fmt::Display) {
        println!("INFO  {message}");
    }

    fn finish(self) -> ExitCode {
        if self.blocking_failures == 0 {
            println!("No blocking readiness issues detected by performed checks");
            ExitCode::SUCCESS
        } else {
            println!("Found {} blocking readiness issues", self.blocking_failures);
            ExitCode::from(1)
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.as_slice() {
        [command] if command == "version" => {
            println!("Aequimuta {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        [command] if command == "init" => init(),
        [command] if command == "validate" => validate(),
        [command] if command == "validate-publishing" => validate_publishing(),
        [command] if command == "doctor" => doctor(),
        [command] if command == "apply" => apply(),
        [command, service, publisher] if command == "publish" => publish(service, publisher),
        [command, service, publisher] if command == "status" => status(service, publisher),
        _ => {
            eprintln!("Usage: aequimuta <command>");
            ExitCode::from(2)
        }
    }
}

fn init() -> ExitCode {
    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(DECLARATION_PATH)
    {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            eprintln!("error: aequimuta.toml already exists");
            return ExitCode::from(1);
        }
        Err(error) => {
            eprintln!("error: failed to create aequimuta.toml: {error}");
            return ExitCode::from(1);
        }
    };

    if let Err(error) = file.write_all(INITIAL_DECLARATION) {
        eprintln!("error: failed to write aequimuta.toml: {error}");
        return ExitCode::from(1);
    }

    println!("Initialized Aequimuta project at aequimuta.toml");
    ExitCode::SUCCESS
}

fn validate() -> ExitCode {
    if let Err(error) = load_declaration() {
        eprintln!("{error}");
        return ExitCode::from(1);
    }

    println!("aequimuta.toml is valid");
    ExitCode::SUCCESS
}

fn validate_publishing() -> ExitCode {
    let declaration = match load_declaration() {
        Ok(declaration) => declaration,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(1);
        }
    };

    if let Err(error) = load_publishing_intent(&declaration) {
        eprintln!("{error}");
        return ExitCode::from(1);
    }

    println!("aequimuta.publish.toml is valid");
    ExitCode::SUCCESS
}

fn doctor() -> ExitCode {
    let mut report = DoctorReport::new();
    println!("Project");

    let declaration = match load_declaration() {
        Ok(declaration) => {
            report.pass(DECLARATION_PATH);
            declaration
        }
        Err(error) => {
            report.fail(format!(
                "{DECLARATION_PATH}: {}",
                diagnostic_error_detail(error)
            ));
            return report.finish();
        }
    };

    let publishing_intent = match load_publishing_intent(&declaration) {
        Ok(publishing_intent) => {
            report.pass(PUBLISHING_PATH);
            publishing_intent
        }
        Err(error) => {
            report.fail(format!(
                "{PUBLISHING_PATH}: {}",
                diagnostic_error_detail(error)
            ));
            return report.finish();
        }
    };

    if publishing_intent.publications.is_empty() {
        report.info("No desired publications to check");
        return report.finish();
    }

    report.info(format!(
        "Desired publications: {}",
        publishing_intent.publications.len()
    ));

    let mut unsupported_publishers = Vec::new();
    let mut seen_unsupported_publishers = HashSet::new();

    for publication in &publishing_intent.publications {
        if !publisher_is_operationally_supported(&publication.publisher)
            && seen_unsupported_publishers.insert(publication.publisher.as_str())
        {
            unsupported_publishers.push(publication.publisher.as_str());
        }
    }

    if unsupported_publishers.is_empty() {
        report.pass("Operational publisher support");
    } else {
        report.fail(format!(
            "Operational publisher support: unsupported desired publisher tokens: {}",
            unsupported_publishers.join(", ")
        ));
    }

    let desired_tailscale: Vec<(&str, u16)> = publishing_intent
        .publications
        .iter()
        .filter(|publication| publication.publisher == TAILSCALE_SERVE_TCP_PUBLISHER)
        .filter_map(|publication| {
            declaration
                .services
                .iter()
                .find(|service| service.name == publication.service)
                .map(|service| (publication.service.as_str(), service.port))
        })
        .collect();
    let tailscale_slots_are_ambiguous = !desired_tailscale.is_empty()
        && tailscale_serve_tcp_publications_are_ambiguous(&declaration, &publishing_intent);

    if !desired_tailscale.is_empty() {
        if tailscale_slots_are_ambiguous {
            report.fail(
                "Tailscale desired-slot rules: multiple publications target the same current-node TCP port",
            );
        } else {
            report.pass("Tailscale desired-slot rules");
        }
    }

    let services_by_name: HashMap<&str, &Service> = declaration
        .services
        .iter()
        .map(|service| (service.name.as_str(), service))
        .collect();
    let mut checked_backends = HashSet::new();
    println!("Local backends");

    for publication in &publishing_intent.publications {
        let Some(service) = services_by_name.get(publication.service.as_str()) else {
            continue;
        };

        if !checked_backends.insert(service.port) {
            continue;
        }

        let address = format!("127.0.0.1:{}", service.port);
        if local_tcp_backend::ensure_reachable(service.port).is_ok() {
            report.pass(format!("{} {address}", service.name));
        } else {
            report.fail(format!("{} {address} is not reachable", service.name));
        }
    }

    if !desired_tailscale.is_empty() && !tailscale_slots_are_ambiguous {
        println!("Tailscale");

        match tailscale_serve_tcp::inspect_client_prerequisites() {
            Ok(()) => {
                report.pass("Client state permits endpoint resolution");
                let ports: Vec<u16> = desired_tailscale.iter().map(|(_, port)| *port).collect();

                match tailscale_serve_tcp::observe_ports(&ports) {
                    Ok(states) => {
                        for ((service, _), state) in desired_tailscale.iter().zip(states) {
                            match state {
                                tailscale_serve_tcp::ProviderState::Absent
                                | tailscale_serve_tcp::ProviderState::AlreadySatisfied => report
                                    .pass(format!("{service} Serve slot permits apply")),
                                tailscale_serve_tcp::ProviderState::Conflict => report.fail(
                                    format!(
                                        "{service} Serve slot is blocked by incompatible existing state"
                                    ),
                                ),
                                tailscale_serve_tcp::ProviderState::Indeterminate => report.fail(
                                    format!("{service} Serve slot cannot be classified safely"),
                                ),
                            }
                        }
                    }
                    Err(error) => report.fail(format!(
                        "Serve state observation: {}",
                        diagnostic_error_detail(&error)
                    )),
                }
            }
            Err(error) => report.fail(format!(
                "Tailscale client prerequisites: {}",
                diagnostic_error_detail(&error)
            )),
        }
    }

    let desired_openssh_services: Vec<&str> = publishing_intent
        .publications
        .iter()
        .filter(|publication| publication.publisher == OPENSSH_REVERSE_TCP_PUBLISHER)
        .map(|publication| publication.service.as_str())
        .collect();

    if !desired_openssh_services.is_empty() {
        println!("OpenSSH");
        let declared_services: Vec<&str> = declaration
            .services
            .iter()
            .map(|service| service.name.as_str())
            .collect();

        match openssh_reverse_tcp::load_and_resolve_desired(
            &declared_services,
            &desired_openssh_services,
        ) {
            Ok(publications) => {
                report.pass("Provider configuration and desired-slot rules");

                match openssh_reverse_tcp::inspect_runtime() {
                    Ok(runtime) => {
                        report.pass("Runtime path has no unsafe existing entry");

                        for service in &desired_openssh_services {
                            let publication = match publications.get(service) {
                                Ok(publication) => publication,
                                Err(error) => {
                                    report.fail(format!(
                                        "{service} provider configuration: {}",
                                        diagnostic_error_detail(&error)
                                    ));
                                    continue;
                                }
                            };

                            match openssh_reverse_tcp::inspect_control_path(&runtime, publication) {
                                Ok(control_path) => {
                                    report.pass(format!(
                                        "{service} ssh executable and control-path resolution"
                                    ));

                                    match openssh_reverse_tcp::inspect_existing_master(
                                        &runtime,
                                        &control_path,
                                        publication,
                                    ) {
                                        Ok(openssh_reverse_tcp::ExistingMaster::Absent) => report
                                            .pass(format!(
                                                "{service} local control state: no existing master"
                                            )),
                                        Ok(openssh_reverse_tcp::ExistingMaster::Responding) => {
                                            report.pass(format!(
                                                "{service} local control state: existing master responds"
                                            ));
                                        }
                                        Err(error) => report.fail(format!(
                                            "{service} local control state: {}",
                                            diagnostic_error_detail(&error)
                                        )),
                                    }
                                }
                                Err(error) => report.fail(format!(
                                    "{service} ssh executable and control-path resolution: {}",
                                    diagnostic_error_detail(&error)
                                )),
                            }
                        }
                    }
                    Err(error) => report.fail(format!(
                        "Runtime path inspection: {}",
                        diagnostic_error_detail(&error)
                    )),
                }
            }
            Err(error) => report.fail(format!(
                "Provider configuration and desired-slot rules: {}",
                diagnostic_error_detail(&error)
            )),
        }

        report.info(
            "OpenSSH remote reachability, host-key trust, credentials, authentication, forwarding policy, and listener availability were not probed",
        );
    }

    report.finish()
}

fn diagnostic_error_detail(error: &str) -> &str {
    error.strip_prefix("error: ").unwrap_or(error)
}

fn publish(service_name: &str, publisher: &str) -> ExitCode {
    let declaration = match load_declaration() {
        Ok(declaration) => declaration,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(1);
        }
    };

    let publishing_intent = match load_publishing_intent(&declaration) {
        Ok(publishing_intent) => publishing_intent,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(1);
        }
    };

    let service = match declaration
        .services
        .iter()
        .find(|service| service.name == service_name)
    {
        Some(service) => service,
        None => {
            eprintln!("error: selected service does not exist");
            return ExitCode::from(1);
        }
    };

    if !publishing_intent.publications.iter().any(|publication| {
        publication.service == service_name && publication.publisher == publisher
    }) {
        eprintln!("error: selected publication is not in desired state");
        return ExitCode::from(1);
    }

    match publisher {
        TAILSCALE_SERVE_TCP_PUBLISHER => {
            if tailscale_serve_tcp_publications_are_ambiguous(&declaration, &publishing_intent) {
                eprintln!("error: desired Tailscale Serve TCP publications conflict");
                return ExitCode::from(1);
            }

            match ensure_tailscale_publication(service_name, service.port) {
                Ok(success) => {
                    println!("{success}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::from(1)
                }
            }
        }
        OPENSSH_REVERSE_TCP_PUBLISHER => {
            let declared_services: Vec<&str> = declaration
                .services
                .iter()
                .map(|service| service.name.as_str())
                .collect();
            let desired_services: Vec<&str> = publishing_intent
                .publications
                .iter()
                .filter(|publication| publication.publisher == OPENSSH_REVERSE_TCP_PUBLISHER)
                .map(|publication| publication.service.as_str())
                .collect();
            let publication = match openssh_reverse_tcp::load_and_resolve(
                service_name,
                &declared_services,
                &desired_services,
            ) {
                Ok(publication) => publication,
                Err(error) => {
                    eprintln!("{error}");
                    return ExitCode::from(1);
                }
            };

            match ensure_openssh_publication(service_name, service.port, &publication) {
                Ok(success) => {
                    println!("{success}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::from(1)
                }
            }
        }
        _ => {
            eprintln!("error: selected publisher is not supported");
            ExitCode::from(1)
        }
    }
}

fn apply() -> ExitCode {
    let declaration = match load_declaration() {
        Ok(declaration) => declaration,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(1);
        }
    };

    let publishing_intent = match load_publishing_intent(&declaration) {
        Ok(publishing_intent) => publishing_intent,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(1);
        }
    };

    if publishing_intent
        .publications
        .iter()
        .any(|publication| !publisher_is_operationally_supported(&publication.publisher))
    {
        eprintln!("error: desired publisher is not supported");
        return ExitCode::from(1);
    }

    if tailscale_serve_tcp_publications_are_ambiguous(&declaration, &publishing_intent) {
        eprintln!("error: desired Tailscale Serve TCP publications conflict");
        return ExitCode::from(1);
    }

    let declared_services: Vec<&str> = declaration
        .services
        .iter()
        .map(|service| service.name.as_str())
        .collect();
    let desired_openssh_services: Vec<&str> = publishing_intent
        .publications
        .iter()
        .filter(|publication| publication.publisher == OPENSSH_REVERSE_TCP_PUBLISHER)
        .map(|publication| publication.service.as_str())
        .collect();
    let resolved_openssh = if desired_openssh_services.is_empty() {
        None
    } else {
        match openssh_reverse_tcp::load_and_resolve_desired(
            &declared_services,
            &desired_openssh_services,
        ) {
            Ok(publications) => Some(publications),
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::from(1);
            }
        }
    };

    let services_by_name: HashMap<&str, &Service> = declaration
        .services
        .iter()
        .map(|service| (service.name.as_str(), service))
        .collect();
    let mut checked_backends = HashSet::new();

    for publication in &publishing_intent.publications {
        let Some(service) = services_by_name.get(publication.service.as_str()) else {
            eprintln!("{INVALID_PUBLISHING_ERROR}");
            return ExitCode::from(1);
        };

        if checked_backends.insert(service.port)
            && let Err(error) = local_tcp_backend::ensure_reachable(service.port)
        {
            eprintln!("{error}");
            return ExitCode::from(1);
        }
    }

    if resolved_openssh.is_some()
        && let Err(error) = openssh_reverse_tcp::preflight_runtime()
    {
        eprintln!("{error}");
        return ExitCode::from(1);
    }

    if publishing_intent.publications.is_empty() {
        println!("No desired publications to apply");
        return ExitCode::SUCCESS;
    }

    let mut successful_publications = 0_usize;

    for publication in &publishing_intent.publications {
        let Some(service) = services_by_name.get(publication.service.as_str()) else {
            eprintln!("{INVALID_PUBLISHING_ERROR}");
            return ExitCode::from(1);
        };
        let result = match publication.publisher.as_str() {
            TAILSCALE_SERVE_TCP_PUBLISHER => {
                ensure_tailscale_publication(&publication.service, service.port)
            }
            OPENSSH_REVERSE_TCP_PUBLISHER => resolved_openssh
                .as_ref()
                .ok_or_else(|| {
                    "error: desired OpenSSH reverse TCP publication has no provider configuration"
                        .to_owned()
                })
                .and_then(|publications| publications.get(&publication.service))
                .and_then(|provider_publication| {
                    ensure_openssh_publication(
                        &publication.service,
                        service.port,
                        provider_publication,
                    )
                }),
            _ => Err("error: desired publisher is not supported".to_owned()),
        };

        match result {
            Ok(success) => {
                println!("{success}");
                successful_publications += 1;
            }
            Err(error) => {
                eprintln!(
                    "{}",
                    contextual_apply_error(
                        &publication.service,
                        &publication.publisher,
                        &error,
                        successful_publications,
                    )
                );
                return ExitCode::from(1);
            }
        }
    }

    println!(
        "Applied {} desired publications",
        publishing_intent.publications.len()
    );
    ExitCode::SUCCESS
}

fn publisher_is_operationally_supported(publisher: &str) -> bool {
    matches!(
        publisher,
        TAILSCALE_SERVE_TCP_PUBLISHER | OPENSSH_REVERSE_TCP_PUBLISHER
    )
}

fn ensure_tailscale_publication(service_name: &str, port: u16) -> Result<String, String> {
    match tailscale_serve_tcp::ensure(port)? {
        EnsureOutcome::Created { endpoint } => Ok(format!(
            "Published {service_name} via {TAILSCALE_SERVE_TCP_PUBLISHER} at {endpoint}"
        )),
        EnsureOutcome::AlreadySatisfied { endpoint } => Ok(format!(
            "Publication already satisfied for {service_name} via {TAILSCALE_SERVE_TCP_PUBLISHER} at {endpoint}"
        )),
    }
}

fn ensure_openssh_publication(
    service_name: &str,
    local_port: u16,
    publication: &openssh_reverse_tcp::ProviderPublication,
) -> Result<String, String> {
    openssh_reverse_tcp::ensure(local_port, publication)?;

    Ok(format!(
        "Ensured {service_name} via {OPENSSH_REVERSE_TCP_PUBLISHER}: {}@{}:{} listen {}:{} -> 127.0.0.1:{local_port} (SSH-session-backed; no automatic reconnect)",
        publication.user,
        publication.host,
        publication.ssh_port,
        publication.listen_address,
        publication.listen_port,
    ))
}

fn contextual_apply_error(
    service: &str,
    publisher: &str,
    error: &str,
    successful_publications: usize,
) -> String {
    let detail = error.strip_prefix("error: ").unwrap_or(error);
    let rollback_context = if successful_publications == 0 {
        ""
    } else {
        "; earlier successful publications were not rolled back"
    };

    format!("error: apply failed for {service} via {publisher}: {detail}{rollback_context}")
}

fn status(service_name: &str, publisher: &str) -> ExitCode {
    let declaration = match load_declaration() {
        Ok(declaration) => declaration,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(1);
        }
    };

    let publishing_intent = match load_publishing_intent(&declaration) {
        Ok(publishing_intent) => publishing_intent,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(1);
        }
    };

    let service = match declaration
        .services
        .iter()
        .find(|service| service.name == service_name)
    {
        Some(service) => service,
        None => {
            eprintln!("error: selected service does not exist");
            return ExitCode::from(1);
        }
    };

    if !publishing_intent.publications.iter().any(|publication| {
        publication.service == service_name && publication.publisher == publisher
    }) {
        eprintln!("error: selected publication is not in desired state");
        return ExitCode::from(1);
    }

    if publisher != TAILSCALE_SERVE_TCP_PUBLISHER {
        eprintln!("error: selected publisher is not supported");
        return ExitCode::from(1);
    }

    if tailscale_serve_tcp_publications_are_ambiguous(&declaration, &publishing_intent) {
        eprintln!("error: desired Tailscale Serve TCP publications conflict");
        return ExitCode::from(1);
    }

    let relation = match tailscale_serve_tcp::observe(service.port) {
        Ok(tailscale_serve_tcp::ProviderState::Absent) => "absent",
        Ok(tailscale_serve_tcp::ProviderState::AlreadySatisfied) => "satisfied",
        Ok(tailscale_serve_tcp::ProviderState::Conflict) => "conflict",
        Ok(tailscale_serve_tcp::ProviderState::Indeterminate) => "indeterminate",
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(1);
        }
    };

    println!("Publication status for {service_name} via {publisher}: {relation}");
    ExitCode::SUCCESS
}

fn load_publishing_intent(declaration: &Declaration) -> Result<PublishingIntent, &'static str> {
    let bytes = match fs::read(PUBLISHING_PATH) {
        Ok(bytes) => bytes,
        Err(_) => return Err(PUBLISHING_READ_ERROR),
    };

    let source = match std::str::from_utf8(&bytes) {
        Ok(source) => source,
        Err(_) => return Err(PUBLISHING_UTF8_ERROR),
    };

    let publishing_intent: PublishingIntent = match toml::from_str(source) {
        Ok(publishing_intent) => publishing_intent,
        Err(_) => return Err(INVALID_PUBLISHING_ERROR),
    };

    if !publishing_intent_is_valid(&publishing_intent, declaration) {
        return Err(INVALID_PUBLISHING_ERROR);
    }

    Ok(publishing_intent)
}

fn load_declaration() -> Result<Declaration, &'static str> {
    let bytes = match fs::read(DECLARATION_PATH) {
        Ok(bytes) => bytes,
        Err(_) => return Err(DECLARATION_READ_ERROR),
    };

    let source = match std::str::from_utf8(&bytes) {
        Ok(source) => source,
        Err(_) => return Err(DECLARATION_UTF8_ERROR),
    };

    let declaration: Declaration = match toml::from_str(source) {
        Ok(declaration) => declaration,
        Err(_) => return Err(INVALID_DECLARATION_ERROR),
    };

    if !declaration_is_valid(&declaration) {
        return Err(INVALID_DECLARATION_ERROR);
    }

    Ok(declaration)
}

fn declaration_is_valid(declaration: &Declaration) -> bool {
    let mut names = HashSet::with_capacity(declaration.services.len());

    declaration.services.iter().all(|service| {
        !service.name.is_empty()
            && service.name.trim() == service.name
            && !service.name.chars().any(char::is_control)
            && service.port != 0
            && names.insert(service.name.as_str())
    })
}

fn publishing_intent_is_valid(
    publishing_intent: &PublishingIntent,
    declaration: &Declaration,
) -> bool {
    if !publishing_intent
        .publications
        .iter()
        .all(|publication| publisher_token_is_valid(&publication.publisher))
    {
        return false;
    }

    let service_names: HashSet<&str> = declaration
        .services
        .iter()
        .map(|service| service.name.as_str())
        .collect();

    if !publishing_intent
        .publications
        .iter()
        .all(|publication| service_names.contains(publication.service.as_str()))
    {
        return false;
    }

    let mut publications = HashSet::with_capacity(publishing_intent.publications.len());

    publishing_intent.publications.iter().all(|publication| {
        publications.insert((publication.service.as_str(), publication.publisher.as_str()))
    })
}

fn tailscale_serve_tcp_publications_are_ambiguous(
    declaration: &Declaration,
    publishing_intent: &PublishingIntent,
) -> bool {
    let service_ports: HashMap<&str, u16> = declaration
        .services
        .iter()
        .map(|service| (service.name.as_str(), service.port))
        .collect();
    let mut selected_ports = HashSet::new();

    for publication in &publishing_intent.publications {
        if publication.publisher != TAILSCALE_SERVE_TCP_PUBLISHER {
            continue;
        }

        let Some(port) = service_ports.get(publication.service.as_str()) else {
            return true;
        };

        if !selected_ports.insert(*port) {
            return true;
        }
    }

    false
}

fn publisher_token_is_valid(publisher: &str) -> bool {
    let bytes = publisher.as_bytes();

    if !matches!(bytes.first(), Some(b'a'..=b'z')) {
        return false;
    }

    let mut previous_was_hyphen = false;

    for &byte in &bytes[1..] {
        match byte {
            b'a'..=b'z' | b'0'..=b'9' => previous_was_hyphen = false,
            b'-' if !previous_was_hyphen => previous_was_hyphen = true,
            _ => return false,
        }
    }

    !previous_was_hyphen
}
