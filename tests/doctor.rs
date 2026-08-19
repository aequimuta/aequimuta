#[allow(dead_code)]
mod support;

use std::fs;
use std::io::{self, ErrorKind, Read};
use std::net::TcpListener;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use support::TestDirectory;

const DECLARATION_FILE: &str = "aequimuta.toml";
const PUBLISHING_FILE: &str = "aequimuta.publish.toml";
const OPENSSH_CONFIGURATION_FILE: &str = "aequimuta.openssh-reverse-tcp.toml";
const TAILSCALE_PUBLISHER: &str = "tailscale-serve-tcp";
const OPENSSH_PUBLISHER: &str = "openssh-reverse-tcp";
const TEST_HOST: &str = "edge.example.com";
const TEST_USER: &str = "aequimuta";
const TEST_SSH_PORT: u16 = 22;
const OPENSSH_CONTROL_FILE: &str = "cm-0123456789abcdef0123456789abcdef01234567";
const FNV1A_64_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV1A_64_PRIME: u64 = 0x00000100000001b3;
const USAGE_STDERR: &[u8] = b"Usage: aequimuta <command>\n";
const OPENSSH_INFO: &str = "INFO  OpenSSH remote reachability, host-key trust, credentials, authentication, forwarding policy, and listener availability were not probed";
const GOOD_CLIENT_JSON: &str = "{\"BackendState\":\"Running\",\"Self\":{\"DNSName\":\"doctor-test.example.ts.net.\",\"Online\":true},\"CurrentTailnet\":{\"MagicDNSEnabled\":true}}";
const SECRET_MARKER: &str = "super-secret-provider-output";

static NEXT_RUNTIME_DIRECTORY: AtomicU64 = AtomicU64::new(0);

type TestResult = Result<(), Box<dyn std::error::Error>>;
type ProjectInputCase<'a> = (&'a str, Option<&'a [u8]>, Option<&'a [u8]>, &'a str);
type TailscaleFailureCase<'a> = (&'a str, &'a [(&'a str, &'a str)], usize, &'a str);

#[derive(Debug, Eq, PartialEq)]
struct TreeEntry {
    relative: PathBuf,
    device: u64,
    inode: u64,
    mode: u32,
    links: u64,
    uid: u32,
    gid: u32,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
    contents: Option<Vec<u8>>,
    link_target: Option<PathBuf>,
}

struct Project {
    directory: TestDirectory,
    snapshot: Vec<TreeEntry>,
}

impl Project {
    fn new(
        label: &str,
        declaration: Option<&[u8]>,
        publishing: Option<&[u8]>,
        openssh_configuration: Option<&[u8]>,
    ) -> io::Result<Self> {
        let directory = TestDirectory::new(label)?;

        if let Some(declaration) = declaration {
            fs::write(directory.path().join(DECLARATION_FILE), declaration)?;
        }
        if let Some(publishing) = publishing {
            fs::write(directory.path().join(PUBLISHING_FILE), publishing)?;
        }
        if let Some(configuration) = openssh_configuration {
            fs::write(
                directory.path().join(OPENSSH_CONFIGURATION_FILE),
                configuration,
            )?;
        }

        let snapshot = snapshot_tree(directory.path())?;
        Ok(Self {
            directory,
            snapshot,
        })
    }

    fn path(&self) -> &Path {
        self.directory.path()
    }

    fn assert_unchanged(&self) -> io::Result<()> {
        assert_eq!(snapshot_tree(self.path())?, self.snapshot);
        Ok(())
    }
}

struct ShortRuntimeDirectory {
    path: PathBuf,
}

impl ShortRuntimeDirectory {
    fn new() -> io::Result<Self> {
        loop {
            let sequence = NEXT_RUNTIME_DIRECTORY.fetch_add(1, Ordering::Relaxed) & 0x3ff;
            let process = u64::from(std::process::id()) & 0x3f_ffff;
            let identifier = process << 10 | sequence;
            let path = Path::new("/tmp").join(format!("ad{identifier:08x}"));

            match fs::create_dir(&path) {
                Ok(()) => {
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ShortRuntimeDirectory {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path)
            && error.kind() != ErrorKind::NotFound
        {
            eprintln!(
                "failed to clean up doctor runtime fixture {}: {error}",
                self.path.display()
            );
        }
    }
}

struct BackendListener {
    listener: TcpListener,
}

impl BackendListener {
    fn new() -> io::Result<Self> {
        Ok(Self {
            listener: TcpListener::bind(("127.0.0.1", 0))?,
        })
    }

    fn port(&self) -> io::Result<u16> {
        Ok(self.listener.local_addr()?.port())
    }

    fn accepted_payloads(&self) -> io::Result<Vec<Vec<u8>>> {
        self.listener.set_nonblocking(true)?;
        let mut payloads = Vec::new();

        loop {
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
                    let mut payload = Vec::new();
                    stream.read_to_end(&mut payload)?;
                    payloads.push(payload);
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(payloads),
                Err(error) => return Err(error),
            }
        }
    }
}

struct FakeProviders {
    directory: TestDirectory,
    tailscale_log: PathBuf,
    ssh_log: PathBuf,
    tailscale_state: PathBuf,
    ssh_state: PathBuf,
    initial_tailscale_state: Vec<TreeEntry>,
    initial_ssh_state: Vec<TreeEntry>,
}

impl FakeProviders {
    fn new(label: &str) -> io::Result<Self> {
        let directory = TestDirectory::new(label)?;
        let tailscale = directory.path().join("tailscale");
        let ssh = directory.path().join("ssh");
        let tailscale_log = directory.path().join("tailscale.log");
        let ssh_log = directory.path().join("ssh.log");
        let tailscale_state = directory.path().join("tailscale.state");
        let ssh_state = directory.path().join("ssh.state");

        fs::write(&tailscale, FAKE_TAILSCALE)?;
        fs::set_permissions(&tailscale, fs::Permissions::from_mode(0o700))?;
        fs::write(&ssh, FAKE_SSH)?;
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700))?;
        fs::write(&tailscale_log, b"")?;
        fs::write(&ssh_log, b"")?;
        fs::write(&tailscale_state, b"tailscale-provider-state\n")?;
        fs::write(&ssh_state, b"openssh-provider-state\n")?;

        let initial_tailscale_state = snapshot_path(&tailscale_state)?;
        let initial_ssh_state = snapshot_path(&ssh_state)?;

        Ok(Self {
            directory,
            tailscale_log,
            ssh_log,
            tailscale_state,
            ssh_state,
            initial_tailscale_state,
            initial_ssh_state,
        })
    }

    fn path(&self) -> &Path {
        self.directory.path()
    }

    fn run(
        &self,
        project: &Project,
        runtime: &ShortRuntimeDirectory,
        args: &[&str],
        overrides: &[(&str, &str)],
    ) -> io::Result<Output> {
        let reported_control_path = provider_runtime_directory(runtime.path())
            .join(OPENSSH_CONTROL_FILE)
            .into_os_string();
        let mut command = Command::new(env!("CARGO_BIN_EXE_aequimuta"));
        command
            .args(args)
            .current_dir(project.path())
            .env("PATH", self.path())
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env("AEQUIMUTA_DOCTOR_TAILSCALE_LOG", &self.tailscale_log)
            .env("AEQUIMUTA_DOCTOR_SSH_LOG", &self.ssh_log)
            .env("AEQUIMUTA_DOCTOR_CLIENT_MODE", "success")
            .env("AEQUIMUTA_DOCTOR_CLIENT_JSON", GOOD_CLIENT_JSON)
            .env("AEQUIMUTA_DOCTOR_SERVE_MODE", "success")
            .env("AEQUIMUTA_DOCTOR_SERVE_JSON", "{}")
            .env("AEQUIMUTA_DOCTOR_SSH_G_MODE", "success")
            .env("AEQUIMUTA_DOCTOR_SSH_CHECK_MODE", "live")
            .env("AEQUIMUTA_DOCTOR_REPORTED_CONTROL", reported_control_path);

        for (key, value) in overrides {
            command.env(key, value);
        }

        command.output()
    }

