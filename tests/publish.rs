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
const TEST_DNS_NAME: &str = "publish-test.example.ts.net";
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
    state: PathBuf,
}

impl FakeTailscale {
    fn new(label: &str) -> io::Result<Self> {
        let directory = TestDirectory::new(label)?;
        let executable = directory.path().join("tailscale");
        let log = directory.path().join("calls.log");
        let state = directory.path().join("mutated");

        fs::write(&executable, FAKE_TAILSCALE)?;
        let mut permissions = fs::metadata(&executable)?.permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&executable, permissions)?;

        Ok(Self {
            directory,
            log,
            state,
        })
    }

    fn path(&self) -> &Path {
        self.directory.path()
    }

    fn environment(&self, mode: &str, port: u16) -> Vec<(&'static str, OsString)> {
        vec![
            ("PATH", self.path().as_os_str().to_owned()),
            ("AEQUIMUTA_FAKE_MODE", OsString::from(mode)),
            ("AEQUIMUTA_FAKE_PORT", OsString::from(port.to_string())),
            ("AEQUIMUTA_FAKE_LOG", self.log.as_os_str().to_owned()),
            ("AEQUIMUTA_FAKE_STATE", self.state.as_os_str().to_owned()),
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

fn assert_operation_failure(output: &Output, label: &str) {
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

fn assert_no_mutation(calls: &[String], label: &str) {
    assert!(
        calls.iter().all(|call| !call.starts_with("serve --bg ")),
        "{label}: unexpected mutation call: {calls:?}"
    );
    assert_no_broad_or_unrelated_commands(calls, label);
}

fn assert_no_broad_or_unrelated_commands(calls: &[String], label: &str) {
    assert!(
        calls.iter().all(|call| {
            !call.contains("funnel")
                && !call.contains("reset")
                && !call.ends_with(" off")
                && !call.contains(" serve off")
        }),
        "{label}: forbidden provider command: {calls:?}"
    );
}

fn expected_created_stdout(service: &str, port: u16) -> Vec<u8> {
    format!("Published {service} via {SUPPORTED_PUBLISHER} at tcp://{TEST_DNS_NAME}:{port}\n")
        .into_bytes()
}

fn expected_already_satisfied_stdout(service: &str, port: u16) -> Vec<u8> {
    format!(
        "Publication already satisfied for {service} via {SUPPORTED_PUBLISHER} at tcp://{TEST_DNS_NAME}:{port}\n"
    )
    .into_bytes()
}

fn expected_read_only_calls() -> Vec<String> {
    vec!["status --json".to_owned(), "serve status --json".to_owned()]
}

fn expected_creation_calls(port: u16) -> Vec<String> {
    vec![
        "status --json".to_owned(),
        "serve status --json".to_owned(),
        format!("serve --bg --tcp={port} tcp://127.0.0.1:{port}"),
        "serve status --json".to_owned(),
    ]
}

#[test]
fn publish_rejects_invalid_argument_counts_before_reading_files() -> TestResult {
    let project = Project::new("publish-usage", &[0xff], &[0xff])?;
    let cases: &[(&str, &[&str])] = &[
        ("missing-service-and-publisher", &["publish"]),
        ("missing-publisher", &["publish", "web"]),
        (
            "additional-argument",
            &["publish", "web", SUPPORTED_PUBLISHER, "extra"],
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
fn publish_selection_failures_do_not_invoke_tailscale() -> TestResult {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();

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
            &["publish", service, publisher],
            fake.environment("already-satisfied", port),
        )?;

        assert_operation_failure(&output, label);
        let calls = fake.calls()?;
        assert!(
            calls.is_empty(),
            "{label}: tailscale was invoked: {calls:?}"
        );
        project.assert_unchanged()?;

        fake.cleanup()?;
        project.cleanup()?;
    }

    drop(listener);
    Ok(())
}

#[test]
fn publish_rejects_same_port_desired_collision_before_provider_calls() -> TestResult {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
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
        "publish-same-port-collision",
        declaration.as_bytes(),
        publishing,
    )?;
    let fake = FakeTailscale::new("publish-same-port-collision")?;

    let output = run_aequimuta_with_env(
        project.path(),
        &["publish", "web", SUPPORTED_PUBLISHER],
        fake.environment("already-satisfied", port),
    )?;

    assert_operation_failure(&output, "same-port-collision");
    assert!(
        fake.calls()?.is_empty(),
        "same-port collision invoked provider"
    );
    project.assert_unchanged()?;

    fake.cleanup()?;
    project.cleanup()?;
    drop(listener);
    Ok(())
}

#[test]
fn publish_preflight_failures_do_not_mutate_provider_state() -> TestResult {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let reachable_port = listener.local_addr()?.port();
    let declaration = service_declaration("web", reachable_port);
    let publishing = publishing_intent("web", SUPPORTED_PUBLISHER);

    let missing_executable_project =
        Project::new("publish-missing-executable", &declaration, &publishing)?;
    let empty_path = TestDirectory::new("publish-empty-path")?;
    let output = run_aequimuta_with_env(
        missing_executable_project.path(),
        &["publish", "web", SUPPORTED_PUBLISHER],
        [("PATH", empty_path.path().as_os_str())],
    )?;
    assert_operation_failure(&output, "missing-executable");
    missing_executable_project.assert_unchanged()?;
    empty_path.cleanup()?;
    missing_executable_project.cleanup()?;

    for (label, mode, expected_calls) in [
        (
            "daemon-status-failure",
            "daemon-status-failure",
            vec!["status --json".to_owned()],
        ),
        (
            "daemon-not-running",
            "daemon-not-running",
            vec!["status --json".to_owned()],
        ),
        (
            "magic-dns-disabled",
            "magic-dns-disabled",
            vec!["status --json".to_owned()],
        ),
        (
            "magic-dns-missing",
            "magic-dns-missing",
            vec!["status --json".to_owned()],
        ),
        (
            "serve-status-failure",
            "serve-status-failure",
            expected_read_only_calls(),
        ),
        (
            "invalid-provider-json",
            "invalid-provider-json",
            expected_read_only_calls(),
        ),
        (
            "unsupported-provider-state",
            "unsupported-provider-state",
            expected_read_only_calls(),
        ),
    ] {
        let project = Project::new(label, &declaration, &publishing)?;
        let fake = FakeTailscale::new(label)?;
        let output = run_aequimuta_with_env(
            project.path(),
            &["publish", "web", SUPPORTED_PUBLISHER],
            fake.environment(mode, reachable_port),
        )?;

        assert_operation_failure(&output, label);
        let calls = fake.calls()?;
        assert_eq!(calls, expected_calls, "{label}: unexpected provider calls");
        assert_no_mutation(&calls, label);
        project.assert_unchanged()?;

        fake.cleanup()?;
        project.cleanup()?;
    }

    let unavailable_listener = TcpListener::bind(("127.0.0.1", 0))?;
    let unavailable_port = unavailable_listener.local_addr()?.port();
    drop(unavailable_listener);
    let project = Project::new(
        "publish-backend-unreachable",
        &service_declaration("web", unavailable_port),
        &publishing_intent("web", SUPPORTED_PUBLISHER),
    )?;
    let fake = FakeTailscale::new("publish-backend-unreachable")?;
    let output = run_aequimuta_with_env(
        project.path(),
        &["publish", "web", SUPPORTED_PUBLISHER],
        fake.environment("already-satisfied", unavailable_port),
    )?;

    assert_operation_failure(&output, "backend-unreachable");
    let calls = fake.calls()?;
    assert_eq!(
        calls,
        vec!["status --json".to_owned()],
        "backend-unreachable: provider state should not be observed after failed reachability"
    );
    assert_no_mutation(&calls, "backend-unreachable");
    project.assert_unchanged()?;

    fake.cleanup()?;
    project.cleanup()?;
    drop(listener);
    Ok(())
}

#[test]
fn publish_reports_already_satisfied_without_mutation() -> TestResult {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let declaration = service_declaration("web", port);
    let publishing = publishing_intent("web", SUPPORTED_PUBLISHER);
    let project = Project::new("publish-already-satisfied", &declaration, &publishing)?;
    let fake = FakeTailscale::new("publish-already-satisfied")?;

    let output = run_aequimuta_with_env(
        project.path(),
        &["publish", "web", SUPPORTED_PUBLISHER],
        fake.environment("already-satisfied", port),
    )?;

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        output.stdout,
        expected_already_satisfied_stdout("web", port)
    );
    assert!(output.stderr.is_empty());
    let calls = fake.calls()?;
    assert_eq!(calls, expected_read_only_calls());
    assert_no_mutation(&calls, "already-satisfied");
    project.assert_unchanged()?;

    fake.cleanup()?;
    project.cleanup()?;
    drop(listener);
    Ok(())
}

#[test]
fn publish_rejects_conflicting_provider_states_without_mutation() -> TestResult {
    let cases = [
        ("different-target", "different-target"),
        ("tls-terminated-tcp", "tls-terminated-tcp"),
        ("http-serve", "http-serve"),
        ("https-serve", "https-serve"),
        ("funnel-enabled", "funnel-enabled"),
        ("foreground-state", "foreground-state"),
        ("proxy-protocol", "proxy-protocol"),
        ("unknown-handler-state", "unknown-handler-state"),
    ];

    for (label, mode) in cases {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let project = Project::new(
            label,
            &service_declaration("web", port),
            &publishing_intent("web", SUPPORTED_PUBLISHER),
        )?;
        let fake = FakeTailscale::new(label)?;

        let output = run_aequimuta_with_env(
            project.path(),
            &["publish", "web", SUPPORTED_PUBLISHER],
            fake.environment(mode, port),
        )?;

        assert_operation_failure(&output, label);
        let calls = fake.calls()?;
        assert_eq!(
            calls,
            expected_read_only_calls(),
            "{label}: unexpected calls"
        );
        assert_no_mutation(&calls, label);
        project.assert_unchanged()?;

        fake.cleanup()?;
        project.cleanup()?;
        drop(listener);
    }

    Ok(())
}

#[test]
fn publish_creates_one_exact_mapping_and_verifies_post_state() -> TestResult {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let project = Project::new(
        "publish-created",
        &service_declaration("web", port),
        &publishing_intent("web", SUPPORTED_PUBLISHER),
    )?;
    let fake = FakeTailscale::new("publish-created")?;

    let output = run_aequimuta_with_env(
        project.path(),
        &["publish", "web", SUPPORTED_PUBLISHER],
        fake.environment("create", port),
    )?;

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, expected_created_stdout("web", port));
    assert!(output.stderr.is_empty());
    let calls = fake.calls()?;
    assert_eq!(calls, expected_creation_calls(port));
    assert_no_broad_or_unrelated_commands(&calls, "created");
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("serve --bg "))
            .count(),
        1,
        "creation must execute exactly one mutation"
    );

    let second_output = run_aequimuta_with_env(
        project.path(),
        &["publish", "web", SUPPORTED_PUBLISHER],
        fake.environment("create", port),
    )?;
    assert_eq!(second_output.status.code(), Some(0));
    assert_eq!(
        second_output.stdout,
        expected_already_satisfied_stdout("web", port)
    );
    assert!(second_output.stderr.is_empty());
    let calls = fake.calls()?;
    let mut expected_calls = expected_creation_calls(port);
    expected_calls.extend(expected_read_only_calls());
    assert_eq!(calls, expected_calls);
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("serve --bg "))
            .count(),
        1,
        "the already-satisfied retry must not mutate"
    );
    assert_no_broad_or_unrelated_commands(&calls, "created-retry");
    project.assert_unchanged()?;

    fake.cleanup()?;
    project.cleanup()?;
    drop(listener);
    Ok(())
}

