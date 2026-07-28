use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(0);

pub(crate) struct TestDirectory {
    path: PathBuf,
    cleaned: bool,
}

impl TestDirectory {
    pub(crate) fn new(label: &str) -> io::Result<Self> {
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

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn cleanup(mut self) -> io::Result<()> {
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

pub(crate) fn run_aequimuta(directory: &Path, args: &[&str]) -> io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_aequimuta"))
        .args(args)
        .current_dir(directory)
        .output()
}