    fn tailscale_calls(&self) -> io::Result<Vec<String>> {
        Ok(fs::read_to_string(&self.tailscale_log)?
            .lines()
            .map(str::to_owned)
            .collect())
    }

    fn ssh_calls(&self) -> io::Result<Vec<Vec<String>>> {
        parse_argv_log(&self.ssh_log)
    }

    fn assert_state_unchanged(&self) -> io::Result<()> {
        assert_eq!(
            snapshot_path(&self.tailscale_state)?,
            self.initial_tailscale_state
        );
        assert_eq!(snapshot_path(&self.ssh_state)?, self.initial_ssh_state);
        Ok(())
    }
}

fn snapshot_tree(root: &Path) -> io::Result<Vec<TreeEntry>> {
    let mut entries = Vec::new();
    snapshot_tree_at(root, root, &mut entries)?;
    Ok(entries)
}

fn snapshot_path(path: &Path) -> io::Result<Vec<TreeEntry>> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("snapshot path has no parent"))?;
    let mut entries = Vec::new();
    snapshot_tree_at(parent, path, &mut entries)?;
    Ok(entries)
}

fn snapshot_tree_at(root: &Path, path: &Path, entries: &mut Vec<TreeEntry>) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    let relative = path.strip_prefix(root).unwrap_or(path).to_owned();
    let contents = if metadata.file_type().is_file() {
        Some(fs::read(path)?)
    } else {
        None
    };
    let link_target = if metadata.file_type().is_symlink() {
        Some(fs::read_link(path)?)
    } else {
        None
    };

    entries.push(TreeEntry {
        relative,
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
        links: metadata.nlink(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        size: metadata.size(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
        contents,
        link_target,
    });

    if metadata.file_type().is_dir() {
        let mut children = fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()?;
        children.sort();

        for child in children {
            snapshot_tree_at(root, &child, entries)?;
        }
    }

    Ok(())
}

fn parse_argv_log(path: &Path) -> io::Result<Vec<Vec<String>>> {
    let source = fs::read_to_string(path)?;
    let mut calls = Vec::new();
    let mut current: Option<Vec<String>> = None;

    for line in source.lines() {
        match line {
            "BEGIN" if current.is_none() => current = Some(Vec::new()),
            "END" => {
                let call = current.take().ok_or_else(|| {
                    io::Error::new(ErrorKind::InvalidData, "orphan argv terminator")
                })?;
                calls.push(call);
            }
            argument => current
                .as_mut()
                .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "orphan argv argument"))?
                .push(argument.to_owned()),
        }
    }

    if current.is_some() {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "unterminated argv call",
        ));
    }

    Ok(calls)
}

fn run_with_path(
    project: &Project,
    runtime: &ShortRuntimeDirectory,
    args: &[&str],
    path: &Path,
) -> io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_aequimuta"))
        .args(args)
        .current_dir(project.path())
        .env("PATH", path)
        .env("XDG_RUNTIME_DIR", runtime.path())
        .output()
}

fn service_declaration(services: &[(&str, u16)]) -> Vec<u8> {
    let mut source = String::new();
    for (name, port) in services {
        source.push_str(&format!(
            "[[services]]\nname = \"{name}\"\nport = {port}\n\n"
        ));
    }
    source.into_bytes()
}

fn publishing_intent(publications: &[(&str, &str)]) -> Vec<u8> {
    let mut source = String::new();
    for (service, publisher) in publications {
        source.push_str(&format!(
            "[[publications]]\nservice = \"{service}\"\npublisher = \"{publisher}\"\n\n"
        ));
    }
    source.into_bytes()
}

fn openssh_configuration(publications: &[(&str, &str, u16)]) -> Vec<u8> {
    let mut source = String::new();
    for (service, host, listen_port) in publications {
        source.push_str(&format!(
            "[[publications]]\n\
             service = \"{service}\"\n\
             host = \"{host}\"\n\
             user = \"{TEST_USER}\"\n\
             ssh_port = {TEST_SSH_PORT}\n\
             listen_address = \"0.0.0.0\"\n\
             listen_port = {listen_port}\n\n"
        ));
    }
    source.into_bytes()
}

fn provider_runtime_directory(runtime: &Path) -> PathBuf {
    runtime.join("aequimuta").join("openssh-reverse-tcp")
}

fn prepare_provider_runtime_directory(runtime: &Path) -> io::Result<PathBuf> {
    let aequimuta = runtime.join("aequimuta");
    fs::create_dir(&aequimuta)?;
    fs::set_permissions(&aequimuta, fs::Permissions::from_mode(0o700))?;
    let provider = aequimuta.join("openssh-reverse-tcp");
    fs::create_dir(&provider)?;
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o700))?;
    Ok(provider)
}

fn expected_control_path(runtime: &Path, host: &str) -> PathBuf {
    let mut hash = FNV1A_64_OFFSET_BASIS;

    for byte in host
        .as_bytes()
        .iter()
        .copied()
        .chain(std::iter::once(0))
        .chain(TEST_USER.as_bytes().iter().copied())
        .chain(std::iter::once(0))
        .chain(TEST_SSH_PORT.to_be_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV1A_64_PRIME);
    }

    let file_name = format!("cm-{hash:016x}{}", &OPENSSH_CONTROL_FILE[19..]);
    provider_runtime_directory(runtime).join(file_name)
}

fn closed_local_port() -> io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

fn output_stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn assert_usage_failure(output: &Output) {
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, USAGE_STDERR);
}

fn assert_diagnostic_result(output: &Output, expected_code: i32, expected_failures: usize) {
    assert_eq!(
        output.status.code(),
        Some(expected_code),
        "unexpected diagnostic status; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = output_stdout(output);
    let actual_failures = stdout
        .lines()
        .filter(|line| line.starts_with("FAIL  "))
        .count();
    assert_eq!(actual_failures, expected_failures, "{stdout}");

    if expected_failures == 0 {
        assert!(stdout.ends_with("No blocking readiness issues detected by performed checks\n"));
    } else {
        assert!(stdout.ends_with(&format!(
            "Found {expected_failures} blocking readiness issues\n"
        )));
    }
}

fn assert_no_secret_leak(output: &Output) {
    assert!(!output_stdout(output).contains(SECRET_MARKER));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(SECRET_MARKER));
}

fn call_has_pair(call: &[String], first: &str, second: &str) -> bool {
    call.windows(2)
        .any(|arguments| arguments[0] == first && arguments[1] == second)
}

fn is_ssh_expansion(call: &[String]) -> bool {
    call.iter().any(|argument| argument == "-G")
}

fn is_ssh_check(call: &[String]) -> bool {
    call_has_pair(call, "-O", "check")
}

fn assert_only_non_mutating_provider_calls(fake: &FakeProviders) -> io::Result<()> {
    let tailscale_calls = fake.tailscale_calls()?;
    assert!(
        tailscale_calls
            .iter()
            .all(|call| { call == "status --json" || call == "serve status --json" })
    );
    assert!(tailscale_calls.iter().all(|call| {
        !call.contains("--bg")
            && !call.contains("off")
            && !call.contains("reset")
            && !call.to_ascii_lowercase().contains("funnel")
    }));

    let ssh_calls = fake.ssh_calls()?;
    assert!(
        ssh_calls
            .iter()
            .all(|call| is_ssh_expansion(call) || is_ssh_check(call)),
        "unexpected SSH invocation: {ssh_calls:?}"
    );
    assert!(ssh_calls.iter().all(|call| {
        !call
            .iter()
            .any(|argument| matches!(argument.as_str(), "-M" | "-N" | "-R" | "-T"))
            && !call.iter().any(|argument| {
                (argument.starts_with("ControlMaster=") && argument != "ControlMaster=no")
                    || argument.starts_with("RemoteCommand=")
            })
            && !call_has_pair(call, "-O", "forward")
            && !call_has_pair(call, "-O", "cancel")
            && !call_has_pair(call, "-O", "exit")
    }));
    assert!(ssh_calls.iter().all(|call| {
        call.last().map(String::as_str) == Some(TEST_HOST)
            || call.last().map(String::as_str) == Some("edge-two.example.com")
    }));
    Ok(())
}

