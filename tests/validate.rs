mod support;

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::Path;

use support::{TestDirectory, run_aequimuta};

const DECLARATION_FILE: &str = "aequimuta.toml";
const VALID_STDOUT: &[u8] = b"aequimuta.toml is valid\n";
const INVALID_DECLARATION_STDERR: &[u8] = b"error: aequimuta.toml is not a valid declaration\n";
const INVALID_UTF8_STDERR: &[u8] = b"error: aequimuta.toml is not valid UTF-8\n";
const READ_ERROR_STDERR: &[u8] = b"error: failed to read aequimuta.toml\n";

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn directory_entries(path: &Path) -> io::Result<Vec<OsString>> {
    let mut entries = fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort();
    Ok(entries)
}

fn assert_valid_declaration(label: &str, contents: &[u8]) -> TestResult {
    let directory = TestDirectory::new(label)?;
    let declaration_path = directory.path().join(DECLARATION_FILE);
    fs::write(&declaration_path, contents)?;
    let entries_before = directory_entries(directory.path())?;

    let output = run_aequimuta(directory.path(), &["validate"])?;

    assert_eq!(
        output.status.code(),
        Some(0),
        "{label}: unexpected exit status"
    );
    assert_eq!(
        output.stdout.as_slice(),
        VALID_STDOUT,
        "{label}: unexpected stdout"
    );
    assert!(output.stderr.is_empty(), "{label}: unexpected stderr");
    assert_eq!(
        fs::read(&declaration_path)?,
        contents,
        "{label}: declaration changed"
    );
    assert_eq!(
        directory_entries(directory.path())?,
        entries_before,
        "{label}: directory entries changed"
    );

    directory.cleanup()?;
    Ok(())
}

fn assert_invalid_declaration(label: &str, contents: &[u8], expected_stderr: &[u8]) -> TestResult {
    let directory = TestDirectory::new(label)?;
    let declaration_path = directory.path().join(DECLARATION_FILE);
    fs::write(&declaration_path, contents)?;
    let entries_before = directory_entries(directory.path())?;

    let output = run_aequimuta(directory.path(), &["validate"])?;

    assert_eq!(
        output.status.code(),
        Some(1),
        "{label}: unexpected exit status"
    );
    assert!(output.stdout.is_empty(), "{label}: unexpected stdout");
    assert_eq!(
        output.stderr.as_slice(),
        expected_stderr,
        "{label}: unexpected stderr"
    );
    assert_eq!(
        fs::read(&declaration_path)?,
        contents,
        "{label}: declaration changed"
    );
    assert_eq!(
        directory_entries(directory.path())?,
        entries_before,
        "{label}: directory entries changed"
    );

    directory.cleanup()?;
    Ok(())
}

#[test]
fn validate_accepts_valid_declarations() -> TestResult {
    let cases: &[(&str, &[u8])] = &[
        (
            "validate-comment-only",
            b"# Aequimuta service declarations\n",
        ),
        ("validate-services-absent", b""),
        ("validate-services-empty", b"services = []\n"),
        (
            "validate-single-minimum-port",
            br#"[[services]]
name = "web"
port = 1
"#,
        ),
        (
            "validate-multiple-identity-boundaries",
            br#"[[services]]
name = "web"
port = 8080

[[services]]
name = "api"
port = 8080

[[services]]
name = "Web"
port = 3000

[[services]]
name = "\u00e9"
port = 65535

[[services]]
name = "e\u0301"
port = 4000
"#,
        ),
    ];

    for (label, contents) in cases {
        assert_valid_declaration(label, contents)?;
    }

    Ok(())
}

