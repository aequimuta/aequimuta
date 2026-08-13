#[allow(dead_code)]
mod support;

use std::ffi::OsString;
use std::fs;
use std::io;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use support::TestDirectory;

const DECLARATION_FILE: &str = "aequimuta.toml";
const PUBLISHING_FILE: &str = "aequimuta.publish.toml";
const OPENSSH_CONFIGURATION_FILE: &str = "aequimuta.openssh-reverse-tcp.toml";
const TAILSCALE_PUBLISHER: &str = "tailscale-serve-tcp";
const OPENSSH_PUBLISHER: &str = "openssh-reverse-tcp";
const TEST_DNS_NAME: &str = "apply-test.example.ts.net";
const TEST_HOST: &str = "edge.example.com";
const TEST_USER: &str = "aequimuta";
const TEST_SSH_PORT: u16 = 22;
const OPENSSH_CONTROL_FILE: &str = "cm-0123456789abcdef0123456789abcdef01234567";
const FNV1A_64_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV1A_64_PRIME: u64 = 0x00000100000001b3;
const USAGE_STDERR: &[u8] = b"Usage: aequimuta <command>\n";

static NEXT_SCRATCH_DIRECTORY: AtomicU64 = AtomicU64::new(0);

type TestResult = Result<(), Box<dyn std::error::Error>>;
type InvalidInputCase<'a> = (&'a str, &'a [u8], &'a [u8], &'a [u8]);
type OpenSshConfigurationCase<'a> = (&'a str, Option<&'a [u8]>, &'a [u8]);

struct Project {
    directory: TestDirectory,
    declaration: Vec<u8>,
    publishing: Vec<u8>,
    openssh_configuration: Option<Vec<u8>>,
    entries: Vec<OsString>,
}

impl Project {
    fn new(
        label: &str,
        declaration: &[u8],
        publishing: &[u8],
        openssh_configuration: Option<&[u8]>,
    ) -> io::Result<Self> {
        let directory = TestDirectory::new(label)?;
        fs::write(directory.path().join(DECLARATION_FILE), declaration)?;
        fs::write(directory.path().join(PUBLISHING_FILE), publishing)?;

        if let Some(configuration) = openssh_configuration {
            fs::write(
                directory.path().join(OPENSSH_CONFIGURATION_FILE),
                configuration,
            )?;
        }

        Ok(Self {
            declaration: declaration.to_vec(),
            publishing: publishing.to_vec(),
            openssh_configuration: openssh_configuration.map(<[u8]>::to_vec),
            entries: directory_entries(directory.path())?,
            directory,
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

        match &self.openssh_configuration {
            Some(expected) => assert_eq!(
                fs::read(self.path().join(OPENSSH_CONFIGURATION_FILE))?,
                *expected,
                "OpenSSH provider configuration changed"
            ),
            None => assert!(
                !self.path().join(OPENSSH_CONFIGURATION_FILE).exists(),
                "OpenSSH provider configuration was created"
            ),
        }

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

struct ScratchDirectory {
    path: PathBuf,
    cleaned: bool,
}

impl ScratchDirectory {
    fn new(prefix: &str) -> io::Result<Self> {
        loop {
            let sequence = NEXT_SCRATCH_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let process = std::process::id() & 0x00ff_ffff;
            let path =
                std::env::temp_dir().join(format!("{prefix}{process:06x}{:02x}", sequence & 0xff));

            match fs::create_dir(&path) {
                Ok(()) => {
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
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

    fn cleanup(&mut self) -> io::Result<()> {
        if self.cleaned {
            return Ok(());
        }

        fs::remove_dir_all(&self.path)?;
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for ScratchDirectory {
    fn drop(&mut self) {
        if !self.cleaned
            && let Err(error) = fs::remove_dir_all(&self.path)
        {
            eprintln!(
                "failed to clean up apply test directory {}: {error}",
                self.path.display()
            );
        }
    }
}

struct SocketEmulator {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<io::Result<()>>>,
}

impl SocketEmulator {
    fn start(master_pids: PathBuf, control_path: PathBuf) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while recorded_pid_count(&master_pids)? < 1 {
                if thread_stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(5));
            }

            let staging_path = control_path.with_file_name("fixture-control-socket");
            let listener = UnixListener::bind(&staging_path)?;
            fs::set_permissions(&staging_path, fs::Permissions::from_mode(0o600))?;
            fs::rename(&staging_path, &control_path)?;

            while !thread_stop.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(5));
            }

            drop(listener);
            match fs::remove_file(&control_path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            }
        });

        Self {
            stop,
            handle: Some(handle),
        }
    }

    fn stop(&mut self) -> io::Result<()> {
        self.stop.store(true, Ordering::Release);
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };

        handle
            .join()
            .map_err(|_| io::Error::other("fake control socket thread panicked"))?
    }
}

impl Drop for SocketEmulator {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("failed to stop apply fake control socket: {error}");
        }
    }
}

struct FakeProviders {
    root: ScratchDirectory,
    runtime: ScratchDirectory,
    log: PathBuf,
    state_directory: PathBuf,
    master_state: PathBuf,
    master_pids: PathBuf,
    master_stop: PathBuf,
    reported_control_path: PathBuf,
    control_path: PathBuf,
    socket_emulator: Option<SocketEmulator>,
    stopped: bool,
}

impl FakeProviders {
    fn new() -> io::Result<Self> {
        let root = ScratchDirectory::new("aqf")?;
        let runtime = ScratchDirectory::new("aqr")?;
        let log = root.path().join("calls.log");
        let state_directory = root.path().join("state");
        let master_state = root.path().join("master-state");
        let master_pids = root.path().join("master-pids");
        let master_stop = root.path().join("master-stop");
        fs::create_dir(&state_directory)?;

        let tailscale = root.path().join("tailscale");
        fs::write(&tailscale, FAKE_TAILSCALE)?;
        fs::set_permissions(&tailscale, fs::Permissions::from_mode(0o700))?;

        let ssh = root.path().join("ssh");
        fs::write(&ssh, FAKE_SSH)?;
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700))?;