fn assert_open_ssh_info_once(output: &Output) {
    assert_eq!(
        output_stdout(output)
            .lines()
            .filter(|line| *line == OPENSSH_INFO)
            .count(),
        1
    );
}

fn assert_section_order(stdout: &str, sections: &[&str]) {
    let mut previous = 0;
    for (index, section) in sections.iter().enumerate() {
        let position = stdout
            .find(section)
            .unwrap_or_else(|| panic!("missing section {section:?}: {stdout}"));
        if index != 0 {
            assert!(position > previous, "sections out of order: {stdout}");
        }
        previous = position;
    }
}

const FAKE_TAILSCALE: &[u8] = br#"#!/bin/sh
set -eu

printf '%s\n' "$*" >> "$AEQUIMUTA_DOCTOR_TAILSCALE_LOG"

if [ "$#" -eq 2 ] && [ "$1" = "status" ] && [ "$2" = "--json" ]; then
    if [ "$AEQUIMUTA_DOCTOR_CLIENT_MODE" = "nonzero" ]; then
        printf 'super-secret-provider-output client\n/private/client-state\n' >&2
        exit 23
    fi
    printf '%s\n' "$AEQUIMUTA_DOCTOR_CLIENT_JSON"
    exit 0
fi

if [ "$#" -eq 3 ] && [ "$1" = "serve" ] && [ "$2" = "status" ] && [ "$3" = "--json" ]; then
    if [ "$AEQUIMUTA_DOCTOR_SERVE_MODE" = "nonzero" ]; then
        printf 'super-secret-provider-output serve\n/private/serve-state\n' >&2
        exit 24
    fi
    printf '%s\n' "$AEQUIMUTA_DOCTOR_SERVE_JSON"
    exit 0
fi

printf 'super-secret-provider-output unexpected tailscale mutation: %s\n' "$*" >&2
exit 99
"#;

const FAKE_SSH: &[u8] = br#"#!/bin/sh
set -eu

{
    printf 'BEGIN\n'
    for argument do
        printf '%s\n' "$argument"
    done
    printf 'END\n'
} >> "$AEQUIMUTA_DOCTOR_SSH_LOG"

case " $* " in
    *" -G "*)
        if [ "$AEQUIMUTA_DOCTOR_SSH_G_MODE" = "nonzero" ]; then
            printf 'super-secret-provider-output ssh-G\n/private/ssh-config\n' >&2
            exit 25
        fi
        if [ "$AEQUIMUTA_DOCTOR_SSH_G_MODE" = "malformed" ]; then
            printf 'hostname malformed.example.com\n'
            exit 0
        fi
        printf 'controlpath %s\n' "$AEQUIMUTA_DOCTOR_REPORTED_CONTROL"
        exit 0
        ;;
    *" -O check "*)
        if [ "$AEQUIMUTA_DOCTOR_SSH_CHECK_MODE" = "stale" ]; then
            printf 'super-secret-provider-output stale socket\n/private/control-state\n' >&2
            exit 255
        fi
        printf 'Master running (pid=1234)\r\n' >&2
        exit 0
        ;;
esac

printf 'super-secret-provider-output unexpected ssh mutation or remote command\n' >&2
exit 99
"#;

#[test]
fn doctor_rejects_extra_arguments_before_any_observation() -> TestResult {
    let invalid_project = Project::new(
        "doctor-usage-invalid",
        Some(&[0xff]),
        Some(&[0xff]),
        Some(&[0xff]),
    )?;
    let runtime = ShortRuntimeDirectory::new()?;
    let runtime_before = snapshot_tree(runtime.path())?;
    let fake = FakeProviders::new("doctor-usage-fake")?;

    for args in [
        &["doctor", "web"][..],
        &["doctor", "web", TAILSCALE_PUBLISHER][..],
        &["doctor", "--remote"][..],
    ] {
        let output = fake.run(&invalid_project, &runtime, args, &[])?;
        assert_usage_failure(&output);
    }

    assert!(fake.tailscale_calls()?.is_empty());
    assert!(fake.ssh_calls()?.is_empty());
    assert_eq!(snapshot_tree(runtime.path())?, runtime_before);
    invalid_project.assert_unchanged()?;

    let backend = BackendListener::new()?;
    let port = backend.port()?;
    let declaration = service_declaration(&[("web", port)]);
    let publishing = publishing_intent(&[("web", TAILSCALE_PUBLISHER)]);
    let valid_project = Project::new(
        "doctor-usage-backend",
        Some(&declaration),
        Some(&publishing),
        None,
    )?;
    let output = fake.run(&valid_project, &runtime, &["doctor", "unexpected"], &[])?;

    assert_usage_failure(&output);
    assert!(backend.accepted_payloads()?.is_empty());
    assert!(fake.tailscale_calls()?.is_empty());
    assert!(fake.ssh_calls()?.is_empty());
    valid_project.assert_unchanged()?;
    fake.assert_state_unchanged()?;
    Ok(())
}

#[test]
fn doctor_reports_project_input_failures_and_empty_state() -> TestResult {
    let runtime = ShortRuntimeDirectory::new()?;
    let empty_path = TestDirectory::new("doctor-input-empty-path")?;
    let cases: &[ProjectInputCase<'_>] = &[
        (
            "missing-core",
            None,
            Some(b"[[publications]\n"),
            "FAIL  aequimuta.toml: failed to read aequimuta.toml",
        ),
        (
            "invalid-core",
            Some(b"[[services]\n"),
            Some(b"# valid publishing intent\n"),
            "FAIL  aequimuta.toml: aequimuta.toml is not a valid declaration",
        ),
        (
            "missing-publishing",
            Some(b"# valid declaration\n"),
            None,
            "FAIL  aequimuta.publish.toml: failed to read aequimuta.publish.toml",
        ),
        (
            "invalid-publishing",
            Some(b"# valid declaration\n"),
            Some(b"[[publications]\n"),
            "FAIL  aequimuta.publish.toml: aequimuta.publish.toml is not a valid publishing intent",
        ),
    ];

    for &(label, declaration, publishing, expected_failure) in cases {
        let project = Project::new(label, declaration, publishing, Some(&[0xff]))?;
        let output = run_with_path(&project, &runtime, &["doctor"], empty_path.path())?;

        assert_diagnostic_result(&output, 1, 1);
        let stdout = output_stdout(&output);
        assert!(stdout.contains(expected_failure), "{label}: {stdout}");
        assert!(!stdout.contains("Local backends\n"));
        assert!(!stdout.contains("Tailscale\n"));
        assert!(!stdout.contains("OpenSSH\n"));
        project.assert_unchanged()?;
    }

    let backend = BackendListener::new()?;
    let declaration = service_declaration(&[("web", backend.port()?)]);
    let empty_project = Project::new(
        "doctor-empty",
        Some(&declaration),
        Some(b"# no desired publications\n"),
        Some(b"not valid TOML = [\nprivate_key = \"must-not-be-read\"\n"),
    )?;
    let fake = FakeProviders::new("doctor-empty-fake")?;
    fs::write(runtime.path().join("aequimuta"), b"unsafe but unrelated\n")?;
    let runtime_before = snapshot_tree(runtime.path())?;
    let output = fake.run(&empty_project, &runtime, &["doctor"], &[])?;

    assert_diagnostic_result(&output, 0, 0);
    assert_eq!(
        output.stdout,
        b"Project\n\
          PASS  aequimuta.toml\n\
          PASS  aequimuta.publish.toml\n\
          INFO  No desired publications to check\n\
          No blocking readiness issues detected by performed checks\n"
    );
    assert!(backend.accepted_payloads()?.is_empty());
    assert!(fake.tailscale_calls()?.is_empty());
    assert!(fake.ssh_calls()?.is_empty());
    assert_eq!(snapshot_tree(runtime.path())?, runtime_before);
    empty_project.assert_unchanged()?;
    fake.assert_state_unchanged()?;
    Ok(())
}