#[test]
fn publish_rejects_bad_post_state_after_successful_mutation() -> TestResult {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let project = Project::new(
        "publish-bad-post-state",
        &service_declaration("web", port),
        &publishing_intent("web", SUPPORTED_PUBLISHER),
    )?;
    let fake = FakeTailscale::new("publish-bad-post-state")?;

    let output = run_aequimuta_with_env(
        project.path(),
        &["publish", "web", SUPPORTED_PUBLISHER],
        fake.environment("bad-post-state", port),
    )?;

    assert_operation_failure(&output, "bad-post-state");
    let calls = fake.calls()?;
    assert_eq!(calls, expected_creation_calls(port));
    assert_no_broad_or_unrelated_commands(&calls, "bad-post-state");
    project.assert_unchanged()?;

    fake.cleanup()?;
    project.cleanup()?;
    drop(listener);
    Ok(())
}

#[test]
fn publish_reports_mutation_failure_and_only_reobserves_state() -> TestResult {
    for mode in ["mutation-failure", "mutation-failure-desired"] {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let project = Project::new(
            mode,
            &service_declaration("web", port),
            &publishing_intent("web", SUPPORTED_PUBLISHER),
        )?;
        let fake = FakeTailscale::new(mode)?;

        let output = run_aequimuta_with_env(
            project.path(),
            &["publish", "web", SUPPORTED_PUBLISHER],
            fake.environment(mode, port),
        )?;

        assert_operation_failure(&output, mode);
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("fake provider"),
            "{mode}: raw provider stderr leaked to the CLI"
        );
        let calls = fake.calls()?;
        assert_eq!(calls, expected_creation_calls(port));
        assert_no_broad_or_unrelated_commands(&calls, mode);
        project.assert_unchanged()?;

        fake.cleanup()?;
        project.cleanup()?;
        drop(listener);
    }

    Ok(())
}