        let provider_runtime = runtime.path().join("aequimuta/openssh-reverse-tcp");
        let reported_control_path = provider_runtime.join(OPENSSH_CONTROL_FILE);
        let control_path = expected_control_path(runtime.path());

        Ok(Self {
            root,
            runtime,
            log,
            state_directory,
            master_state,
            master_pids,
            master_stop,
            reported_control_path,
            control_path,
            socket_emulator: None,
            stopped: false,
        })
    }

    fn enable_master_creation(&mut self) {
        self.socket_emulator = Some(SocketEmulator::start(
            self.master_pids.clone(),
            self.control_path.clone(),
        ));
    }

    fn run(
        &self,
        project: &Project,
        args: &[&str],
        fail_tailscale_port: Option<u16>,
        fail_openssh_local_port: Option<u16>,
    ) -> io::Result<Output> {
        Command::new(env!("CARGO_BIN_EXE_aequimuta"))
            .args(args)
            .current_dir(project.path())
            .env("PATH", self.root.path())
            .env("XDG_RUNTIME_DIR", self.runtime.path())
            .env("AEQUIMUTA_APPLY_FAKE_LOG", &self.log)
            .env("AEQUIMUTA_APPLY_FAKE_STATE", &self.state_directory)
            .env("AEQUIMUTA_APPLY_FAKE_MASTER_STATE", &self.master_state)
            .env("AEQUIMUTA_APPLY_FAKE_MASTER_PIDS", &self.master_pids)
            .env("AEQUIMUTA_APPLY_FAKE_MASTER_STOP", &self.master_stop)
            .env(
                "AEQUIMUTA_APPLY_FAKE_REPORTED_CONTROL",
                &self.reported_control_path,
            )
            .env("AEQUIMUTA_APPLY_FAKE_CONTROL", &self.control_path)
            .env(
                "AEQUIMUTA_APPLY_FAIL_TAILSCALE_PORT",
                fail_tailscale_port
                    .map(|port| port.to_string())
                    .unwrap_or_default(),
            )
            .env(
                "AEQUIMUTA_APPLY_FAIL_OPENSSH_LOCAL_PORT",
                fail_openssh_local_port
                    .map(|port| port.to_string())
                    .unwrap_or_default(),
            )
            .output()
    }

    fn calls(&self) -> io::Result<Vec<String>> {
        match fs::read_to_string(&self.log) {
            Ok(source) => Ok(source.lines().map(str::to_owned).collect()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    fn order_events(&self) -> io::Result<Vec<String>> {
        Ok(self
            .calls()?
            .into_iter()
            .filter(|line| line.starts_with("ORDER "))
            .collect())
    }

    fn master_count(&self) -> io::Result<usize> {
        recorded_pid_count(&self.master_pids)
    }

    fn tailscale_state_exists(&self, port: u16) -> bool {
        self.state_directory
            .join(format!("tailscale-{port}"))
            .exists()
    }

    fn assert_no_rollback_or_delete(&self) -> io::Result<()> {
        let calls = self.calls()?;
        assert!(
            calls.iter().all(|call| {
                !call.contains(" reset")
                    && !call.ends_with(" off")
                    && !call.contains(" serve off")
                    && !call.contains(" -O cancel ")
                    && !call.contains(" -O exit ")
            }),
            "forbidden rollback or delete call: {calls:?}"
        );
        Ok(())
    }

    fn shutdown(&mut self) -> io::Result<()> {
        if self.stopped {
            return Ok(());
        }

        fs::write(&self.master_stop, b"stop\n")?;
        let pids = recorded_pids(&self.master_pids)?;
        let deadline = Instant::now() + Duration::from_secs(3);

        for pid in pids {
            let process_path = Path::new("/proc").join(pid.to_string());
            while process_path.exists() {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("fake OpenSSH master {pid} did not exit"),
                    ));
                }
                thread::sleep(Duration::from_millis(5));
            }
        }

        if let Some(mut emulator) = self.socket_emulator.take() {
            emulator.stop()?;
        }
        self.stopped = true;
        Ok(())
    }

    fn cleanup(mut self) -> io::Result<()> {
        self.shutdown()?;
        self.root.cleanup()?;
        self.runtime.cleanup()
    }
}