#[test]
fn doctor_reports_applicability_and_provider_configuration_failures() -> TestResult {
    let unsupported_backend = BackendListener::new()?;
    let unsupported_port = unsupported_backend.port()?;
    let unsupported_declaration = service_declaration(&[("web", unsupported_port)]);
    let unsupported_publishing = publishing_intent(&[("web", "example-publisher")]);
    let unsupported_project = Project::new(
        "doctor-unsupported",
        Some(&unsupported_declaration),
        Some(&unsupported_publishing),
        None,
    )?;
    let unsupported_runtime = ShortRuntimeDirectory::new()?;
    let unsupported_fake = FakeProviders::new("doctor-unsupported-fake")?;
    let output =
        unsupported_fake.run(&unsupported_project, &unsupported_runtime, &["doctor"], &[])?;

    assert_diagnostic_result(&output, 1, 1);
    assert!(output_stdout(&output).contains(
        "FAIL  Operational publisher support: unsupported desired publisher tokens: example-publisher"
    ));
    assert_eq!(unsupported_backend.accepted_payloads()?, [Vec::<u8>::new()]);
    assert!(unsupported_fake.tailscale_calls()?.is_empty());
    assert!(unsupported_fake.ssh_calls()?.is_empty());
    unsupported_project.assert_unchanged()?;

    let shared_backend = BackendListener::new()?;
    let shared_port = shared_backend.port()?;
    let ambiguous_declaration = service_declaration(&[("web", shared_port), ("api", shared_port)]);
    let ambiguous_publishing =
        publishing_intent(&[("web", TAILSCALE_PUBLISHER), ("api", TAILSCALE_PUBLISHER)]);
    let ambiguous_project = Project::new(
        "doctor-tailscale-ambiguity",
        Some(&ambiguous_declaration),
        Some(&ambiguous_publishing),
        None,
    )?;
    let ambiguous_runtime = ShortRuntimeDirectory::new()?;
    let ambiguous_fake = FakeProviders::new("doctor-tailscale-ambiguity-fake")?;
    let output = ambiguous_fake.run(&ambiguous_project, &ambiguous_runtime, &["doctor"], &[])?;

    assert_diagnostic_result(&output, 1, 1);
    assert!(output_stdout(&output).contains(
        "FAIL  Tailscale desired-slot rules: multiple publications target the same current-node TCP port"
    ));
    assert_eq!(shared_backend.accepted_payloads()?, [Vec::<u8>::new()]);
    assert!(ambiguous_fake.tailscale_calls()?.is_empty());
    ambiguous_project.assert_unchanged()?;

    let conditional_backend = BackendListener::new()?;
    let conditional_port = conditional_backend.port()?;
    let conditional_declaration = service_declaration(&[("web", conditional_port)]);
    let conditional_publishing = publishing_intent(&[("web", TAILSCALE_PUBLISHER)]);
    let conditional_project = Project::new(
        "doctor-conditional-openssh",
        Some(&conditional_declaration),
        Some(&conditional_publishing),
        Some(b"not valid TOML = [\nprivate_key = \"must-not-be-read\"\n"),
    )?;
    let conditional_runtime = ShortRuntimeDirectory::new()?;
    let conditional_fake = FakeProviders::new("doctor-conditional-openssh-fake")?;
    let output =
        conditional_fake.run(&conditional_project, &conditional_runtime, &["doctor"], &[])?;

    assert_diagnostic_result(&output, 0, 0);
    assert!(!output_stdout(&output).contains("OpenSSH\n"));
    assert_eq!(
        conditional_fake.tailscale_calls()?,
        ["status --json", "serve status --json"]
    );
    assert!(conditional_fake.ssh_calls()?.is_empty());
    conditional_project.assert_unchanged()?;

    let configuration_cases: &[(&str, Option<&[u8]>, &str)] = &[
        (
            "missing-openssh",
            None,
            "failed to read aequimuta.openssh-reverse-tcp.toml",
        ),
        (
            "invalid-openssh",
            Some(b"future = true\n"),
            "aequimuta.openssh-reverse-tcp.toml is not a valid OpenSSH reverse TCP configuration",
        ),
        (
            "unresolved-openssh",
            Some(b"# no provider publications\n"),
            "desired OpenSSH reverse TCP publication has no provider configuration",
        ),
    ];

    for &(label, configuration, detail) in configuration_cases {
        let backend = BackendListener::new()?;
        let declaration = service_declaration(&[("web", backend.port()?)]);
        let publishing = publishing_intent(&[("web", OPENSSH_PUBLISHER)]);
        let project = Project::new(label, Some(&declaration), Some(&publishing), configuration)?;
        let runtime = ShortRuntimeDirectory::new()?;
        let fake = FakeProviders::new(&format!("{label}-fake"))?;
        let output = fake.run(&project, &runtime, &["doctor"], &[])?;

        assert_diagnostic_result(&output, 1, 1);
        let stdout = output_stdout(&output);
        assert!(
            stdout.contains(&format!(
                "FAIL  Provider configuration and desired-slot rules: {detail}"
            )),
            "{label}: {stdout}"
        );
        assert_open_ssh_info_once(&output);
        assert!(fake.ssh_calls()?.is_empty());
        assert!(fake.tailscale_calls()?.is_empty());
        project.assert_unchanged()?;
        fake.assert_state_unchanged()?;
    }

    let web_backend = BackendListener::new()?;
    let api_backend = BackendListener::new()?;
    let ambiguity_declaration =
        service_declaration(&[("web", web_backend.port()?), ("api", api_backend.port()?)]);
    let ambiguity_publishing =
        publishing_intent(&[("web", OPENSSH_PUBLISHER), ("api", OPENSSH_PUBLISHER)]);
    let ambiguity_configuration =
        openssh_configuration(&[("web", TEST_HOST, 18_080), ("api", TEST_HOST, 18_080)]);
    let project = Project::new(
        "doctor-openssh-slot-ambiguity",
        Some(&ambiguity_declaration),
        Some(&ambiguity_publishing),
        Some(&ambiguity_configuration),
    )?;
    let runtime = ShortRuntimeDirectory::new()?;
    let fake = FakeProviders::new("doctor-openssh-slot-ambiguity-fake")?;
    let output = fake.run(&project, &runtime, &["doctor"], &[])?;

    assert_diagnostic_result(&output, 1, 1);
    assert!(output_stdout(&output).contains(
        "FAIL  Provider configuration and desired-slot rules: desired OpenSSH reverse TCP publications conflict"
    ));
    assert_open_ssh_info_once(&output);
    assert!(fake.ssh_calls()?.is_empty());
    project.assert_unchanged()?;
    Ok(())
}