const FAKE_TAILSCALE: &[u8] = br#"#!/bin/sh
set -eu

printf '%s\n' "$*" >> "$AEQUIMUTA_FAKE_LOG"

mode=$AEQUIMUTA_FAKE_MODE
port=$AEQUIMUTA_FAKE_PORT
unrelated_port=443
if [ "$port" = "$unrelated_port" ]; then
    unrelated_port=444
fi
unrelated="{\"TCP\":{\"$unrelated_port\":{\"HTTPS\":true}},\"Web\":{\"unrelated.example.ts.net:$unrelated_port\":{\"Handlers\":{}}},\"Services\":{\"svc:unrelated\":{\"TCP\":{\"$port\":{\"TCPForward\":\"127.0.0.1:9090\"}}}},\"AllowFunnel\":{\"unrelated.example.ts.net:$unrelated_port\":true}}"
desired_with_unrelated="{\"TCP\":{\"$unrelated_port\":{\"HTTPS\":true},\"$port\":{\"TCPForward\":\"127.0.0.1:$port\"}},\"Web\":{\"unrelated.example.ts.net:$unrelated_port\":{\"Handlers\":{}}},\"Services\":{\"svc:unrelated\":{\"TCP\":{\"$port\":{\"TCPForward\":\"127.0.0.1:9090\"}}}},\"AllowFunnel\":{\"unrelated.example.ts.net:$unrelated_port\":true}}"