impl Drop for FakeProviders {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            eprintln!("failed to stop apply fake providers: {error}");
        }
    }
}

fn expected_control_path(runtime: &Path) -> PathBuf {
    let mut hash = FNV1A_64_OFFSET_BASIS;

    for byte in TEST_HOST
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
    runtime
        .join("aequimuta/openssh-reverse-tcp")
        .join(file_name)
}

fn recorded_pid_count(path: &Path) -> io::Result<usize> {
    Ok(recorded_pids(path)?.len())
}

fn recorded_pids(path: &Path) -> io::Result<Vec<u32>> {
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    source
        .lines()
        .map(|line| {
            line.parse::<u32>()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid fake master PID"))
        })
        .collect()
}

fn directory_entries(path: &Path) -> io::Result<Vec<OsString>> {
    let mut entries = fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort();
    Ok(entries)
}

fn service_declaration(services: &[(&str, u16)]) -> Vec<u8> {
    let mut declaration = String::new();

    for (name, port) in services {
        declaration.push_str(&format!(
            "[[services]]\nname = \"{name}\"\nport = {port}\n\n"
        ));
    }

    declaration.into_bytes()
}

fn publishing_intent(publications: &[(&str, &str)]) -> Vec<u8> {
    let mut publishing = String::new();

    for (service, publisher) in publications {
        publishing.push_str(&format!(
            "[[publications]]\nservice = \"{service}\"\npublisher = \"{publisher}\"\n\n"
        ));
    }

    publishing.into_bytes()
}

fn openssh_configuration(publications: &[(&str, &str, u16)]) -> Vec<u8> {
    let mut configuration = String::new();

    for (service, listen_address, listen_port) in publications {
        configuration.push_str(&format!(
            "[[publications]]\n\
             service = \"{service}\"\n\
             host = \"{TEST_HOST}\"\n\
             user = \"{TEST_USER}\"\n\
             ssh_port = {TEST_SSH_PORT}\n\
             listen_address = \"{listen_address}\"\n\
             listen_port = {listen_port}\n\n"
        ));
    }

    configuration.into_bytes()
}

fn expected_tailscale_created(service: &str, port: u16) -> String {
    format!("Published {service} via {TAILSCALE_PUBLISHER} at tcp://{TEST_DNS_NAME}:{port}\n")
}

fn expected_tailscale_satisfied(service: &str, port: u16) -> String {
    format!(
        "Publication already satisfied for {service} via {TAILSCALE_PUBLISHER} at tcp://{TEST_DNS_NAME}:{port}\n"
    )
}

fn expected_openssh_ensured(
    service: &str,
    local_port: u16,
    listen_address: &str,
    listen_port: u16,
) -> String {
    format!(
        "Ensured {service} via {OPENSSH_PUBLISHER}: \
         {TEST_USER}@{TEST_HOST}:{TEST_SSH_PORT} listen \
         {listen_address}:{listen_port} -> 127.0.0.1:{local_port} \
         (SSH-session-backed; no automatic reconnect)\n"
    )
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
        "{label}: missing error prefix: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.ends_with(b"\n"),
        "{label}: missing final newline"
    );
    assert_eq!(
        output.stderr.iter().filter(|&&byte| byte == b'\n').count(),
        1,
        "{label}: error was not one line: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn closed_local_port() -> io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

#[test]
fn apply_rejects_extra_arguments_before_project_or_provider_access() -> TestResult {
    let project = Project::new("apply-usage", &[0xff], &[0xff], Some(&[0xff]))?;
    let fake = FakeProviders::new()?;

    for args in [
        &["apply", "web"][..],
        &["apply", "web", TAILSCALE_PUBLISHER][..],
        &["apply", "--all"][..],
    ] {
        let output = fake.run(&project, args, None, None)?;

        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert_eq!(output.stderr, USAGE_STDERR);
    }

    assert!(fake.calls()?.is_empty());
    project.assert_unchanged()?;
    fake.cleanup()?;
    project.cleanup()?;
    Ok(())
}

#[test]
fn apply_empty_desired_state_is_exact_noop() -> TestResult {
    let project = Project::new(
        "apply-empty",
        b"# no services\n",
        b"# no desired publications\n",
        Some(b"stale = [invalid provider configuration\n"),
    )?;
    let fake = FakeProviders::new()?;

    let output = fake.run(&project, &["apply"], None, None)?;

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"No desired publications to apply\n");
    assert!(output.stderr.is_empty());
    assert!(fake.calls()?.is_empty());
    project.assert_unchanged()?;
    fake.cleanup()?;
    project.cleanup()?;
    Ok(())
}