#[test]
fn doctor_checks_unique_backends_once_without_protocol_payload() -> TestResult {
    let backend = BackendListener::new()?;
    let port = backend.port()?;
    let declaration = service_declaration(&[("web", port), ("api", port)]);
    let publishing = publishing_intent(&[
        ("web", TAILSCALE_PUBLISHER),
        ("web", OPENSSH_PUBLISHER),
        ("api", OPENSSH_PUBLISHER),
    ]);
    let configuration = openssh_configuration(&[
        ("web", TEST_HOST, 18_080),
        ("api", "edge-two.example.com", 18_081),
    ]);
    let project = Project::new(
        "doctor-backend-deduplication",
        Some(&declaration),
        Some(&publishing),
        Some(&configuration),
    )?;
    let runtime = ShortRuntimeDirectory::new()?;
    let runtime_before = snapshot_tree(runtime.path())?;
    let fake = FakeProviders::new("doctor-backend-deduplication-fake")?;
    let output = fake.run(&project, &runtime, &["doctor"], &[])?;

    assert_diagnostic_result(&output, 0, 0);
    let payloads = backend.accepted_payloads()?;
    assert_eq!(payloads, [Vec::<u8>::new()]);
    assert_eq!(
        output_stdout(&output)
            .lines()
            .filter(|line| line.contains(&format!("127.0.0.1:{port}")))
            .count(),
        1
    );
    assert_eq!(snapshot_tree(runtime.path())?, runtime_before);
    assert_only_non_mutating_provider_calls(&fake)?;
    project.assert_unchanged()?;
    fake.assert_state_unchanged()?;

    let unreachable_port = closed_local_port()?;
    let declaration = service_declaration(&[("broken", unreachable_port)]);
    let publishing = publishing_intent(&[("broken", TAILSCALE_PUBLISHER)]);
    let project = Project::new(
        "doctor-backend-unreachable",
        Some(&declaration),
        Some(&publishing),
        None,
    )?;
    let runtime = ShortRuntimeDirectory::new()?;
    let fake = FakeProviders::new("doctor-backend-unreachable-fake")?;
    let output = fake.run(&project, &runtime, &["doctor"], &[])?;

    assert_diagnostic_result(&output, 1, 1);
    assert!(output_stdout(&output).contains(&format!(
        "FAIL  broken 127.0.0.1:{unreachable_port} is not reachable"
    )));
    assert_eq!(
        fake.tailscale_calls()?,
        ["status --json", "serve status --json"]
    );
    assert_only_non_mutating_provider_calls(&fake)?;
    project.assert_unchanged()?;
    Ok(())
}

#[test]
fn doctor_fails_closed_on_tailscale_query_and_client_errors() -> TestResult {
    let missing_backend = BackendListener::new()?;
    let missing_port = missing_backend.port()?;
    let missing_declaration = service_declaration(&[("web", missing_port)]);
    let missing_publishing = publishing_intent(&[("web", TAILSCALE_PUBLISHER)]);
    let missing_project = Project::new(
        "doctor-tailscale-missing",
        Some(&missing_declaration),
        Some(&missing_publishing),
        None,
    )?;
    let runtime = ShortRuntimeDirectory::new()?;
    let empty_path = TestDirectory::new("doctor-tailscale-missing-path")?;
    let output = run_with_path(&missing_project, &runtime, &["doctor"], empty_path.path())?;

    assert_diagnostic_result(&output, 1, 1);
    assert!(
        output_stdout(&output).contains(
            "FAIL  Tailscale client prerequisites: tailscale executable is not available"
        )
    );
    missing_project.assert_unchanged()?;

    let cases: &[TailscaleFailureCase<'_>] = &[
        (
            "client-nonzero",
            &[("AEQUIMUTA_DOCTOR_CLIENT_MODE", "nonzero")],
            1,
            "FAIL  Tailscale client prerequisites: Tailscale daemon or client is not operational",
        ),
        (
            "client-malformed",
            &[("AEQUIMUTA_DOCTOR_CLIENT_JSON", "{\"BackendState\":")],
            1,
            "FAIL  Tailscale client prerequisites: Tailscale client state is indeterminate",
        ),
        (
            "client-prerequisite",
            &[(
                "AEQUIMUTA_DOCTOR_CLIENT_JSON",
                "{\"BackendState\":\"Stopped\",\"Self\":{\"DNSName\":\"doctor-test.example.ts.net.\",\"Online\":false},\"CurrentTailnet\":{\"MagicDNSEnabled\":true}}",
            )],
            1,
            "FAIL  Tailscale client prerequisites: Tailscale daemon or client is not operational",
        ),
        (
            "serve-nonzero",
            &[("AEQUIMUTA_DOCTOR_SERVE_MODE", "nonzero")],
            2,
            "FAIL  Serve state observation: failed to inspect Tailscale Serve state",
        ),
        (
            "serve-malformed",
            &[("AEQUIMUTA_DOCTOR_SERVE_JSON", "{\"TCP\":")],
            2,
            "FAIL  Serve state observation: failed to inspect Tailscale Serve state",
        ),
    ];

    for &(label, overrides, expected_calls, expected_failure) in cases {
        let backend = BackendListener::new()?;
        let declaration = service_declaration(&[("web", backend.port()?)]);
        let publishing = publishing_intent(&[("web", TAILSCALE_PUBLISHER)]);
        let project = Project::new(label, Some(&declaration), Some(&publishing), None)?;
        let runtime = ShortRuntimeDirectory::new()?;
        let fake = FakeProviders::new(&format!("{label}-fake"))?;
        let output = fake.run(&project, &runtime, &["doctor"], overrides)?;

        assert_diagnostic_result(&output, 1, 1);
        let stdout = output_stdout(&output);
        assert!(stdout.contains(expected_failure), "{label}: {stdout}");
        assert_eq!(
            fake.tailscale_calls()?.len(),
            expected_calls,
            "{label}: unexpected query count"
        );
        assert!(fake.ssh_calls()?.is_empty());
        assert_no_secret_leak(&output);
        assert_only_non_mutating_provider_calls(&fake)?;
        project.assert_unchanged()?;
        fake.assert_state_unchanged()?;
    }

    Ok(())
}