#[test]
fn validate_rejects_invalid_declarations() -> TestResult {
    let cases: &[(&str, &[u8])] = &[
        (
            "validate-duplicate-name",
            br#"[[services]]
name = "web"
port = 8080

[[services]]
name = "web"
port = 3000
"#,
        ),
        (
            "validate-missing-name",
            br#"[[services]]
port = 8080
"#,
        ),
        (
            "validate-missing-port",
            br#"[[services]]
name = "web"
"#,
        ),
        (
            "validate-empty-name",
            br#"[[services]]
name = ""
port = 8080
"#,
        ),
        (
            "validate-leading-whitespace-name",
            br#"[[services]]
name = " web"
port = 8080
"#,
        ),
        (
            "validate-trailing-whitespace-name",
            br#"[[services]]
name = "web "
port = 8080
"#,
        ),
        (
            "validate-control-character-name",
            br#"[[services]]
name = "we\u0001b"
port = 8080
"#,
        ),
        (
            "validate-zero-port",
            br#"[[services]]
name = "web"
port = 0
"#,
        ),
        (
            "validate-port-above-range",
            br#"[[services]]
name = "web"
port = 65536
"#,
        ),
        (
            "validate-port-wrong-type",
            br#"[[services]]
name = "web"
port = "8080"
"#,
        ),
        (
            "validate-name-wrong-type",
            br#"[[services]]
name = 8080
port = 8080
"#,
        ),
        ("validate-unknown-root-field", b"publisher = \"example\"\n"),
        (
            "validate-unknown-service-field",
            br#"[[services]]
name = "web"
port = 8080
protocol = "tcp"
"#,
        ),
        ("validate-services-wrong-type", b"services = \"web\"\n"),
        ("validate-non-table-service-entry", b"services = [8080]\n"),
        ("validate-malformed-toml", b"[[services]\n"),
    ];

    for (label, contents) in cases {
        assert_invalid_declaration(label, contents, INVALID_DECLARATION_STDERR)?;
    }

    assert_invalid_declaration("validate-invalid-utf8", &[0xff], INVALID_UTF8_STDERR)?;

    Ok(())
}

#[test]
fn validate_does_not_search_parent_directories() -> TestResult {
    let directory = TestDirectory::new("validate-no-parent-search")?;
    let parent_declaration = directory.path().join(DECLARATION_FILE);
    let original_contents = b"services = []\n";
    fs::write(&parent_declaration, original_contents)?;
    let child = directory.path().join("child");
    fs::create_dir(&child)?;
    let parent_entries_before = directory_entries(directory.path())?;

    let output = run_aequimuta(&child, &["validate"])?;

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, READ_ERROR_STDERR);
    assert_eq!(fs::read(parent_declaration)?, original_contents);
    assert!(directory_entries(&child)?.is_empty());
    assert_eq!(directory_entries(directory.path())?, parent_entries_before);

    directory.cleanup()?;
    Ok(())
}

#[test]
fn validate_reports_other_read_errors_without_panicking() -> TestResult {
    let directory = TestDirectory::new("validate-read-error")?;
    let declaration_path = directory.path().join(DECLARATION_FILE);
    fs::create_dir(&declaration_path)?;
    let entries_before = directory_entries(directory.path())?;

    let output = run_aequimuta(directory.path(), &["validate"])?;

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, READ_ERROR_STDERR);
    assert!(declaration_path.is_dir());
    assert_eq!(directory_entries(directory.path())?, entries_before);

    directory.cleanup()?;
    Ok(())
}

#[test]
fn validate_rejects_additional_arguments_before_reading() -> TestResult {
    let directory = TestDirectory::new("validate-extra-argument")?;
    let declaration_path = directory.path().join(DECLARATION_FILE);
    let original_contents = [0xff];
    fs::write(&declaration_path, original_contents)?;
    let entries_before = directory_entries(directory.path())?;

    let output = run_aequimuta(directory.path(), &["validate", "extra"])?;

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, b"Usage: aequimuta version\n");
    assert_eq!(fs::read(declaration_path)?, original_contents);
    assert_eq!(directory_entries(directory.path())?, entries_before);

    directory.cleanup()?;
    Ok(())
}
