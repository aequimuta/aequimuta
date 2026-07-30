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

            match tailscale_serve_tcp::ensure(service.port) {
                Ok(EnsureOutcome::Created { endpoint }) => {
                    println!("Published {service_name} via {publisher} at {endpoint}");
                    ExitCode::SUCCESS
                }
                Ok(EnsureOutcome::AlreadySatisfied { endpoint }) => {
                    println!(
                        "Publication already satisfied for {service_name} via {publisher} at {endpoint}"
                    );
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

            if let Err(error) = openssh_reverse_tcp::ensure(service.port, &publication) {
                eprintln!("{error}");
                return ExitCode::from(1);
            }

            println!(
                "Ensured {service_name} via {publisher}: {}@{}:{} listen {}:{} -> 127.0.0.1:{} (SSH-session-backed; no automatic reconnect)",
                publication.user,
                publication.host,
                publication.ssh_port,
                publication.listen_address,
                publication.listen_port,
                service.port
            );
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("error: selected publisher is not supported");
            ExitCode::from(1)
        }
    }
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