#[test]
fn doctor_projects_tailscale_relations_and_queries_once() -> TestResult {
    let cases = [
        (
            "absent",
            "{}".to_owned(),
            0,
            "PASS  web Serve slot permits apply",
        ),
        (
            "satisfied",
            String::new(),
            0,
            "PASS  web Serve slot permits apply",
        ),
        (
            "conflict",
            String::new(),
            1,
            "FAIL  web Serve slot is blocked by incompatible existing state",
        ),
        (
            "indeterminate",
            String::new(),
            1,
            "FAIL  web Serve slot cannot be classified safely",
        ),
    ];

    for (label, configured_json, expected_failures, expected_row) in cases {
        let backend = BackendListener::new()?;
        let port = backend.port()?;
        let serve_json = match label {
            "satisfied" => {
                format!(r#"{{"TCP":{{"{port}":{{"TCPForward":"127.0.0.1:{port}"}}}}}}"#)
            }
            "conflict" => format!(r#"{{"TCP":{{"{port}":{{"HTTPS":true}}}}}}"#),
            "indeterminate" => {
                format!(r#"{{"TCP":{{"0{port}":{{"TCPForward":"127.0.0.1:{port}"}}}}}}"#)
            }
            _ => configured_json,
        };
        let declaration = service_declaration(&[("web", port)]);
        let publishing = publishing_intent(&[("web", TAILSCALE_PUBLISHER)]);
        let project = Project::new(label, Some(&declaration), Some(&publishing), None)?;
        let runtime = ShortRuntimeDirectory::new()?;
        let fake = FakeProviders::new(&format!("doctor-tailscale-{label}-fake"))?;
        let output = fake.run(
            &project,
            &runtime,
            &["doctor"],
            &[("AEQUIMUTA_DOCTOR_SERVE_JSON", &serve_json)],
        )?;

        let expected_code = if expected_failures == 0 { 0 } else { 1 };
        assert_diagnostic_result(&output, expected_code, expected_failures);
        let stdout = output_stdout(&output);
        assert!(stdout.contains(expected_row), "{label}: {stdout}");
        assert!(!stdout.contains("Publication status for"));
        assert_eq!(
            fake.tailscale_calls()?,
            ["status --json", "serve status --json"]
        );
        assert!(fake.ssh_calls()?.is_empty());
        assert_only_non_mutating_provider_calls(&fake)?;
        project.assert_unchanged()?;
        fake.assert_state_unchanged()?;
    }

    let web_backend = BackendListener::new()?;
    let api_backend = BackendListener::new()?;
    let web_port = web_backend.port()?;
    let api_port = api_backend.port()?;
    let declaration = service_declaration(&[("web", web_port), ("api", api_port)]);
    let publishing =
        publishing_intent(&[("web", TAILSCALE_PUBLISHER), ("api", TAILSCALE_PUBLISHER)]);
    let serve_json = format!(
        r#"{{"TCP":{{"{web_port}":{{"TCPForward":"127.0.0.1:{web_port}"}},"{api_port}":{{"TCPForward":"127.0.0.1:{api_port}"}}}}}}"#
    );
    let project = Project::new(
        "doctor-tailscale-query-once",
        Some(&declaration),
        Some(&publishing),
        None,
    )?;
    let runtime = ShortRuntimeDirectory::new()?;
    let fake = FakeProviders::new("doctor-tailscale-query-once-fake")?;
    let output = fake.run(
        &project,
        &runtime,
        &["doctor"],
        &[("AEQUIMUTA_DOCTOR_SERVE_JSON", &serve_json)],
    )?;

    assert_diagnostic_result(&output, 0, 0);
    let stdout = output_stdout(&output);
    assert!(stdout.contains("PASS  web Serve slot permits apply"));
    assert!(stdout.contains("PASS  api Serve slot permits apply"));
    assert_eq!(
        fake.tailscale_calls()?,
        ["status --json", "serve status --json"]
    );
    assert_only_non_mutating_provider_calls(&fake)?;
    fake.assert_state_unchanged()?;
    project.assert_unchanged()?;
    Ok(())
}

#[test]
fn doctor_reports_ssh_availability_and_expansion_failures() -> TestResult {
    let missing_backend = BackendListener::new()?;
    let missing_declaration = service_declaration(&[("web", missing_backend.port()?)]);
    let missing_publishing = publishing_intent(&[("web", OPENSSH_PUBLISHER)]);
    let missing_configuration = openssh_configuration(&[("web", TEST_HOST, 18_080)]);
    let missing_project = Project::new(
        "doctor-ssh-missing",
        Some(&missing_declaration),
        Some(&missing_publishing),
        Some(&missing_configuration),
    )?;
    let missing_runtime = ShortRuntimeDirectory::new()?;
    let missing_runtime_before = snapshot_tree(missing_runtime.path())?;
    let empty_path = TestDirectory::new("doctor-ssh-missing-path")?;
    let output = run_with_path(
        &missing_project,
        &missing_runtime,
        &["doctor"],
        empty_path.path(),
    )?;

    assert_diagnostic_result(&output, 1, 1);
    assert!(output_stdout(&output).contains(
        "FAIL  web ssh executable and control-path resolution: ssh executable is not available"
    ));
    assert_open_ssh_info_once(&output);
    assert_eq!(
        snapshot_tree(missing_runtime.path())?,
        missing_runtime_before
    );
    missing_project.assert_unchanged()?;

    for (label, mode) in [
        ("ssh-g-nonzero", "nonzero"),
        ("ssh-g-malformed", "malformed"),
    ] {
        let backend = BackendListener::new()?;
        let declaration = service_declaration(&[("web", backend.port()?)]);
        let publishing = publishing_intent(&[("web", OPENSSH_PUBLISHER)]);
        let configuration = openssh_configuration(&[("web", TEST_HOST, 18_080)]);
        let project = Project::new(
            label,
            Some(&declaration),
            Some(&publishing),
            Some(&configuration),
        )?;
        let runtime = ShortRuntimeDirectory::new()?;
        let runtime_before = snapshot_tree(runtime.path())?;
        let fake = FakeProviders::new(&format!("doctor-{label}-fake"))?;
        let output = fake.run(
            &project,
            &runtime,
            &["doctor"],
            &[("AEQUIMUTA_DOCTOR_SSH_G_MODE", mode)],
        )?;

        assert_diagnostic_result(&output, 1, 1);
        let stdout = output_stdout(&output);
        assert!(stdout.contains(
            "FAIL  web ssh executable and control-path resolution: failed to resolve a safe OpenSSH control path"
        ));
        assert!(!stdout.contains(runtime.path().to_string_lossy().as_ref()));
        assert_open_ssh_info_once(&output);
        assert_no_secret_leak(&output);
        let calls = fake.ssh_calls()?;
        assert_eq!(calls.len(), 1);
        assert!(is_ssh_expansion(&calls[0]));
        assert!(!is_ssh_check(&calls[0]));
        assert_only_non_mutating_provider_calls(&fake)?;
        assert_eq!(snapshot_tree(runtime.path())?, runtime_before);
        project.assert_unchanged()?;
        fake.assert_state_unchanged()?;
    }

    Ok(())
}

#[test]
fn doctor_inspects_runtime_without_preparing_it() -> TestResult {
    let backend = BackendListener::new()?;
    let declaration = service_declaration(&[("web", backend.port()?)]);
    let publishing = publishing_intent(&[("web", OPENSSH_PUBLISHER)]);
    let configuration = openssh_configuration(&[("web", TEST_HOST, 18_080)]);

    let absent_project = Project::new(
        "doctor-runtime-child-absent",
        Some(&declaration),
        Some(&publishing),
        Some(&configuration),
    )?;
    let absent_runtime = ShortRuntimeDirectory::new()?;
    let absent_before = snapshot_tree(absent_runtime.path())?;
    let absent_fake = FakeProviders::new("doctor-runtime-child-absent-fake")?;
    let output = absent_fake.run(&absent_project, &absent_runtime, &["doctor"], &[])?;

    assert_diagnostic_result(&output, 0, 0);
    let stdout = output_stdout(&output);
    assert!(stdout.contains("PASS  Runtime path has no unsafe existing entry"));
    assert!(stdout.contains("PASS  web local control state: no existing master"));
    assert!(!absent_runtime.path().join("aequimuta").exists());
    assert_eq!(snapshot_tree(absent_runtime.path())?, absent_before);
    let calls = absent_fake.ssh_calls()?;
    assert_eq!(calls.len(), 1);
    assert!(is_ssh_expansion(&calls[0]));
    assert!(!is_ssh_check(&calls[0]));
    assert_only_non_mutating_provider_calls(&absent_fake)?;
    absent_project.assert_unchanged()?;

    let safe_project = Project::new(
        "doctor-runtime-child-safe",
        Some(&declaration),
        Some(&publishing),
        Some(&configuration),
    )?;
    let safe_runtime = ShortRuntimeDirectory::new()?;
    prepare_provider_runtime_directory(safe_runtime.path())?;
    let safe_before = snapshot_tree(safe_runtime.path())?;
    let safe_fake = FakeProviders::new("doctor-runtime-child-safe-fake")?;
    let output = safe_fake.run(&safe_project, &safe_runtime, &["doctor"], &[])?;

    assert_diagnostic_result(&output, 0, 0);
    assert!(output_stdout(&output).contains("PASS  Runtime path has no unsafe existing entry"));
    assert_eq!(snapshot_tree(safe_runtime.path())?, safe_before);
    assert_only_non_mutating_provider_calls(&safe_fake)?;
    safe_project.assert_unchanged()?;

    let unsafe_root_project = Project::new(
        "doctor-runtime-root-unsafe",
        Some(&declaration),
        Some(&publishing),
        Some(&configuration),
    )?;
    let unsafe_root = ShortRuntimeDirectory::new()?;
    fs::set_permissions(unsafe_root.path(), fs::Permissions::from_mode(0o755))?;
    let unsafe_root_before = snapshot_tree(unsafe_root.path())?;
    let unsafe_root_fake = FakeProviders::new("doctor-runtime-root-unsafe-fake")?;
    let output = unsafe_root_fake.run(&unsafe_root_project, &unsafe_root, &["doctor"], &[])?;

    assert_diagnostic_result(&output, 1, 1);
    assert!(output_stdout(&output).contains(
        "FAIL  Runtime path inspection: XDG_RUNTIME_DIR is not safe for OpenSSH reverse TCP control state"
    ));
    assert!(unsafe_root_fake.ssh_calls()?.is_empty());
    assert_eq!(snapshot_tree(unsafe_root.path())?, unsafe_root_before);
    unsafe_root_project.assert_unchanged()?;

    let unsafe_child_project = Project::new(
        "doctor-runtime-child-unsafe",
        Some(&declaration),
        Some(&publishing),
        Some(&configuration),
    )?;
    let unsafe_child_runtime = ShortRuntimeDirectory::new()?;
    let aequimuta_directory = unsafe_child_runtime.path().join("aequimuta");
    fs::create_dir(&aequimuta_directory)?;
    fs::set_permissions(&aequimuta_directory, fs::Permissions::from_mode(0o700))?;
    let unsafe_child = aequimuta_directory.join("openssh-reverse-tcp");
    fs::write(&unsafe_child, b"preserve unsafe runtime child\n")?;
    fs::set_permissions(&unsafe_child, fs::Permissions::from_mode(0o600))?;
    let unsafe_child_before = snapshot_tree(unsafe_child_runtime.path())?;
    let unsafe_child_fake = FakeProviders::new("doctor-runtime-child-unsafe-fake")?;
    let output = unsafe_child_fake.run(
        &unsafe_child_project,
        &unsafe_child_runtime,
        &["doctor"],
        &[],
    )?;

    assert_diagnostic_result(&output, 1, 1);
    assert!(output_stdout(&output).contains(
        "FAIL  Runtime path inspection: XDG_RUNTIME_DIR is not safe for OpenSSH reverse TCP control state"
    ));
    assert!(unsafe_child_fake.ssh_calls()?.is_empty());
    assert_eq!(
        snapshot_tree(unsafe_child_runtime.path())?,
        unsafe_child_before
    );
    assert_eq!(fs::read(&unsafe_child)?, b"preserve unsafe runtime child\n");
    unsafe_child_project.assert_unchanged()?;
    Ok(())
}

#[test]
fn doctor_checks_only_safe_existing_control_sockets() -> TestResult {
    let declaration_for = |port| service_declaration(&[("web", port)]);
    let publishing = publishing_intent(&[("web", OPENSSH_PUBLISHER)]);
    let configuration = openssh_configuration(&[("web", TEST_HOST, 18_080)]);

    let absent_backend = BackendListener::new()?;
    let absent_declaration = declaration_for(absent_backend.port()?);
    let absent_project = Project::new(
        "doctor-control-absent",
        Some(&absent_declaration),
        Some(&publishing),
        Some(&configuration),
    )?;
    let absent_runtime = ShortRuntimeDirectory::new()?;
    prepare_provider_runtime_directory(absent_runtime.path())?;
    let absent_before = snapshot_tree(absent_runtime.path())?;
    let absent_fake = FakeProviders::new("doctor-control-absent-fake")?;
    let output = absent_fake.run(&absent_project, &absent_runtime, &["doctor"], &[])?;

    assert_diagnostic_result(&output, 0, 0);
    assert!(output_stdout(&output).contains("PASS  web local control state: no existing master"));
    let absent_calls = absent_fake.ssh_calls()?;
    assert_eq!(absent_calls.len(), 1);
    assert!(is_ssh_expansion(&absent_calls[0]));
    assert!(!is_ssh_check(&absent_calls[0]));
    assert_eq!(snapshot_tree(absent_runtime.path())?, absent_before);
    assert_only_non_mutating_provider_calls(&absent_fake)?;
    absent_project.assert_unchanged()?;

    let live_backend = BackendListener::new()?;
    let live_declaration = declaration_for(live_backend.port()?);
    let live_project = Project::new(
        "doctor-control-live",
        Some(&live_declaration),
        Some(&publishing),
        Some(&configuration),
    )?;
    let live_runtime = ShortRuntimeDirectory::new()?;
    prepare_provider_runtime_directory(live_runtime.path())?;
    let live_path = expected_control_path(live_runtime.path(), TEST_HOST);
    let live_listener = UnixListener::bind(&live_path)?;
    fs::set_permissions(&live_path, fs::Permissions::from_mode(0o600))?;
    let live_before = snapshot_tree(live_runtime.path())?;
    let live_fake = FakeProviders::new("doctor-control-live-fake")?;
    let output = live_fake.run(&live_project, &live_runtime, &["doctor"], &[])?;

    assert_diagnostic_result(&output, 0, 0);
    assert!(
        output_stdout(&output).contains("PASS  web local control state: existing master responds")
    );
    let live_calls = live_fake.ssh_calls()?;
    assert_eq!(live_calls.len(), 2);
    assert_eq!(
        live_calls.iter().filter(|call| is_ssh_check(call)).count(),
        1
    );
    assert_eq!(snapshot_tree(live_runtime.path())?, live_before);
    assert_only_non_mutating_provider_calls(&live_fake)?;
    live_project.assert_unchanged()?;
    drop(live_listener);

    let stale_backend = BackendListener::new()?;
    let stale_declaration = declaration_for(stale_backend.port()?);
    let stale_project = Project::new(
        "doctor-control-stale",
        Some(&stale_declaration),
        Some(&publishing),
        Some(&configuration),
    )?;
    let stale_runtime = ShortRuntimeDirectory::new()?;
    prepare_provider_runtime_directory(stale_runtime.path())?;
    let stale_path = expected_control_path(stale_runtime.path(), TEST_HOST);
    let stale_listener = UnixListener::bind(&stale_path)?;
    fs::set_permissions(&stale_path, fs::Permissions::from_mode(0o600))?;
    drop(stale_listener);
    let stale_before = snapshot_tree(stale_runtime.path())?;
    let stale_fake = FakeProviders::new("doctor-control-stale-fake")?;
    let output = stale_fake.run(
        &stale_project,
        &stale_runtime,
        &["doctor"],
        &[("AEQUIMUTA_DOCTOR_SSH_CHECK_MODE", "stale")],
    )?;

    assert_diagnostic_result(&output, 1, 1);
    assert!(output_stdout(&output).contains(
        "FAIL  web local control state: OpenSSH control socket state is stale or unsafe"
    ));
    assert_no_secret_leak(&output);
    let stale_calls = stale_fake.ssh_calls()?;
    assert_eq!(stale_calls.len(), 2);
    assert_eq!(
        stale_calls.iter().filter(|call| is_ssh_check(call)).count(),
        1
    );
    assert_eq!(snapshot_tree(stale_runtime.path())?, stale_before);
    assert!(fs::symlink_metadata(&stale_path)?.file_type().is_socket());
    assert_only_non_mutating_provider_calls(&stale_fake)?;
    stale_project.assert_unchanged()?;

    let unsafe_backend = BackendListener::new()?;
    let unsafe_declaration = declaration_for(unsafe_backend.port()?);
    let unsafe_project = Project::new(
        "doctor-control-unsafe",
        Some(&unsafe_declaration),
        Some(&publishing),
        Some(&configuration),
    )?;
    let unsafe_runtime = ShortRuntimeDirectory::new()?;
    prepare_provider_runtime_directory(unsafe_runtime.path())?;
    let unsafe_path = expected_control_path(unsafe_runtime.path(), TEST_HOST);
    fs::write(&unsafe_path, b"preserve unsafe control entry\n")?;
    fs::set_permissions(&unsafe_path, fs::Permissions::from_mode(0o600))?;
    let unsafe_before = snapshot_tree(unsafe_runtime.path())?;
    let unsafe_fake = FakeProviders::new("doctor-control-unsafe-fake")?;
    let output = unsafe_fake.run(&unsafe_project, &unsafe_runtime, &["doctor"], &[])?;

    assert_diagnostic_result(&output, 1, 1);
    assert!(output_stdout(&output).contains(
        "FAIL  web local control state: OpenSSH control socket state is stale or unsafe"
    ));
    let unsafe_calls = unsafe_fake.ssh_calls()?;
    assert_eq!(unsafe_calls.len(), 1);
    assert!(is_ssh_expansion(&unsafe_calls[0]));
    assert!(!is_ssh_check(&unsafe_calls[0]));
    assert_eq!(snapshot_tree(unsafe_runtime.path())?, unsafe_before);
    assert_eq!(fs::read(&unsafe_path)?, b"preserve unsafe control entry\n");
    assert_only_non_mutating_provider_calls(&unsafe_fake)?;
    unsafe_project.assert_unchanged()?;
    Ok(())
}

#[test]
fn doctor_aggregates_failures_and_continues_independent_provider_branches() -> TestResult {
    let unreachable_port = closed_local_port()?;
    let declaration = service_declaration(&[("broken", unreachable_port)]);
    let publishing = publishing_intent(&[("broken", TAILSCALE_PUBLISHER)]);
    let conflict_json = format!(r#"{{"TCP":{{"{unreachable_port}":{{"HTTPS":true}}}}}}"#);
    let project = Project::new(
        "doctor-aggregation",
        Some(&declaration),
        Some(&publishing),
        None,
    )?;
    let runtime = ShortRuntimeDirectory::new()?;
    let fake = FakeProviders::new("doctor-aggregation-fake")?;
    let output = fake.run(
        &project,
        &runtime,
        &["doctor"],
        &[("AEQUIMUTA_DOCTOR_SERVE_JSON", &conflict_json)],
    )?;

    assert_diagnostic_result(&output, 1, 2);
    let stdout = output_stdout(&output);
    assert!(stdout.contains(&format!(
        "FAIL  broken 127.0.0.1:{unreachable_port} is not reachable"
    )));
    assert!(stdout.contains("FAIL  broken Serve slot is blocked by incompatible existing state"));
    assert_section_order(
        &stdout,
        &["Project\n", "Local backends\n", "Tailscale\n", "Found 2"],
    );
    assert_eq!(
        fake.tailscale_calls()?,
        ["status --json", "serve status --json"]
    );
    assert_only_non_mutating_provider_calls(&fake)?;
    project.assert_unchanged()?;

    let tailscale_backend = BackendListener::new()?;
    let openssh_backend = BackendListener::new()?;
    let declaration = service_declaration(&[
        ("web", tailscale_backend.port()?),
        ("admin", openssh_backend.port()?),
    ]);
    let publishing =
        publishing_intent(&[("web", TAILSCALE_PUBLISHER), ("admin", OPENSSH_PUBLISHER)]);
    let configuration = openssh_configuration(&[("admin", TEST_HOST, 18_080)]);
    let project = Project::new(
        "doctor-provider-independence",
        Some(&declaration),
        Some(&publishing),
        Some(&configuration),
    )?;
    let runtime = ShortRuntimeDirectory::new()?;
    let runtime_before = snapshot_tree(runtime.path())?;
    let fake = FakeProviders::new("doctor-provider-independence-fake")?;
    let output = fake.run(
        &project,
        &runtime,
        &["doctor"],
        &[("AEQUIMUTA_DOCTOR_CLIENT_MODE", "nonzero")],
    )?;

    assert_diagnostic_result(&output, 1, 1);
    let stdout = output_stdout(&output);
    assert!(stdout.contains("FAIL  Tailscale client prerequisites"));
    assert!(stdout.contains("OpenSSH\n"));
    assert!(stdout.contains("PASS  admin ssh executable and control-path resolution"));
    assert!(stdout.contains("PASS  admin local control state: no existing master"));
    assert_open_ssh_info_once(&output);
    assert_eq!(fake.tailscale_calls()?, ["status --json"]);
    let ssh_calls = fake.ssh_calls()?;
    assert_eq!(ssh_calls.len(), 1);
    assert!(is_ssh_expansion(&ssh_calls[0]));
    assert_eq!(snapshot_tree(runtime.path())?, runtime_before);
    assert_only_non_mutating_provider_calls(&fake)?;
    assert_no_secret_leak(&output);
    project.assert_unchanged()?;
    fake.assert_state_unchanged()?;
    Ok(())
}

#[test]
fn doctor_preserves_all_scoped_state_and_emits_deterministic_output() -> TestResult {
    let backend = BackendListener::new()?;
    let port = backend.port()?;
    let declaration = service_declaration(&[("web", port)]);
    let publishing = publishing_intent(&[("web", TAILSCALE_PUBLISHER), ("web", OPENSSH_PUBLISHER)]);
    let configuration = openssh_configuration(&[("web", TEST_HOST, 18_080)]);
    let project = Project::new(
        "doctor-deterministic",
        Some(&declaration),
        Some(&publishing),
        Some(&configuration),
    )?;
    let runtime = ShortRuntimeDirectory::new()?;
    prepare_provider_runtime_directory(runtime.path())?;
    let control_path = expected_control_path(runtime.path(), TEST_HOST);
    let control_listener = UnixListener::bind(&control_path)?;
    fs::set_permissions(&control_path, fs::Permissions::from_mode(0o600))?;
    let runtime_before = snapshot_tree(runtime.path())?;
    let fake = FakeProviders::new("doctor-deterministic-fake")?;
    let serve_json = format!(r#"{{"TCP":{{"{port}":{{"TCPForward":"127.0.0.1:{port}"}}}}}}"#);
    let output = fake.run(
        &project,
        &runtime,
        &["doctor"],
        &[("AEQUIMUTA_DOCTOR_SERVE_JSON", &serve_json)],
    )?;

    assert_diagnostic_result(&output, 0, 0);
    let expected = format!(
        "Project\n\
         PASS  aequimuta.toml\n\
         PASS  aequimuta.publish.toml\n\
         INFO  Desired publications: 2\n\
         PASS  Operational publisher support\n\
         PASS  Tailscale desired-slot rules\n\
         Local backends\n\
         PASS  web 127.0.0.1:{port}\n\
         Tailscale\n\
         PASS  Client state permits endpoint resolution\n\
         PASS  web Serve slot permits apply\n\
         OpenSSH\n\
         PASS  Provider configuration and desired-slot rules\n\
         PASS  Runtime path has no unsafe existing entry\n\
         PASS  web ssh executable and control-path resolution\n\
         PASS  web local control state: existing master responds\n\
         INFO  OpenSSH remote reachability, host-key trust, credentials, authentication, forwarding policy, and listener availability were not probed\n\
         No blocking readiness issues detected by performed checks\n"
    );
    assert_eq!(output.stdout, expected.as_bytes());
    assert_open_ssh_info_once(&output);
    assert_section_order(
        &output_stdout(&output),
        &[
            "Project\n",
            "Local backends\n",
            "Tailscale\n",
            "OpenSSH\n",
            "No blocking readiness issues",
        ],
    );
    assert!(!output_stdout(&output).contains(runtime.path().to_string_lossy().as_ref()));
    assert_eq!(backend.accepted_payloads()?, [Vec::<u8>::new()]);
    assert_eq!(
        fake.tailscale_calls()?,
        ["status --json", "serve status --json"]
    );
    let ssh_calls = fake.ssh_calls()?;
    assert_eq!(ssh_calls.len(), 2);
    assert_eq!(
        ssh_calls
            .iter()
            .filter(|call| is_ssh_expansion(call))
            .count(),
        1
    );
    assert_eq!(
        ssh_calls.iter().filter(|call| is_ssh_check(call)).count(),
        1
    );
    assert_only_non_mutating_provider_calls(&fake)?;
    assert_no_secret_leak(&output);
    assert_eq!(snapshot_tree(runtime.path())?, runtime_before);
    project.assert_unchanged()?;
    fake.assert_state_unchanged()?;
    drop(control_listener);
    Ok(())
}