#[test]
fn apply_rejects_invalid_core_or_publishing_before_provider_mutation() -> TestResult {
    let cases: &[InvalidInputCase<'_>] = &[
        (
            "invalid-core",
            b"[[services]\n",
            b"# publishing is otherwise empty\n",
            b"error: aequimuta.toml is not a valid declaration\n",
        ),
        (
            "invalid-publishing",
            b"# declaration is valid and empty\n",
            b"[[publications]\n",
            b"error: aequimuta.publish.toml is not a valid publishing intent\n",
        ),
    ];

    for &(label, declaration, publishing, expected_stderr) in cases {
        let project = Project::new(label, declaration, publishing, None)?;
        let fake = FakeProviders::new()?;
        let output = fake.run(&project, &["apply"], None, None)?;

        assert_operation_failure(&output, label);
        assert_eq!(output.stderr, expected_stderr);
        assert!(fake.calls()?.is_empty(), "{label}: provider was invoked");
        project.assert_unchanged()?;
        fake.cleanup()?;
        project.cleanup()?;
    }

    Ok(())
}

#[test]
fn apply_rejects_unsupported_late_entry_before_mutation() -> TestResult {
    let web = TcpListener::bind(("127.0.0.1", 0))?;
    let api = TcpListener::bind(("127.0.0.1", 0))?;
    let web_port = web.local_addr()?.port();
    let api_port = api.local_addr()?.port();
    let declaration = service_declaration(&[("web", web_port), ("api", api_port)]);
    let publishing =
        publishing_intent(&[("web", TAILSCALE_PUBLISHER), ("api", "example-publisher")]);
    let project = Project::new("apply-unsupported-late", &declaration, &publishing, None)?;
    let fake = FakeProviders::new()?;

    let output = fake.run(&project, &["apply"], None, None)?;

    assert_operation_failure(&output, "unsupported late entry");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not supported"),
        "unexpected unsupported-publisher error: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fake.calls()?.is_empty(),
        "provider was invoked before rejection"
    );
    assert!(!fake.tailscale_state_exists(web_port));
    project.assert_unchanged()?;
    fake.cleanup()?;
    project.cleanup()?;
    drop((web, api));
    Ok(())
}

#[test]
fn apply_rejects_openssh_configuration_failures_before_mutation() -> TestResult {
    let backend = TcpListener::bind(("127.0.0.1", 0))?;
    let port = backend.local_addr()?.port();
    let declaration = service_declaration(&[("web", port)]);
    let publishing = publishing_intent(&[("web", OPENSSH_PUBLISHER)]);
    let invalid_configuration = b"future = true\n";
    let unresolved_configuration = b"# no provider publications\n";
    let cases: &[OpenSshConfigurationCase<'_>] = &[
        (
            "missing-provider-file",
            None,
            b"error: failed to read aequimuta.openssh-reverse-tcp.toml\n",
        ),
        (
            "invalid-provider-file",
            Some(invalid_configuration),
            b"error: aequimuta.openssh-reverse-tcp.toml is not a valid OpenSSH reverse TCP configuration\n",
        ),
        (
            "unresolved-provider-service",
            Some(unresolved_configuration),
            b"error: desired OpenSSH reverse TCP publication has no provider configuration\n",
        ),
    ];

    for &(label, configuration, expected_stderr) in cases {
        let project = Project::new(label, &declaration, &publishing, configuration)?;
        let fake = FakeProviders::new()?;
        let output = fake.run(&project, &["apply"], None, None)?;

        assert_operation_failure(&output, label);
        assert_eq!(output.stderr, expected_stderr);
        assert!(fake.calls()?.is_empty(), "{label}: provider was invoked");
        project.assert_unchanged()?;
        fake.cleanup()?;
        project.cleanup()?;
    }

    drop(backend);
    Ok(())
}

