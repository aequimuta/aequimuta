mod support;

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::Path;

use support::{TestDirectory, run_aequimuta};

const DECLARATION_FILE: &str = "aequimuta.toml";
const PUBLISHING_FILE: &str = "aequimuta.publish.toml";
const VALID_STDOUT: &[u8] = b"aequimuta.publish.toml is valid\n";
const INVALID_PUBLISHING_STDERR: &[u8] =
    b"error: aequimuta.publish.toml is not a valid publishing intent\n";
const INVALID_PUBLISHING_UTF8_STDERR: &[u8] = b"error: aequimuta.publish.toml is not valid UTF-8\n";
const PUBLISHING_READ_ERROR_STDERR: &[u8] = b"error: failed to read aequimuta.publish.toml\n";
const INVALID_DECLARATION_STDERR: &[u8] = b"error: aequimuta.toml is not a valid declaration\n";
const DECLARATION_READ_ERROR_STDERR: &[u8] = b"error: failed to read aequimuta.toml\n";
const USAGE_STDERR: &[u8] = b"Usage: aequimuta <command>\n";
const WEB_DECLARATION: &[u8] = br#"[[services]]
name = "web"
port = 8080
"#;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn directory_entries(path: &Path) -> io::Result<Vec<OsString>> {
    let mut entries = fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort();
    Ok(entries)
}

fn assert_valid_publishing(label: &str, declaration: &[u8], publishing: &[u8]) -> TestResult {
    let directory = TestDirectory::new(label)?;
    let declaration_path = directory.path().join(DECLARATION_FILE);
    let publishing_path = directory.path().join(PUBLISHING_FILE);
    fs::write(&declaration_path, declaration)?;
    fs::write(&publishing_path, publishing)?;
    let entries_before = directory_entries(directory.path())?;

    let output = run_aequimuta(directory.path(), &["validate-publishing"])?;

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
        declaration,
        "{label}: service declaration changed"
    );
    assert_eq!(
        fs::read(&publishing_path)?,
        publishing,
        "{label}: publishing intent changed"
    );
    assert_eq!(
        directory_entries(directory.path())?,
        entries_before,
        "{label}: directory entries changed"
    );

    directory.cleanup()?;
    Ok(())
}

