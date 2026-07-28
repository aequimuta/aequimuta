use serde::Deserialize;
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::process::ExitCode;

const DECLARATION_PATH: &str = "aequimuta.toml";
const INITIAL_DECLARATION: &[u8] = b"# Aequimuta service declarations\n";
const INVALID_DECLARATION_ERROR: &str = "error: aequimuta.toml is not a valid declaration";

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
        _ => {
            eprintln!("Usage: aequimuta version");
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
    let bytes = match fs::read(DECLARATION_PATH) {
        Ok(bytes) => bytes,
        Err(_) => {
            eprintln!("error: failed to read aequimuta.toml");
            return ExitCode::from(1);
        }
    };

    let source = match std::str::from_utf8(&bytes) {
        Ok(source) => source,
        Err(_) => {
            eprintln!("error: aequimuta.toml is not valid UTF-8");
            return ExitCode::from(1);
        }
    };

    let declaration: Declaration = match toml::from_str(source) {
        Ok(declaration) => declaration,
        Err(_) => {
            eprintln!("{INVALID_DECLARATION_ERROR}");
            return ExitCode::from(1);
        }
    };

    if !declaration_is_valid(&declaration) {
        eprintln!("{INVALID_DECLARATION_ERROR}");
        return ExitCode::from(1);
    }

    println!("aequimuta.toml is valid");
    ExitCode::SUCCESS
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
