use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let command = args.next();

    match (command.as_deref(), args.next()) {
        (Some("version"), None) => {
            println!("Aequimuta {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("Usage: aequimuta version");
            ExitCode::from(2)
        }
    }
}