fn assert_invalid_publishing(
    label: &str,
    declaration: &[u8],
    publishing: &[u8],
    expected_stderr: &[u8],
) -> TestResult {
    let directory = TestDirectory::new(label)?;
    let declaration_path = directory.path().join(DECLARATION_FILE);
    let publishing_path = directory.path().join(PUBLISHING_FILE);
    fs::write(&declaration_path, declaration)?;
    fs::write(&publishing_path, publishing)?;
    let entries_before = directory_entries(directory.path())?;

    let output = run_aequimuta(directory.path(), &["validate-publishing"])?;

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
        declaration,
        "{label}: service declaration changed"
    );
    assert_eq!(
        fs::read(&publishing_path)?,
        publishing,
        "{label}: publishing intent changed"
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
fn validate_publishing_accepts_valid_intents() -> TestResult {
    let cases: &[(&str, &[u8], &[u8])] = &[
        ("publishing-empty-file", WEB_DECLARATION, b""),
        ("publishing-whitespace-only", WEB_DECLARATION, b" \n\t\n"),
        (
            "publishing-comment-only",
            WEB_DECLARATION,
            b"# No desired publications.\n",
        ),
        (
            "publishing-publications-omitted",
            WEB_DECLARATION,
            b"# The publications field is intentionally omitted.\n",
        ),
        (
            "publishing-empty-collection",
            WEB_DECLARATION,
            b"publications = []\n",
        ),
        (
            "publishing-single-publication",
            WEB_DECLARATION,
            br#"[[publications]]
service = "web"
publisher = "example-publisher"
"#,
        ),
        (
            "publishing-same-service-different-publishers",
            WEB_DECLARATION,
            br#"[[publications]]
service = "web"
publisher = "example-publisher"

[[publications]]
service = "web"
publisher = "another-publisher"
"#,
        ),
        (
            "publishing-different-services-same-publisher",
            br#"[[services]]
name = "web"
port = 8080

[[services]]
name = "api"
port = 3000
"#,
            br#"[[publications]]
service = "web"
publisher = "example-publisher"

[[publications]]
service = "api"
publisher = "example-publisher"
"#,
        ),
        (
            "publishing-token-single-segment",
            WEB_DECLARATION,
            br#"[[publications]]
service = "web"
publisher = "example"
"#,
        ),
        (
            "publishing-token-hyphenated",
            WEB_DECLARATION,
            br#"[[publications]]
service = "web"
publisher = "example-publisher"
"#,
        ),
        (
            "publishing-token-with-digits",
            WEB_DECLARATION,
            br#"[[publications]]
service = "web"
publisher = "publisher2-test3"
"#,
        ),
        (
            "publishing-exact-service-reference",
            br#"[[services]]
name = "Web"
port = 8080
"#,
            br#"[[publications]]
service = "Web"
publisher = "example-publisher"
"#,
        ),
    ];

    for &(label, declaration, publishing) in cases {
        assert_valid_publishing(label, declaration, publishing)?;
    }

    Ok(())
}

#[test]
fn validate_publishing_rejects_invalid_intents() -> TestResult {
    let cases: &[(&str, &[u8], &[u8])] = &[
        (
            "publishing-malformed-toml",
            WEB_DECLARATION,
            b"[[publications]\n",
        ),
        (
            "publishing-invalid-root-type",
            WEB_DECLARATION,
            b"publications = \"web\"\n",
        ),
        (
            "publishing-invalid-entry-shape",
            WEB_DECLARATION,
            b"publications = [8080]\n",
        ),
        (
            "publishing-missing-service",
            WEB_DECLARATION,
            br#"[[publications]]
publisher = "example-publisher"
"#,
        ),
        (
            "publishing-missing-publisher",
            WEB_DECLARATION,
            br#"[[publications]]
service = "web"
"#,
        ),
        (
            "publishing-invalid-service-type",
            WEB_DECLARATION,
            br#"[[publications]]
service = 8080
publisher = "example-publisher"
"#,
        ),
        (
            "publishing-invalid-publisher-type",
            WEB_DECLARATION,
            br#"[[publications]]
service = "web"
publisher = 8080
"#,
        ),
        (
            "publishing-unknown-service",
            WEB_DECLARATION,
            br#"[[publications]]
service = "Web"
publisher = "example-publisher"
"#,
        ),
        (
            "publishing-duplicate-pair",
            WEB_DECLARATION,
            br#"[[publications]]
service = "web"
publisher = "example-publisher"

[[publications]]
service = "web"
publisher = "example-publisher"
"#,
        ),
        (
            "publishing-token-starts-uppercase",
            WEB_DECLARATION,
            br#"[[publications]]
service = "web"
publisher = "Example"
"#,
        ),
        (
            "publishing-token-starts-with-digit",
            WEB_DECLARATION,
            br#"[[publications]]
service = "web"
publisher = "2example"
"#,
        ),
        (
            "publishing-token-leading-hyphen",
            WEB_DECLARATION,
            br#"[[publications]]
service = "web"
publisher = "-example"
"#,
        ),
        (
            "publishing-token-trailing-hyphen",
            WEB_DECLARATION,
            br#"[[publications]]
service = "web"
publisher = "example-"
"#,
        ),
        (
            "publishing-token-consecutive-hyphen",
            WEB_DECLARATION,
            br#"[[publications]]
service = "web"
publisher = "example--publisher"
"#,
        ),
        (
            "publishing-token-contains-dot",
            WEB_DECLARATION,
            br#"[[publications]]
service = "web"
publisher = "example.publisher"
"#,
        ),
        (
            "publishing-token-contains-underscore",
            WEB_DECLARATION,
            br#"[[publications]]
service = "web"
publisher = "example_publisher"
"#,
        ),
        (
            "publishing-token-contains-whitespace",
            WEB_DECLARATION,
            br#"[[publications]]
service = "web"
publisher = "example publisher"
"#,
        ),
        (
            "publishing-unknown-root-field",
            WEB_DECLARATION,
            b"mode = \"test\"\n",
        ),
        (
            "publishing-unknown-publication-field",
            WEB_DECLARATION,
            br#"[[publications]]
service = "web"
publisher = "example-publisher"
mode = "test"
"#,
        ),
    ];

    for &(label, declaration, publishing) in cases {
        assert_invalid_publishing(label, declaration, publishing, INVALID_PUBLISHING_STDERR)?;
    }

    assert_invalid_publishing(
        "publishing-invalid-utf8",
        WEB_DECLARATION,
        &[0xff],
        INVALID_PUBLISHING_UTF8_STDERR,
    )?;

    Ok(())
}

#[test]
fn validate_publishing_requires_both_input_files() -> TestResult {
    let missing_declaration = TestDirectory::new("publishing-missing-declaration")?;
    let publishing_path = missing_declaration.path().join(PUBLISHING_FILE);
    let publishing = br#"[[publications]]
service = "web"
publisher = "example-publisher"
"#;
    fs::write(&publishing_path, publishing)?;
    let entries_before = directory_entries(missing_declaration.path())?;

    let output = run_aequimuta(missing_declaration.path(), &["validate-publishing"])?;

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr.as_slice(), DECLARATION_READ_ERROR_STDERR);
    assert!(
        !missing_declaration
            .path()
            .join(DECLARATION_FILE)
            .try_exists()?
    );
    assert_eq!(fs::read(&publishing_path)?, publishing);
    assert_eq!(
        directory_entries(missing_declaration.path())?,
        entries_before
    );
    missing_declaration.cleanup()?;

    let missing_publishing = TestDirectory::new("publishing-missing-intent")?;
    let declaration_path = missing_publishing.path().join(DECLARATION_FILE);
    fs::write(&declaration_path, WEB_DECLARATION)?;
    let entries_before = directory_entries(missing_publishing.path())?;

    let output = run_aequimuta(missing_publishing.path(), &["validate-publishing"])?;

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr.as_slice(), PUBLISHING_READ_ERROR_STDERR);
    assert_eq!(fs::read(&declaration_path)?, WEB_DECLARATION);
    assert!(
        !missing_publishing
            .path()
            .join(PUBLISHING_FILE)
            .try_exists()?
    );
    assert_eq!(
        directory_entries(missing_publishing.path())?,
        entries_before
    );
    missing_publishing.cleanup()?;

    Ok(())
}

