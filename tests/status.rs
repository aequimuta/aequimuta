mod support;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use support::{TestDirectory, run_aequimuta as run_aequimuta_default};

const DECLARATION_FILE: &str = "aequimuta.toml";
const PUBLISHING_FILE: &str = "aequimuta.publish.toml";
const SUPPORTED_PUBLISHER: &str = "tailscale-serve-tcp";
const USAGE_STDERR: &[u8] = b"Usage: aequimuta <command>\n";

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Project {
    directory: TestDirectory,
    declaration: Vec<u8>,
    publishing: Vec<u8>,
    entries: Vec<OsString>,
}

impl Project {
    fn new(label: &str, declaration: &[u8], publishing: &[u8]) -> io::Result<Self> {
        let directory = TestDirectory::new(label)?;
        fs::write(directory.path().join(DECLARATION_FILE), declaration)?;
        fs::write(directory.path().join(PUBLISHING_FILE), publishing)?;

        Ok(Self {
            entries: directory_entries(directory.path())?,
            directory,
            declaration: declaration.to_vec(),
            publishing: publishing.to_vec(),
        })
    }

    fn path(&self) -> &Path {
        self.directory.path()
    }

    fn assert_unchanged(&self) -> io::Result<()> {
        assert_eq!(
            fs::read(self.path().join(DECLARATION_FILE))?,
            self.declaration,
            "service declaration changed"
        );
        assert_eq!(
            fs::read(self.path().join(PUBLISHING_FILE))?,
            self.publishing,
            "publishing intent changed"
        );
        assert_eq!(
            directory_entries(self.path())?,
            self.entries,
            "project directory entries changed"
        );
        Ok(())
    }

    fn cleanup(self) -> io::Result<()> {
        self.directory.cleanup()
    }
}

struct FakeTailscale {
    directory: TestDirectory,
    log: PathBuf,
}

impl FakeTailscale {
    fn new(label: &str) -> io::Result<Self> {
        let directory = TestDirectory::new(label)?;
        let executable = directory.path().join("tailscale");
        let log = directory.path().join("calls.log");

        fs::write(&executable, FAKE_TAILSCALE)?;
        let mut permissions = fs::metadata(&executable)?.permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&executable, permissions)?;

