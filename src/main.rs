use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::process::ExitCode;

const DECLARATION_PATH: &str = "aequimuta.toml";
const INITIAL_DECLARATION: &[u8] = b"# Aequimuta service declarations\n";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let command = args.next();

    match (command.as_deref(), args.next()) {
        (Some("version"), None) => {
            println!("Aequimuta {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        (Some("init"), None) => init(),
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