#[test]
fn validate_publishing_validates_service_declaration_first() -> TestResult {
    let directory = TestDirectory::new("publishing-declaration-first")?;
    let declaration_path = directory.path().join(DECLARATION_FILE);
    let malformed_declaration = b"[[services]\n";
    fs::write(&declaration_path, malformed_declaration)?;
    let entries_before = directory_entries(directory.path())?;

    let output = run_aequimuta(directory.path(), &["validate-publishing"])?;

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr.as_slice(), INVALID_DECLARATION_STDERR);
    assert_eq!(fs::read(&declaration_path)?, malformed_declaration);
    assert!(!directory.path().join(PUBLISHING_FILE).try_exists()?);
    assert_eq!(directory_entries(directory.path())?, entries_before);

    directory.cleanup()?;
    Ok(())
}

#[test]
fn validate_publishing_does_not_search_parent_directories() -> TestResult {
    let directory = TestDirectory::new("publishing-no-parent-search")?;
    let parent_declaration_path = directory.path().join(DECLARATION_FILE);
    let parent_publishing_path = directory.path().join(PUBLISHING_FILE);
    let parent_publishing = b"publications = []\n";
    fs::write(&parent_declaration_path, WEB_DECLARATION)?;
    fs::write(&parent_publishing_path, parent_publishing)?;

    let no_inputs = directory.path().join("no-inputs");
    let declaration_only = directory.path().join("declaration-only");
    fs::create_dir(&no_inputs)?;
    fs::create_dir(&declaration_only)?;
    let child_declaration_path = declaration_only.join(DECLARATION_FILE);
    fs::write(&child_declaration_path, WEB_DECLARATION)?;

    let parent_entries_before = directory_entries(directory.path())?;
    let no_inputs_entries_before = directory_entries(&no_inputs)?;
    let declaration_only_entries_before = directory_entries(&declaration_only)?;

    let output = run_aequimuta(&no_inputs, &["validate-publishing"])?;

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr.as_slice(), DECLARATION_READ_ERROR_STDERR);

    let output = run_aequimuta(&declaration_only, &["validate-publishing"])?;

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr.as_slice(), PUBLISHING_READ_ERROR_STDERR);
    assert_eq!(fs::read(&parent_declaration_path)?, WEB_DECLARATION);
    assert_eq!(fs::read(&parent_publishing_path)?, parent_publishing);
    assert_eq!(fs::read(&child_declaration_path)?, WEB_DECLARATION);
    assert!(!no_inputs.join(DECLARATION_FILE).try_exists()?);
    assert!(!no_inputs.join(PUBLISHING_FILE).try_exists()?);
    assert!(!declaration_only.join(PUBLISHING_FILE).try_exists()?);
    assert_eq!(directory_entries(directory.path())?, parent_entries_before);
    assert_eq!(directory_entries(&no_inputs)?, no_inputs_entries_before);
    assert_eq!(
        directory_entries(&declaration_only)?,
        declaration_only_entries_before
    );

    directory.cleanup()?;
    Ok(())
}

#[test]
fn validate_publishing_rejects_additional_arguments_before_reading() -> TestResult {
    let directory = TestDirectory::new("publishing-extra-argument")?;
    let declaration_path = directory.path().join(DECLARATION_FILE);
    let publishing_path = directory.path().join(PUBLISHING_FILE);
    let original_contents = [0xff];
    fs::write(&declaration_path, original_contents)?;
    fs::write(&publishing_path, original_contents)?;
    let entries_before = directory_entries(directory.path())?;

    let output = run_aequimuta(directory.path(), &["validate-publishing", "extra"])?;

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr.as_slice(), USAGE_STDERR);
    assert_eq!(fs::read(&declaration_path)?, original_contents);
    assert_eq!(fs::read(&publishing_path)?, original_contents);
    assert_eq!(directory_entries(directory.path())?, entries_before);

    directory.cleanup()?;
    Ok(())
}