        Ok(Self { directory, log })
    }

    fn path(&self) -> &Path {
        self.directory.path()
    }

    fn environment(&self, provider_json: &str, fail_status: bool) -> Vec<(&'static str, OsString)> {
        vec![
            ("PATH", self.path().as_os_str().to_owned()),
            ("AEQUIMUTA_FAKE_JSON", OsString::from(provider_json)),
            (
                "AEQUIMUTA_FAKE_FAIL",
                OsString::from(if fail_status { "1" } else { "0" }),
            ),
            ("AEQUIMUTA_FAKE_LOG", self.log.as_os_str().to_owned()),
        ]
    }

    fn calls(&self) -> io::Result<Vec<String>> {
        match fs::read_to_string(&self.log) {
            Ok(log) => Ok(log.lines().map(str::to_owned).collect()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    fn cleanup(self) -> io::Result<()> {
        self.directory.cleanup()
    }
}

fn directory_entries(path: &Path) -> io::Result<Vec<OsString>> {
    let mut entries = fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort();
    Ok(entries)
}

fn run_aequimuta_with_env<I, K, V>(
    directory: &Path,
    args: &[&str],
    environment: I,
) -> io::Result<Output>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    Command::new(env!("CARGO_BIN_EXE_aequimuta"))
        .args(args)
        .current_dir(directory)
        .envs(environment)
        .output()
}

fn service_declaration(name: &str, port: u16) -> Vec<u8> {
    format!(
        "[[services]]\n\
         name = \"{name}\"\n\
         port = {port}\n"
    )
    .into_bytes()
}

fn publishing_intent(service: &str, publisher: &str) -> Vec<u8> {
    format!(
        "[[publications]]\n\
         service = \"{service}\"\n\
         publisher = \"{publisher}\"\n"
    )
    .into_bytes()
}

fn assert_command_failure(output: &Output, label: &str) {
    assert_eq!(
        output.status.code(),
        Some(1),
        "{label}: unexpected exit status"
    );
    assert!(output.stdout.is_empty(), "{label}: unexpected stdout");
    assert!(
        output.stderr.starts_with(b"error: "),
        "{label}: stderr does not start with error prefix: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.ends_with(b"\n"),
        "{label}: stderr does not end with a newline"
    );
    assert_eq!(
        output.stderr.iter().filter(|&&byte| byte == b'\n').count(),
        1,
        "{label}: stderr is not exactly one line: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_status_success(output: &Output, service: &str, publisher: &str, relation: &str) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "{relation}: unexpected exit status"
    );
    assert_eq!(
        output.stdout,
        format!("Publication status for {service} via {publisher}: {relation}\n").as_bytes(),
        "{relation}: unexpected stdout"
    );
    assert!(output.stderr.is_empty(), "{relation}: unexpected stderr");
}

fn assert_only_status_observation(calls: &[String], label: &str) {
    assert_eq!(
        calls,
        ["serve status --json"],
        "{label}: status must perform exactly one read-only provider observation"
    );
}

fn closed_local_port() -> io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

#[test]
fn status_rejects_invalid_argument_counts_before_reading_files() -> TestResult {
    let project = Project::new("status-usage", &[0xff], &[0xff])?;
    let cases: &[(&str, &[&str])] = &[
        ("missing-service-and-publisher", &["status"]),
        ("missing-publisher", &["status", "web"]),
        (
            "additional-argument",
            &["status", "web", SUPPORTED_PUBLISHER, "extra"],
        ),
    ];

    for &(label, args) in cases {
        let output = run_aequimuta_default(project.path(), args)?;

        assert_eq!(output.status.code(), Some(2), "{label}: unexpected status");
        assert!(output.stdout.is_empty(), "{label}: unexpected stdout");
        assert_eq!(output.stderr, USAGE_STDERR, "{label}: unexpected stderr");
        project.assert_unchanged()?;
    }

    project.cleanup()?;
    Ok(())
}

#[test]
fn status_selection_failures_do_not_invoke_tailscale() -> TestResult {
    let port = 41_237;
    let cases = [
        (
            "invalid-declaration",
            b"[[services]\n".to_vec(),
            publishing_intent("web", SUPPORTED_PUBLISHER),
            "web",
            SUPPORTED_PUBLISHER,
        ),
        (
            "invalid-publishing-intent",
            service_declaration("web", port),
            b"[[publications]\n".to_vec(),
            "web",
            SUPPORTED_PUBLISHER,
        ),
        (
            "service-absent",
            service_declaration("web", port),
            publishing_intent("web", SUPPORTED_PUBLISHER),
            "missing",
            SUPPORTED_PUBLISHER,
        ),
        (
            "service-identity-is-case-sensitive",
            service_declaration("web", port),
            publishing_intent("web", SUPPORTED_PUBLISHER),
            "Web",
            SUPPORTED_PUBLISHER,
        ),
        (
            "publication-absent",
            service_declaration("web", port),
            publishing_intent("web", "example-publisher"),
            "web",
            SUPPORTED_PUBLISHER,
        ),
        (
            "unsupported-tailscale",
            service_declaration("web", port),
            publishing_intent("web", "tailscale"),
            "web",
            "tailscale",
        ),
        (
            "unsupported-example-publisher",
            service_declaration("web", port),
            publishing_intent("web", "example-publisher"),
            "web",
            "example-publisher",
        ),
        (
            "unsupported-cloudflare",
            service_declaration("web", port),
            publishing_intent("web", "cloudflare"),
            "web",
            "cloudflare",
        ),
        (
            "unsupported-headscale",
            service_declaration("web", port),
            publishing_intent("web", "headscale"),
            "web",
            "headscale",
        ),
    ];

    for (label, declaration, publishing, service, publisher) in cases {
        let project = Project::new(label, &declaration, &publishing)?;
        let fake = FakeTailscale::new(label)?;
        let output = run_aequimuta_with_env(
            project.path(),
            &["status", service, publisher],
            fake.environment("{}", false),
        )?;

        assert_command_failure(&output, label);
        assert!(
            fake.calls()?.is_empty(),
            "{label}: provider was invoked before selection completed"
        );
        project.assert_unchanged()?;

        fake.cleanup()?;
        project.cleanup()?;
    }

    Ok(())
}

#[test]
fn status_rejects_same_port_desired_ambiguity_before_provider_calls() -> TestResult {
    let port = 41_238;
    let declaration = format!(
        "[[services]]\n\
         name = \"web\"\n\
         port = {port}\n\
         \n\
         [[services]]\n\
         name = \"api\"\n\
         port = {port}\n"
    );
    let publishing = b"[[publications]]\n\
                       service = \"web\"\n\
                       publisher = \"tailscale-serve-tcp\"\n\
                       \n\
                       [[publications]]\n\
                       service = \"api\"\n\
                       publisher = \"tailscale-serve-tcp\"\n";
    let project = Project::new(
        "status-same-port-ambiguity",
        declaration.as_bytes(),
        publishing,
    )?;
    let fake = FakeTailscale::new("status-same-port-ambiguity")?;

    let output = run_aequimuta_with_env(
        project.path(),
        &["status", "web", SUPPORTED_PUBLISHER],
        fake.environment("{}", false),
    )?;

    assert_command_failure(&output, "same-port-ambiguity");
    assert!(
        fake.calls()?.is_empty(),
        "same-port ambiguity invoked provider"
    );
    project.assert_unchanged()?;

    fake.cleanup()?;
    project.cleanup()?;
    Ok(())
}

#[test]
fn status_reports_provider_command_failures_without_leaking_output() -> TestResult {
    let port = 41_239;
    let declaration = service_declaration("web", port);
    let publishing = publishing_intent("web", SUPPORTED_PUBLISHER);

    let missing_project = Project::new("status-missing-executable", &declaration, &publishing)?;
    let empty_path = TestDirectory::new("status-empty-path")?;
    let missing_output = run_aequimuta_with_env(
        missing_project.path(),
        &["status", "web", SUPPORTED_PUBLISHER],
        [("PATH", empty_path.path().as_os_str())],
    )?;

    assert_command_failure(&missing_output, "missing-executable");
    missing_project.assert_unchanged()?;
    empty_path.cleanup()?;
    missing_project.cleanup()?;

    let failed_project = Project::new("status-command-failure", &declaration, &publishing)?;
    let fake = FakeTailscale::new("status-command-failure")?;
    let failed_output = run_aequimuta_with_env(
        failed_project.path(),
        &["status", "web", SUPPORTED_PUBLISHER],
        fake.environment("{}", true),
    )?;

    assert_command_failure(&failed_output, "provider-status-nonzero");
    assert!(
        !String::from_utf8_lossy(&failed_output.stderr).contains("fake provider"),
        "raw provider stderr leaked to the CLI"
    );
    assert_only_status_observation(&fake.calls()?, "provider-status-nonzero");
    failed_project.assert_unchanged()?;

    fake.cleanup()?;
    failed_project.cleanup()?;

    let malformed_project = Project::new("status-malformed-json", &declaration, &publishing)?;
    let fake = FakeTailscale::new("status-malformed-json")?;
    let malformed_output = run_aequimuta_with_env(
        malformed_project.path(),
        &["status", "web", SUPPORTED_PUBLISHER],
        fake.environment(r#"{"TCP":"#, false),
    )?;

    assert_command_failure(&malformed_output, "malformed-provider-json");
    assert_only_status_observation(&fake.calls()?, "malformed-provider-json");
    malformed_project.assert_unchanged()?;

    fake.cleanup()?;
    malformed_project.cleanup()?;
    Ok(())
}

#[test]
fn status_reports_absent_without_mutation() -> TestResult {
    let port = closed_local_port()?;
    let project = Project::new(
        "status-absent",
        &service_declaration("web", port),
        &publishing_intent("web", SUPPORTED_PUBLISHER),
    )?;
    let fake = FakeTailscale::new("status-absent")?;
    let output = run_aequimuta_with_env(
        project.path(),
        &["status", "web", SUPPORTED_PUBLISHER],
        fake.environment(r#"{"TCP":{"443":{"HTTPS":true}}}"#, false),
    )?;

    assert_status_success(&output, "web", SUPPORTED_PUBLISHER, "absent");
    assert_only_status_observation(&fake.calls()?, "absent");
    project.assert_unchanged()?;

    fake.cleanup()?;
    project.cleanup()?;
    Ok(())
}

#[test]
fn status_reports_satisfied_without_a_local_backend_listener() -> TestResult {
    let port = closed_local_port()?;
    let project = Project::new(
        "status-satisfied",
        &service_declaration("web", port),
        &publishing_intent("web", SUPPORTED_PUBLISHER),
    )?;
    let fake = FakeTailscale::new("status-satisfied")?;
    let provider_json = format!(r#"{{"TCP":{{"{port}":{{"TCPForward":"127.0.0.1:{port}"}}}}}}"#);
    let output = run_aequimuta_with_env(
        project.path(),
        &["status", "web", SUPPORTED_PUBLISHER],
        fake.environment(&provider_json, false),
    )?;

    assert_status_success(&output, "web", SUPPORTED_PUBLISHER, "satisfied");
    assert_only_status_observation(&fake.calls()?, "satisfied");
    project.assert_unchanged()?;

    fake.cleanup()?;
    project.cleanup()?;
    Ok(())
}

#[test]
fn status_reports_supported_provider_conflicts_without_mutation() -> TestResult {
    for mode in [
        "different-target",
        "tls-terminated-tcp",
        "http-serve",
        "https-serve",
        "funnel-enabled",
    ] {
        let port = closed_local_port()?;
        let provider_json = match mode {
            "different-target" => {
                format!(r#"{{"TCP":{{"{port}":{{"TCPForward":"127.0.0.1:1"}}}}}}"#)
            }
            "tls-terminated-tcp" => format!(
                r#"{{"TCP":{{"{port}":{{"TCPForward":"127.0.0.1:{port}","TerminateTLS":"status-test.example.ts.net"}}}}}}"#
            ),
            "http-serve" => format!(r#"{{"TCP":{{"{port}":{{"HTTP":true}}}}}}"#),
            "https-serve" => format!(r#"{{"TCP":{{"{port}":{{"HTTPS":true}}}}}}"#),
            "funnel-enabled" => format!(
                r#"{{"TCP":{{"{port}":{{"TCPForward":"127.0.0.1:{port}"}}}},"AllowFunnel":{{"status-test.example.ts.net:{port}":true}}}}"#
            ),
            _ => unreachable!(),
        };
        let project = Project::new(
            mode,
            &service_declaration("web", port),
            &publishing_intent("web", SUPPORTED_PUBLISHER),
        )?;
        let fake = FakeTailscale::new(mode)?;
        let output = run_aequimuta_with_env(
            project.path(),
            &["status", "web", SUPPORTED_PUBLISHER],
            fake.environment(&provider_json, false),
        )?;

        assert_status_success(&output, "web", SUPPORTED_PUBLISHER, "conflict");
        assert_only_status_observation(&fake.calls()?, mode);
        project.assert_unchanged()?;

        fake.cleanup()?;
        project.cleanup()?;
    }

    Ok(())
}

#[test]
fn status_reports_indeterminate_for_valid_noncanonical_tcp_key() -> TestResult {
    let port = closed_local_port()?;
    let project = Project::new(
        "status-indeterminate",
        &service_declaration("web", port),
        &publishing_intent("web", SUPPORTED_PUBLISHER),
    )?;
    let fake = FakeTailscale::new("status-indeterminate")?;
    let provider_json = format!(r#"{{"TCP":{{"0{port}":{{"TCPForward":"127.0.0.1:{port}"}}}}}}"#);
    let output = run_aequimuta_with_env(
        project.path(),
        &["status", "web", SUPPORTED_PUBLISHER],
        fake.environment(&provider_json, false),
    )?;

    assert_status_success(&output, "web", SUPPORTED_PUBLISHER, "indeterminate");
    assert_only_status_observation(&fake.calls()?, "indeterminate");
    project.assert_unchanged()?;

    fake.cleanup()?;
    project.cleanup()?;
    Ok(())
}

const FAKE_TAILSCALE: &[u8] = br#"#!/bin/sh
set -eu

printf '%s\n' "$*" >> "$AEQUIMUTA_FAKE_LOG"

if [ "$#" -ne 3 ] \
    || [ "$1" != "serve" ] \
    || [ "$2" != "status" ] \
    || [ "$3" != "--json" ]; then
    printf 'unexpected fake tailscale command: %s\n' "$*" >&2
    exit 99
fi

if [ "$AEQUIMUTA_FAKE_FAIL" = "1" ]; then
    printf 'fake provider diagnostic\nsecond line\n' >&2
    exit 23
fi

printf '%s\n' "$AEQUIMUTA_FAKE_JSON"
"#;