#[test]
fn apply_rejects_provider_specific_ambiguities_before_mutation() -> TestResult {
    let shared = TcpListener::bind(("127.0.0.1", 0))?;
    let shared_port = shared.local_addr()?.port();
    let tailscale_declaration = service_declaration(&[("web", shared_port), ("api", shared_port)]);
    let tailscale_publishing =
        publishing_intent(&[("web", TAILSCALE_PUBLISHER), ("api", TAILSCALE_PUBLISHER)]);
    let tailscale_project = Project::new(
        "apply-tailscale-ambiguity",
        &tailscale_declaration,
        &tailscale_publishing,
        None,
    )?;
    let tailscale_fake = FakeProviders::new()?;

    let tailscale_output = tailscale_fake.run(&tailscale_project, &["apply"], None, None)?;

    assert_operation_failure(&tailscale_output, "Tailscale ambiguity");
    assert_eq!(
        tailscale_output.stderr,
        b"error: desired Tailscale Serve TCP publications conflict\n"
    );
    assert!(tailscale_fake.calls()?.is_empty());
    tailscale_project.assert_unchanged()?;
    tailscale_fake.cleanup()?;
    tailscale_project.cleanup()?;
    drop(shared);

    let web = TcpListener::bind(("127.0.0.1", 0))?;
    let api = TcpListener::bind(("127.0.0.1", 0))?;
    let declaration = service_declaration(&[
        ("web", web.local_addr()?.port()),
        ("api", api.local_addr()?.port()),
    ]);
    let publishing = publishing_intent(&[("web", OPENSSH_PUBLISHER), ("api", OPENSSH_PUBLISHER)]);
    let configuration =
        openssh_configuration(&[("web", "127.0.0.1", 18_080), ("api", "0.0.0.0", 18_080)]);
    let openssh_project = Project::new(
        "apply-openssh-ambiguity",
        &declaration,
        &publishing,
        Some(&configuration),
    )?;
    let openssh_fake = FakeProviders::new()?;

    let openssh_output = openssh_fake.run(&openssh_project, &["apply"], None, None)?;

    assert_operation_failure(&openssh_output, "OpenSSH ambiguity");
    assert_eq!(
        openssh_output.stderr,
        b"error: desired OpenSSH reverse TCP publications conflict\n"
    );
    assert!(openssh_fake.calls()?.is_empty());
    openssh_project.assert_unchanged()?;
    openssh_fake.cleanup()?;
    openssh_project.cleanup()?;
    drop((web, api));
    Ok(())
}

#[test]
fn apply_preflights_all_unique_local_backends_before_mutation() -> TestResult {
    let reachable = TcpListener::bind(("127.0.0.1", 0))?;
    let reachable_port = reachable.local_addr()?.port();
    let unreachable_port = closed_local_port()?;
    let declaration =
        service_declaration(&[("front", reachable_port), ("broken", unreachable_port)]);
    let publishing = publishing_intent(&[
        ("front", TAILSCALE_PUBLISHER),
        ("broken", OPENSSH_PUBLISHER),
        ("broken", TAILSCALE_PUBLISHER),
    ]);
    let configuration = openssh_configuration(&[("broken", "127.0.0.1", 18_081)]);
    let project = Project::new(
        "apply-backend-preflight",
        &declaration,
        &publishing,
        Some(&configuration),
    )?;
    let fake = FakeProviders::new()?;

    let output = fake.run(&project, &["apply"], None, None)?;

    assert_operation_failure(&output, "project-wide backend preflight");
    assert_eq!(
        output.stderr,
        format!("error: local TCP backend 127.0.0.1:{unreachable_port} is not reachable\n")
            .as_bytes()
    );
    assert!(
        fake.order_events()?.is_empty(),
        "provider mutation began before all backends passed"
    );
    assert_eq!(fake.master_count()?, 0);
    assert!(!fake.tailscale_state_exists(reachable_port));
    project.assert_unchanged()?;
    fake.cleanup()?;
    project.cleanup()?;
    drop(reachable);
    Ok(())
}

#[test]
fn apply_preflights_openssh_runtime_safety_before_provider_invocation() -> TestResult {
    let backend = TcpListener::bind(("127.0.0.1", 0))?;
    let port = backend.local_addr()?.port();
    let declaration = service_declaration(&[("web", port)]);
    let publishing = publishing_intent(&[("web", OPENSSH_PUBLISHER)]);
    let configuration = openssh_configuration(&[("web", "127.0.0.1", 18_086)]);
    let project = Project::new(
        "apply-openssh-runtime-preflight",
        &declaration,
        &publishing,
        Some(&configuration),
    )?;
    let fake = FakeProviders::new()?;
    fs::set_permissions(fake.runtime.path(), fs::Permissions::from_mode(0o755))?;

    let output = fake.run(&project, &["apply"], None, None)?;

    assert_operation_failure(&output, "OpenSSH runtime preflight");
    assert_eq!(
        output.stderr,
        b"error: XDG_RUNTIME_DIR is not safe for OpenSSH reverse TCP control state\n"
    );
    assert!(
        fake.calls()?.is_empty(),
        "unsafe OpenSSH runtime invoked a provider"
    );
    assert_eq!(fake.master_count()?, 0);
    project.assert_unchanged()?;
    fake.cleanup()?;
    project.cleanup()?;
    drop(backend);
    Ok(())
}