if [ "$#" -eq 2 ] && [ "$1" = "status" ] && [ "$2" = "--json" ]; then
    if [ "$mode" = "daemon-status-failure" ]; then
        printf 'fake provider daemon diagnostic\nsecond line\n' >&2
        exit 20
    fi

    if [ "$mode" = "daemon-not-running" ]; then
        printf '{"BackendState":"Stopped","Self":{"DNSName":"publish-test.example.ts.net.","Online":false},"CurrentTailnet":{"MagicDNSEnabled":true}}\n'
        exit 0
    fi

    if [ "$mode" = "magic-dns-disabled" ]; then
        printf '{"BackendState":"Running","Self":{"DNSName":"publish-test.example.ts.net.","Online":true},"CurrentTailnet":{"MagicDNSEnabled":false}}\n'
        exit 0
    fi

    if [ "$mode" = "magic-dns-missing" ]; then
        printf '{"BackendState":"Running","Self":{"DNSName":"publish-test.example.ts.net.","Online":true}}\n'
        exit 0
    fi

    printf '{"BackendState":"Running","Self":{"DNSName":"publish-test.example.ts.net.","Online":true},"CurrentTailnet":{"MagicDNSEnabled":true}}\n'
    exit 0
fi

if [ "$#" -eq 3 ] && [ "$1" = "serve" ] && [ "$2" = "status" ] && [ "$3" = "--json" ]; then
    if [ "$mode" = "serve-status-failure" ]; then
        printf 'fake provider serve diagnostic\nsecond line\n' >&2
        exit 21
    fi

    if [ "$mode" = "invalid-provider-json" ]; then
        printf '{"TCP":'
        exit 0
    fi

    if [ "$mode" = "unsupported-provider-state" ]; then
        printf '{"TCP":{"%s":{"TCPForward":"127.0.0.1:%s"}},"FutureConfig":{}}\n' "$port" "$port"
        exit 0
    fi

    if [ "$mode" = "already-satisfied" ]; then
        printf '%s\n' "$desired_with_unrelated"
        exit 0
    fi

    if [ "$mode" = "different-target" ]; then
        printf '{"TCP":{"%s":{"TCPForward":"127.0.0.1:1"}}}\n' "$port"
        exit 0
    fi

    if [ "$mode" = "tls-terminated-tcp" ]; then
        printf '{"TCP":{"%s":{"TCPForward":"127.0.0.1:%s","TerminateTLS":"publish-test.example.ts.net"}}}\n' "$port" "$port"
        exit 0
    fi

    if [ "$mode" = "http-serve" ]; then
        printf '{"TCP":{"%s":{"HTTP":true}}}\n' "$port"
        exit 0
    fi

    if [ "$mode" = "https-serve" ]; then
        printf '{"TCP":{"%s":{"HTTPS":true}}}\n' "$port"
        exit 0
    fi

    if [ "$mode" = "funnel-enabled" ]; then
        printf '{"TCP":{"%s":{"TCPForward":"127.0.0.1:%s"}},"AllowFunnel":{"publish-test.example.ts.net:%s":true}}\n' "$port" "$port" "$port"
        exit 0
    fi

    if [ "$mode" = "foreground-state" ]; then
        printf '{"Foreground":{"test-session":{"TCP":{"%s":{"TCPForward":"127.0.0.1:%s"}}}}}\n' "$port" "$port"
        exit 0
    fi

    if [ "$mode" = "proxy-protocol" ]; then
        printf '{"TCP":{"%s":{"TCPForward":"127.0.0.1:%s","ProxyProtocol":2}}}\n' "$port" "$port"
        exit 0
    fi

    if [ "$mode" = "unknown-handler-state" ]; then
        printf '{"TCP":{"%s":{"TCPForward":"127.0.0.1:%s","UnknownMode":true}}}\n' "$port" "$port"
        exit 0
    fi

    if [ -f "$AEQUIMUTA_FAKE_STATE" ]; then
        if [ "$mode" = "bad-post-state" ]; then
            printf '{"TCP":{"%s":{"HTTPS":true},"%s":{"TCPForward":"127.0.0.1:1"}},"Web":{"unrelated.example.ts.net:%s":{"Handlers":{}}},"AllowFunnel":{"unrelated.example.ts.net:%s":true}}\n' "$unrelated_port" "$port" "$unrelated_port" "$unrelated_port"
        else
            printf '%s\n' "$desired_with_unrelated"
        fi
    else
        printf '%s\n' "$unrelated"
    fi
    exit 0
fi

if [ "$#" -eq 4 ] \
    && [ "$1" = "serve" ] \
    && [ "$2" = "--bg" ] \
    && [ "$3" = "--tcp=$port" ] \
    && [ "$4" = "tcp://127.0.0.1:$port" ]; then
    if [ "$mode" = "mutation-failure" ] || [ "$mode" = "mutation-failure-desired" ]; then
        if [ "$mode" = "mutation-failure-desired" ]; then
            : > "$AEQUIMUTA_FAKE_STATE"
        fi
        printf 'fake provider mutation diagnostic\nsecond line\n' >&2
        exit 22
    fi

    : > "$AEQUIMUTA_FAKE_STATE"
    exit 0
fi

printf 'unexpected fake tailscale command: %s\n' "$*" >&2
exit 99
"#;
