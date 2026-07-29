use serde::Deserialize;
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::process::ExitCode;

const DECLARATION_PATH: &str = "aequimuta.toml";
const PUBLISHING_PATH: &str = "aequimuta.publish.toml";
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
    let mut args = std::env::args().skip(1);
    let command = args.next();

    match (command.as_deref(), args.next()) {
        (Some("version"), None) => {
            println!("Aequimuta {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        (Some("init"), None) => init(),
        (Some("validate"), None) => validate(),
        (Some("validate-publishing"), None) => validate_publishing(),
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

    let bytes = match fs::read(PUBLISHING_PATH) {
        Ok(bytes) => bytes,
        Err(_) => {
            eprintln!("{PUBLISHING_READ_ERROR}");
            return ExitCode::from(1);
        }
    };

    let source = match std::str::from_utf8(&bytes) {
        Ok(source) => source,
        Err(_) => {
            eprintln!("{PUBLISHING_UTF8_ERROR}");
            return ExitCode::from(1);
        }
    };

    let publishing_intent: PublishingIntent = match toml::from_str(source) {
        Ok(publishing_intent) => publishing_intent,
        Err(_) => {
            eprintln!("{INVALID_PUBLISHING_ERROR}");
            return ExitCode::from(1);
        }
    };

    if !publishing_intent_is_valid(&publishing_intent, &declaration) {
        eprintln!("{INVALID_PUBLISHING_ERROR}");
        return ExitCode::from(1);
    }

    println!("aequimuta.publish.toml is valid");
    ExitCode::SUCCESS
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