#[test]
fn tailscale_only_apply_ignores_stale_openssh_configuration() -> TestResult {
    let backend = TcpListener::bind(("127.0.0.1", 0))?;
    let port = backend.local_addr()?.port();
    let declaration = service_declaration(&[("web", port)]);
    let publishing = publishing_intent(&[("web", TAILSCALE_PUBLISHER)]);
    let project = Project::new(
        "apply-tailscale-stale-openssh",
        &declaration,
        &publishing,
        Some(b"not valid TOML = [\nprivate_key = \"must-not-be-read\"\n"),
    )?;
    let fake = FakeProviders::new()?;

    let output = fake.run(&project, &["apply"], None, None)?;

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        output.stdout,
        format!(
            "{}Applied 1 desired publications\n",
            expected_tailscale_created("web", port)
        )
        .as_bytes()
    );
    assert!(output.stderr.is_empty());
    assert!(
        fake.calls()?
            .iter()
            .all(|call| !call.starts_with("CALL ssh ")),
        "Tailscale-only apply read or used OpenSSH configuration"
    );
    assert!(fake.tailscale_state_exists(port));
    project.assert_unchanged()?;
    fake.cleanup()?;
    project.cleanup()?;
    drop(backend);
    Ok(())
}

#[test]
fn apply_executes_mixed_publications_in_declaration_order_and_is_immutable() -> TestResult {
    let zeta_backend = TcpListener::bind(("127.0.0.1", 0))?;
    let alpha_backend = TcpListener::bind(("127.0.0.1", 0))?;
    let middle_backend = TcpListener::bind(("127.0.0.1", 0))?;
    let zeta_port = zeta_backend.local_addr()?.port();
    let alpha_port = alpha_backend.local_addr()?.port();
    let middle_port = middle_backend.local_addr()?.port();
    let declaration = service_declaration(&[
        ("alpha", alpha_port),
        ("middle", middle_port),
        ("zeta", zeta_port),
    ]);
    let publishing = publishing_intent(&[
        ("zeta", TAILSCALE_PUBLISHER),
        ("alpha", OPENSSH_PUBLISHER),
        ("middle", TAILSCALE_PUBLISHER),
    ]);
    let configuration = openssh_configuration(&[("alpha", "127.0.0.1", 18_082)]);
    let project = Project::new(
        "apply-mixed-order",
        &declaration,
        &publishing,
        Some(&configuration),
    )?;
    let mut fake = FakeProviders::new()?;
    fake.enable_master_creation();

    let output = fake.run(&project, &["apply"], None, None)?;

    let expected_stdout = format!(
        "{}{}{}Applied 3 desired publications\n",
        expected_tailscale_created("zeta", zeta_port),
        expected_openssh_ensured("alpha", alpha_port, "127.0.0.1", 18_082),
        expected_tailscale_created("middle", middle_port),
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, expected_stdout.as_bytes());
    assert!(output.stderr.is_empty());
    assert_eq!(
        fake.order_events()?,
        vec![
            format!("ORDER tailscale {zeta_port}"),
            format!("ORDER openssh {alpha_port}"),
            format!("ORDER tailscale {middle_port}"),
        ]
    );
    assert_eq!(fake.master_count()?, 1);
    fake.assert_no_rollback_or_delete()?;
    project.assert_unchanged()?;
    fake.cleanup()?;
    project.cleanup()?;
    drop((zeta_backend, alpha_backend, middle_backend));
    Ok(())
}

#[test]
fn repeat_apply_preserves_concrete_outcomes_and_reuses_openssh_master() -> TestResult {
    let web_backend = TcpListener::bind(("127.0.0.1", 0))?;
    let api_backend = TcpListener::bind(("127.0.0.1", 0))?;
    let web_port = web_backend.local_addr()?.port();
    let api_port = api_backend.local_addr()?.port();
    let declaration = service_declaration(&[("web", web_port), ("api", api_port)]);
    let publishing = publishing_intent(&[("web", TAILSCALE_PUBLISHER), ("api", OPENSSH_PUBLISHER)]);
    let configuration = openssh_configuration(&[("api", "127.0.0.1", 18_083)]);
    let project = Project::new(
        "apply-repeat",
        &declaration,
        &publishing,
        Some(&configuration),
    )?;
    let mut fake = FakeProviders::new()?;
    fake.enable_master_creation();

    let first = fake.run(&project, &["apply"], None, None)?;
    let second = fake.run(&project, &["apply"], None, None)?;

    assert_eq!(first.status.code(), Some(0));
    assert_eq!(
        first.stdout,
        format!(
            "{}{}Applied 2 desired publications\n",
            expected_tailscale_created("web", web_port),
            expected_openssh_ensured("api", api_port, "127.0.0.1", 18_083),
        )
        .as_bytes()
    );
    assert!(first.stderr.is_empty());
    assert_eq!(second.status.code(), Some(0));
    assert_eq!(
        second.stdout,
        format!(
            "{}{}Applied 2 desired publications\n",
            expected_tailscale_satisfied("web", web_port),
            expected_openssh_ensured("api", api_port, "127.0.0.1", 18_083),
        )
        .as_bytes()
    );
    assert!(second.stderr.is_empty());
    assert_eq!(
        fake.master_count()?,
        1,
        "repeat apply created another master"
    );
    let calls = fake.calls()?;
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("ORDER tailscale "))
            .count(),
        1,
        "repeat apply mutated Tailscale again"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("ORDER openssh "))
            .count(),
        2,
        "each OpenSSH ensure must receive forwarding acknowledgement"
    );
    fake.assert_no_rollback_or_delete()?;
    project.assert_unchanged()?;
    fake.cleanup()?;
    project.cleanup()?;
    drop((web_backend, api_backend));
    Ok(())
}

