mod support;

use std::fs;

use support::{TestDirectory, run_aequimuta};

const DECLARATION_FILE: &str = "aequimuta.toml";
const INITIAL_DECLARATION: &[u8] = b"# Aequimuta service declarations\n";

#[test]
fn init_creates_minimal_declaration() -> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new("init-success")?;

    let output = run_aequimuta(directory.path(), &["init"])?;

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        output.stdout,
        b"Initialized Aequimuta project at aequimuta.toml\n"
    );
    assert!(output.stderr.is_empty());

    let declaration_path = directory.path().join(DECLARATION_FILE);
    assert!(declaration_path.is_file());
    assert_eq!(fs::read(declaration_path)?, INITIAL_DECLARATION);

    let entries = fs::read_dir(directory.path())?.collect::<Result<Vec<_>, _>>()?;
    assert_eq!(entries.len(), 1);

    directory.cleanup()?;
    Ok(())
}

#[test]
fn init_does_not_overwrite_existing_declaration() -> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new("init-existing")?;
    let declaration_path = directory.path().join(DECLARATION_FILE);
    let original_content = b"existing declaration\n";
    fs::write(&declaration_path, original_content)?;

    let output = run_aequimuta(directory.path(), &["init"])?;

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, b"error: aequimuta.toml already exists\n");
    assert_eq!(fs::read(declaration_path)?, original_content);

    directory.cleanup()?;
    Ok(())
}

#[test]
fn init_rejects_additional_arguments() -> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new("init-extra-argument")?;

    let output = run_aequimuta(directory.path(), &["init", "extra"])?;

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, b"Usage: aequimuta <command>\n");
    assert!(!directory.path().join(DECLARATION_FILE).try_exists()?);

    directory.cleanup()?;
    Ok(())
}
