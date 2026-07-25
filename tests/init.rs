use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

const DECLARATION_FILE: &str = "aequimuta.toml";
const INITIAL_DECLARATION: &[u8] = b"# Aequimuta service declarations\n";
static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory {
    path: PathBuf,
    cleaned: bool,
}

impl TestDirectory {
    fn new(label: &str) -> io::Result<Self> {
        loop {
            let sequence = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "aequimuta-{label}-{}-{sequence}",
                std::process::id()
            ));

            match fs::create_dir(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        cleaned: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn cleanup(mut self) -> io::Result<()> {
        fs::remove_dir_all(&self.path)?;
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        if !self.cleaned
            && let Err(error) = fs::remove_dir_all(&self.path)
        {
            eprintln!(
                "failed to clean up test directory {}: {error}",
                self.path.display()
            );
        }
    }
}

fn run_aequimuta(directory: &TestDirectory, args: &[&str]) -> io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_aequimuta"))
        .args(args)
        .current_dir(directory.path())
        .output()
}

#[test]
fn init_creates_minimal_declaration() -> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new("init-success")?;

    let output = run_aequimuta(&directory, &["init"])?;

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

    let output = run_aequimuta(&directory, &["init"])?;

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

    let output = run_aequimuta(&directory, &["init", "extra"])?;

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, b"Usage: aequimuta version\n");
    assert!(!directory.path().join(DECLARATION_FILE).try_exists()?);

    directory.cleanup()?;
    Ok(())
}