#[test]
fn apply_stops_after_first_runtime_failure() -> TestResult {
    let web_backend = TcpListener::bind(("127.0.0.1", 0))?;
    let api_backend = TcpListener::bind(("127.0.0.1", 0))?;
    let web_port = web_backend.local_addr()?.port();
    let api_port = api_backend.local_addr()?.port();
    let declaration = service_declaration(&[("web", web_port), ("api", api_port)]);
    let publishing = publishing_intent(&[("web", TAILSCALE_PUBLISHER), ("api", OPENSSH_PUBLISHER)]);
    let configuration = openssh_configuration(&[("api", "127.0.0.1", 18_084)]);
    let project = Project::new(
        "apply-first-runtime-failure",
        &declaration,
        &publishing,
        Some(&configuration),
    )?;
    let fake = FakeProviders::new()?;

    let output = fake.run(&project, &["apply"], Some(web_port), None)?;

    assert_operation_failure(&output, "first runtime failure");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("web"));
    assert!(stderr.contains(TAILSCALE_PUBLISHER));
    assert!(!stderr.contains("earlier successful publications were not rolled back"));
    assert!(!stderr.contains("super-secret provider diagnostic"));
    assert!(!stderr.contains("Applied 2 desired publications"));
    assert_eq!(
        fake.order_events()?,
        vec![format!("ORDER tailscale {web_port}")]
    );
    assert!(
        fake.calls()?
            .iter()
            .all(|call| !call.starts_with("CALL ssh ")),
        "later OpenSSH publication was invoked"
    );
    assert!(!fake.tailscale_state_exists(web_port));
    fake.assert_no_rollback_or_delete()?;
    project.assert_unchanged()?;
    fake.cleanup()?;
    project.cleanup()?;
    drop((web_backend, api_backend));
    Ok(())
}

#[test]
fn apply_preserves_prior_success_without_rollback_and_stops_remaining() -> TestResult {
    let web_backend = TcpListener::bind(("127.0.0.1", 0))?;
    let api_backend = TcpListener::bind(("127.0.0.1", 0))?;
    let db_backend = TcpListener::bind(("127.0.0.1", 0))?;
    let web_port = web_backend.local_addr()?.port();
    let api_port = api_backend.local_addr()?.port();
    let db_port = db_backend.local_addr()?.port();
    let declaration = service_declaration(&[("web", web_port), ("api", api_port), ("db", db_port)]);
    let publishing = publishing_intent(&[
        ("web", TAILSCALE_PUBLISHER),
        ("api", OPENSSH_PUBLISHER),
        ("db", TAILSCALE_PUBLISHER),
    ]);
    let configuration = openssh_configuration(&[("api", "127.0.0.1", 18_085)]);
    let project = Project::new(
        "apply-partial-runtime-failure",
        &declaration,
        &publishing,
        Some(&configuration),
    )?;
    let mut fake = FakeProviders::new()?;
    fake.enable_master_creation();

    let output = fake.run(&project, &["apply"], None, Some(api_port))?;

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        output.stdout,
        expected_tailscale_created("web", web_port).as_bytes()
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Applied 3"));
    assert_eq!(
        output.stderr.iter().filter(|&&byte| byte == b'\n').count(),
        1,
        "partial failure must remain a one-line error"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.starts_with("error: "));
    assert!(stderr.contains("api"));
    assert!(stderr.contains(OPENSSH_PUBLISHER));
    assert!(stderr.contains("earlier successful publications were not rolled back"));
    assert!(!stderr.contains("super-secret provider diagnostic"));
    assert_eq!(
        fake.order_events()?,
        vec![
            format!("ORDER tailscale {web_port}"),
            format!("ORDER openssh {api_port}"),
        ]
    );
    assert!(
        fake.tailscale_state_exists(web_port),
        "earlier Tailscale success was rolled back"
    );
    assert!(
        !fake.tailscale_state_exists(db_port),
        "later Tailscale publication was executed"
    );
    assert_eq!(fake.master_count()?, 1);
    fake.assert_no_rollback_or_delete()?;
    project.assert_unchanged()?;
    fake.cleanup()?;
    project.cleanup()?;
    drop((web_backend, api_backend, db_backend));
    Ok(())
}

const FAKE_TAILSCALE: &[u8] = br#"#!/bin/sh
set -eu

printf 'CALL tailscale %s\n' "$*" >> "$AEQUIMUTA_APPLY_FAKE_LOG"

if [ "$#" -eq 2 ] && [ "$1" = "status" ] && [ "$2" = "--json" ]; then
    printf '{"BackendState":"Running","Self":{"DNSName":"apply-test.example.ts.net.","Online":true},"CurrentTailnet":{"MagicDNSEnabled":true}}\n'
    exit 0
fi

if [ "$#" -eq 3 ] && [ "$1" = "serve" ] && [ "$2" = "status" ] && [ "$3" = "--json" ]; then
    printf '{"TCP":{'
    separator=
    for marker in "$AEQUIMUTA_APPLY_FAKE_STATE"/tailscale-*; do
        if [ ! -f "$marker" ]; then
            continue
        fi
        name=${marker##*/}
        port=${name#tailscale-}
        printf '%s"%s":{"TCPForward":"127.0.0.1:%s"}' "$separator" "$port" "$port"
        separator=,
    done
    printf '}}\n'
    exit 0
fi

if [ "$#" -eq 4 ] && [ "$1" = "serve" ] && [ "$2" = "--bg" ]; then
    tcp_flag=$3
    port=${tcp_flag#--tcp=}
    if [ "$tcp_flag" != "--tcp=$port" ] || [ "$4" != "tcp://127.0.0.1:$port" ]; then
        printf 'unexpected fake Tailscale mutation\n' >&2
        exit 98
    fi

    printf 'ORDER tailscale %s\n' "$port" >> "$AEQUIMUTA_APPLY_FAKE_LOG"
    if [ "$AEQUIMUTA_APPLY_FAIL_TAILSCALE_PORT" = "$port" ]; then
        printf 'super-secret provider diagnostic\n/private/tailscale-token\n' >&2
        exit 42
    fi

    : > "$AEQUIMUTA_APPLY_FAKE_STATE/tailscale-$port"
    exit 0
fi

printf 'unexpected fake tailscale command: %s\n' "$*" >&2
exit 99
"#;

const FAKE_SSH: &[u8] = br#"#!/bin/sh
set -eu

printf 'CALL ssh %s\n' "$*" >> "$AEQUIMUTA_APPLY_FAKE_LOG"

case " $* " in
    *" -G "*)
        printf 'controlpath %s\n' "$AEQUIMUTA_APPLY_FAKE_REPORTED_CONTROL"
        exit 0
        ;;
    *" -O check "*)
        if [ -f "$AEQUIMUTA_APPLY_FAKE_MASTER_STATE" ]; then
            IFS= read -r master_pid < "$AEQUIMUTA_APPLY_FAKE_MASTER_STATE"
            printf 'Master running (pid=%s)\r\n' "$master_pid" >&2
            exit 0
        fi
        printf 'fake master is not live\n' >&2
        exit 255
        ;;
    *" -O forward "*)
        previous=
        forward=
        for argument do
            if [ "$previous" = "-R" ]; then
                forward=$argument
                break
            fi
            previous=$argument
        done
        local_port=${forward##*:}
        printf 'ORDER openssh %s\n' "$local_port" >> "$AEQUIMUTA_APPLY_FAKE_LOG"
        if [ "$AEQUIMUTA_APPLY_FAIL_OPENSSH_LOCAL_PORT" = "$local_port" ]; then
            printf 'super-secret provider diagnostic\n/private/test-key\n' >&2
            exit 43
        fi
        exit 0
        ;;
esac

printf 'MASTER openssh\n' >> "$AEQUIMUTA_APPLY_FAKE_LOG"
printf '%s\n' "$$" > "$AEQUIMUTA_APPLY_FAKE_MASTER_STATE"
printf '%s\n' "$$" >> "$AEQUIMUTA_APPLY_FAKE_MASTER_PIDS"

attempt=0
while [ ! -S "$AEQUIMUTA_APPLY_FAKE_CONTROL" ]; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 400 ]; then
        printf 'fake control socket was not created\n' >&2
        exit 44
    fi
    /bin/sleep 0.005
done

while [ ! -f "$AEQUIMUTA_APPLY_FAKE_MASTER_STOP" ]; do
    /bin/sleep 0.01
done
exit 0
"#;
