use std::collections::HashSet;
use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const SSH: &str = "/usr/bin/ssh";
const SSHD: &str = "/usr/sbin/sshd";
const SSH_KEYGEN: &str = "/usr/bin/ssh-keygen";
const PUBLISHER: &str = "openssh-reverse-tcp";
const DECLARATION_FILE: &str = "aequimuta.toml";
const PUBLISHING_FILE: &str = "aequimuta.publish.toml";
const PROVIDER_FILE: &str = "aequimuta.openssh-reverse-tcp.toml";
const FORWARD_FAILURE: &[u8] = b"error: OpenSSH remote forwarding request was not acknowledged\n";
const WAIT_TIMEOUT: Duration = Duration::from_secs(5);

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

struct ShortDirectory {
    path: PathBuf,
    cleaned: bool,
}

impl ShortDirectory {
    fn new() -> io::Result<Self> {
        loop {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let process = std::process::id() & 0x00ff_ffff;
            let path = env::temp_dir().join(format!("a{process:06x}{:02x}", sequence & 0xff));

            let mut builder = DirBuilder::new();
            builder.mode(0o700);

            match builder.create(&path) {
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

    fn cleanup(&mut self) -> io::Result<()> {
        if self.cleaned {
            return Ok(());
        }

        fs::remove_dir_all(&self.path)?;
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for ShortDirectory {
    fn drop(&mut self) {
        if !self.cleaned
            && let Err(error) = fs::remove_dir_all(&self.path)
        {
            eprintln!(
                "failed to clean up OpenSSH E2E directory {}: {error}",
                self.path.display()
            );
        }
    }
}

struct EchoServer {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<io::Result<()>>>,
}

impl EchoServer {
    fn start() -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let address = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || echo_loop(listener, &thread_stop));

        Ok(Self {
            address,
            stop,
            thread: Some(thread),
        })
    }

    fn port(&self) -> u16 {
        self.address.port()
    }

    fn round_trip(&self, payload: &[u8]) -> io::Result<()> {
        round_trip(self.address, payload)
    }

    fn stop(&mut self) -> io::Result<()> {
        self.stop.store(true, Ordering::Release);

        let Some(thread) = self.thread.take() else {
            return Ok(());
        };

        match thread.join() {
            Ok(result) => result,
            Err(_) => Err(io::Error::other("echo server thread panicked")),
        }
    }
}

impl Drop for EchoServer {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("failed to stop OpenSSH E2E echo server: {error}");
        }
    }
}

struct RunningSshd {
    child: Option<Child>,
}

impl RunningSshd {
    fn start(configuration: &Path, log: &Path) -> io::Result<Self> {
        let stderr = OpenOptions::new().create_new(true).write(true).open(log)?;
        let child = Command::new(SSHD)
            .arg("-D")
            .arg("-e")
            .arg("-f")
            .arg(configuration)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .spawn()?;

        Ok(Self { child: Some(child) })
    }

    fn ensure_running(&mut self) -> io::Result<()> {
        let Some(child) = self.child.as_mut() else {
            return Err(io::Error::other("isolated sshd is not running"));
        };

        match child.try_wait()? {
            None => Ok(()),
            Some(status) => Err(io::Error::other(format!(
                "isolated sshd exited unexpectedly with {status}"
            ))),
        }
    }

    fn stop(&mut self) -> io::Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };

        if child.try_wait()?.is_none() {
            child.kill()?;
        }
        let _ = child.wait()?;
        Ok(())
    }
}

impl Drop for RunningSshd {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("failed to stop isolated sshd: {error}");
        }
    }
}

struct ProjectSnapshot {
    directory: PathBuf,
    files: Vec<(PathBuf, Vec<u8>)>,
    entries: Vec<OsString>,
}

impl ProjectSnapshot {
    fn capture(directory: &Path) -> io::Result<Self> {
        let files = [DECLARATION_FILE, PUBLISHING_FILE, PROVIDER_FILE]
            .into_iter()
            .map(|name| {
                let path = directory.join(name);
                fs::read(&path).map(|contents| (path, contents))
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            directory: directory.to_path_buf(),
            files,
            entries: directory_entries(directory)?,
        })
    }

    fn assert_unchanged(&self) -> io::Result<()> {
        for (path, expected) in &self.files {
            check(
                fs::read(path)? == *expected,
                format!("publish changed project file {}", path.display()),
            )?;
        }

        check(
            directory_entries(&self.directory)? == self.entries,
            "publish changed project directory entries",
        )
    }
}

struct Fixture {
    root: ShortDirectory,
    project: PathBuf,
    runtime: PathBuf,
    wrapper_directory: PathBuf,
    home: PathBuf,
    client_log: PathBuf,
    sshd_log: PathBuf,
    username: String,
    sshd_port: u16,
    success_listener_port: u16,
    conflict_listener_port: u16,
    rejected_listener_port: u16,
    web_backend: EchoServer,
    conflict_backend: EchoServer,
    rejected_backend: EchoServer,
    sentinel: EchoServer,
    sshd: RunningSshd,
    control_path: Option<PathBuf>,
}

impl Fixture {
    fn start() -> TestResult<Self> {
        check_tool(SSH)?;
        check_tool(SSHD)?;
        check_tool(SSH_KEYGEN)?;

        let root = ShortDirectory::new()?;
        let project = create_private_directory(root.path().join("p"))?;
        let runtime = create_private_directory(root.path().join("r"))?;
        let wrapper_directory = create_private_directory(root.path().join("b"))?;
        let home = create_private_directory(root.path().join("h"))?;
        let key_directory = create_private_directory(root.path().join("k"))?;

        let username = current_username()?;
        let web_backend = EchoServer::start()?;
        let conflict_backend = EchoServer::start()?;
        let rejected_backend = EchoServer::start()?;
        let sentinel = EchoServer::start()?;
        let conflict_listener_port = sentinel.port();

        let mut used_ports = HashSet::from([
            web_backend.port(),
            conflict_backend.port(),
            rejected_backend.port(),
            conflict_listener_port,
        ]);
        let sshd_port = reserve_distinct_port(&mut used_ports)?;
        let success_listener_port = reserve_distinct_port(&mut used_ports)?;
        let rejected_listener_port = reserve_distinct_port(&mut used_ports)?;

        let host_key = key_directory.join("host");
        let client_key = key_directory.join("client");
        generate_key(&host_key)?;
        generate_key(&client_key)?;

        let authorized_keys = key_directory.join("authorized_keys");
        fs::copy(client_key.with_extension("pub"), &authorized_keys)?;
        set_mode(&authorized_keys, 0o600)?;

        let known_hosts = key_directory.join("known_hosts");
        write_known_hosts(&host_key, sshd_port, &known_hosts)?;
        set_mode(&known_hosts, 0o600)?;

        let client_configuration = root.path().join("c");
        write_client_configuration(&client_configuration, &client_key, &known_hosts)?;
        set_mode(&client_configuration, 0o600)?;
        let client_log = root.path().join("e");
        write_ssh_wrapper(
            &wrapper_directory.join("ssh"),
            &client_configuration,
            &client_log,
        )?;

        write_project(
            &project,
            &username,
            sshd_port,
            [
                ("web", web_backend.port(), success_listener_port),
                ("conflict", conflict_backend.port(), conflict_listener_port),
                ("rejected", rejected_backend.port(), rejected_listener_port),
            ],
        )?;

        let sshd_configuration = root.path().join("d");
        let sshd_log = root.path().join("l");
        write_validated_sshd_configuration(
            &sshd_configuration,
            root.path(),
            &host_key,
            &authorized_keys,
            &username,
            sshd_port,
            success_listener_port,
            conflict_listener_port,
        )?;
        let mut sshd = RunningSshd::start(&sshd_configuration, &sshd_log)?;
        wait_for_sshd(&mut sshd, sshd_port)?;

        Ok(Self {
            root,
            project,
            runtime,
            wrapper_directory,
            home,
            client_log,
            sshd_log,
            username,
            sshd_port,
            success_listener_port,
            conflict_listener_port,
            rejected_listener_port,
            web_backend,
            conflict_backend,
            rejected_backend,
            sentinel,
            sshd,
            control_path: None,
        })
    }

    fn exercise(&mut self) -> TestResult {
        let snapshot = ProjectSnapshot::capture(&self.project)?;
        let success_stdout = format!(
            "Ensured web via {PUBLISHER}: {}@127.0.0.1:{} listen 127.0.0.1:{} -> 127.0.0.1:{} (SSH-session-backed; no automatic reconnect)\n",
            self.username,
            self.sshd_port,
            self.success_listener_port,
            self.web_backend.port()
        );

        let first = self.publish("web")?;
        if !first.status.success() {
            return Err(io::Error::other(format!(
                "initial publish failed: {first:?}; client log: {}; sshd log: {}",
                read_diagnostic(&self.client_log),
                read_diagnostic(&self.sshd_log)
            ))
            .into());
        }
        assert_success(&first, success_stdout.as_bytes(), "initial publish")?;
        snapshot.assert_unchanged()?;

        let control_path = self.find_control_path()?;
        self.control_path = Some(control_path.clone());
        validate_runtime_state(&self.runtime, &control_path)?;
        let master_before_repeat = check_master(&control_path)?;
        check(
            count_control_sockets(&self.runtime)? == 1,
            "initial publish did not create exactly one control socket",
        )?;
        wait_for_listener_count(self.success_listener_port, 1)?;
        round_trip(
            SocketAddr::from((Ipv4Addr::LOCALHOST, self.success_listener_port)),
            b"openssh-e2e-initial-nonce",
        )?;

        let repeated = self.publish("web")?;
        assert_success(&repeated, success_stdout.as_bytes(), "repeat publish")?;
        snapshot.assert_unchanged()?;
        check(
            check_master(&control_path)? == master_before_repeat,
            "repeat publish replaced the dedicated ControlMaster",
        )?;
        check(
            count_control_sockets(&self.runtime)? == 1,
            "repeat publish created another control socket",
        )?;
        wait_for_listener_count(self.success_listener_port, 1)?;
        round_trip(
            SocketAddr::from((Ipv4Addr::LOCALHOST, self.success_listener_port)),
            b"openssh-e2e-repeat-nonce",
        )?;

        self.sentinel.round_trip(b"sentinel-before-conflict")?;
        wait_for_listener_count(self.conflict_listener_port, 1)?;
        let conflict = self.publish("conflict")?;
        assert_failure(&conflict, FORWARD_FAILURE, "remote bind conflict")?;
        snapshot.assert_unchanged()?;
        self.sentinel.round_trip(b"sentinel-after-conflict")?;
        wait_for_listener_count(self.conflict_listener_port, 1)?;
        round_trip(
            SocketAddr::from((Ipv4Addr::LOCALHOST, self.success_listener_port)),
            b"success-survives-conflict",
        )?;

        let rejected = self.publish("rejected")?;
        assert_failure(&rejected, FORWARD_FAILURE, "PermitListen rejection")?;
        snapshot.assert_unchanged()?;
        wait_for_listener_count(self.rejected_listener_port, 0)?;
        wait_for_listener_count(self.success_listener_port, 1)?;
        round_trip(
            SocketAddr::from((Ipv4Addr::LOCALHOST, self.success_listener_port)),
            b"success-survives-policy-rejection",
        )?;
        check(
            check_master(&control_path)? == master_before_repeat,
            "provider failures replaced the dedicated ControlMaster",
        )?;
        check(
            count_control_sockets(&self.runtime)? == 1,
            "provider failures changed the control socket count",
        )?;

        self.exit_master()?;
        wait_for_listener_count(self.success_listener_port, 0)?;
        wait_until("control socket removal", || Ok(!control_path.exists()))?;
        check(
            TcpStream::connect_timeout(
                &SocketAddr::from((Ipv4Addr::LOCALHOST, self.success_listener_port)),
                Duration::from_millis(200),
            )
            .is_err(),
            "remote listener remained reachable after the SSH master exited",
        )?;
        self.sentinel.round_trip(b"sentinel-after-master-exit")?;
        snapshot.assert_unchanged()?;
        self.sshd.ensure_running()?;

        Ok(())
    }

    fn exercise_apply(&mut self) -> TestResult {
        fs::write(
            self.project.join(PUBLISHING_FILE),
            format!("[[publications]]\nservice = \"web\"\npublisher = \"{PUBLISHER}\"\n"),
        )?;
        let snapshot = ProjectSnapshot::capture(&self.project)?;
        let success_stdout = format!(
            "Ensured web via {PUBLISHER}: {}@127.0.0.1:{} listen 127.0.0.1:{} -> 127.0.0.1:{} (SSH-session-backed; no automatic reconnect)\nApplied 1 desired publications\n",
            self.username,
            self.sshd_port,
            self.success_listener_port,
            self.web_backend.port()
        );

        let first = self.apply()?;
        if !first.status.success() {
            return Err(io::Error::other(format!(
                "initial apply failed: {first:?}; client log: {}; sshd log: {}",
                read_diagnostic(&self.client_log),
                read_diagnostic(&self.sshd_log)
            ))
            .into());
        }
        assert_success(&first, success_stdout.as_bytes(), "initial apply")?;
        snapshot.assert_unchanged()?;

        let control_path = self.find_control_path()?;
        self.control_path = Some(control_path.clone());
        validate_runtime_state(&self.runtime, &control_path)?;
        let master_before_repeat = check_master(&control_path)?;
        check(
            count_control_sockets(&self.runtime)? == 1,
            "initial apply did not create exactly one control socket",
        )?;
        wait_for_listener_count(self.success_listener_port, 1)?;
        round_trip(
            SocketAddr::from((Ipv4Addr::LOCALHOST, self.success_listener_port)),
            b"openssh-apply-e2e-initial-nonce",
        )?;

        let repeated = self.apply()?;
        assert_success(&repeated, success_stdout.as_bytes(), "repeat apply")?;
        snapshot.assert_unchanged()?;
        check(
            check_master(&control_path)? == master_before_repeat,
            "repeat apply replaced the dedicated ControlMaster",
        )?;
        check(
            count_control_sockets(&self.runtime)? == 1,
            "repeat apply created another control socket",
        )?;
        wait_for_listener_count(self.success_listener_port, 1)?;
        round_trip(
            SocketAddr::from((Ipv4Addr::LOCALHOST, self.success_listener_port)),
            b"openssh-apply-e2e-repeat-nonce",
        )?;
        self.sshd.ensure_running()?;

        Ok(())
    }

    fn publish(&self, service: &str) -> io::Result<Output> {
        Command::new(env!("CARGO_BIN_EXE_aequimuta"))
            .arg("publish")
            .arg(service)
            .arg(PUBLISHER)
            .current_dir(&self.project)
            .env("PATH", &self.wrapper_directory)
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("HOME", &self.home)
            .env("LC_ALL", "C")
            .env_remove("SSH_AUTH_SOCK")
            .env_remove("SSH_AGENT_PID")
            .env_remove("SSH_ASKPASS")
            .env_remove("SSH_ASKPASS_REQUIRE")
            .env_remove("DISPLAY")
            .output()
    }

    fn apply(&self) -> io::Result<Output> {
        Command::new(env!("CARGO_BIN_EXE_aequimuta"))
            .arg("apply")
            .current_dir(&self.project)
            .env("PATH", &self.wrapper_directory)
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("HOME", &self.home)
            .env("LC_ALL", "C")
            .env_remove("SSH_AUTH_SOCK")
            .env_remove("SSH_AGENT_PID")
            .env_remove("SSH_ASKPASS")
            .env_remove("SSH_ASKPASS_REQUIRE")
            .env_remove("DISPLAY")
            .output()
    }

    fn find_control_path(&self) -> io::Result<PathBuf> {
        let directory = self.runtime.join("aequimuta").join("openssh-reverse-tcp");
        let sockets = socket_paths(&directory)?;

        check(
            sockets.len() == 1,
            format!("expected one control socket, found {sockets:?}"),
        )?;

        sockets
            .into_iter()
            .next()
            .ok_or_else(|| io::Error::other("control socket disappeared"))
    }

    fn exit_master(&mut self) -> io::Result<()> {
        let Some(control_path) = self.control_path.as_ref() else {
            return Ok(());
        };

        if !control_path.exists() {
            self.control_path = None;
            return Err(io::Error::other(
                "test-owned ControlMaster socket disappeared before cleanup",
            ));
        }

        let output = Command::new(SSH)
            .arg("-F")
            .arg("none")
            .arg("-S")
            .arg(control_path)
            .arg("-O")
            .arg("exit")
            .arg("-o")
            .arg("ControlMaster=no")
            .arg("127.0.0.1")
            .output()?;

        check(
            output.status.success() && output.stdout.is_empty(),
            format!("failed to exit test-owned ControlMaster: {output:?}"),
        )?;
        self.control_path = None;
        Ok(())
    }

    fn cleanup(&mut self) -> io::Result<()> {
        let mut first_error = None;

        if self.control_path.is_none() {
            match discover_control_path(&self.runtime) {
                Ok(control_path) => self.control_path = control_path,
                Err(error) => first_error = Some(error),
            }
        }
        if self.control_path.is_some() {
            record_cleanup_error(&mut first_error, self.exit_master());
        }
        record_cleanup_error(&mut first_error, self.sshd.stop());
        record_cleanup_error(&mut first_error, self.web_backend.stop());
        record_cleanup_error(&mut first_error, self.conflict_backend.stop());
        record_cleanup_error(&mut first_error, self.rejected_backend.stop());
        record_cleanup_error(&mut first_error, self.sentinel.stop());
        record_cleanup_error(&mut first_error, self.root.cleanup());

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

fn echo_loop(listener: TcpListener, stop: &AtomicBool) -> io::Result<()> {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_read_timeout(Some(Duration::from_millis(200)))?;
                let mut buffer = [0_u8; 4096];

                loop {
                    match stream.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(length) => stream.write_all(&buffer[..length])?,
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                            ) =>
                        {
                            if stop.load(Ordering::Acquire) {
                                break;
                            }
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }

    Ok(())
}

fn round_trip(address: SocketAddr, payload: &[u8]) -> io::Result<()> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(1))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(payload)?;

    let mut response = vec![0_u8; payload.len()];
    stream.read_exact(&mut response)?;
    let _ = stream.shutdown(Shutdown::Both);

    check(
        response == payload,
        format!("unexpected TCP round-trip payload from {address}"),
    )
}

fn check_tool(path: &str) -> io::Result<()> {
    check(
        Path::new(path).is_file(),
        format!("required OpenSSH executable is unavailable: {path}"),
    )
}

fn create_private_directory(path: PathBuf) -> io::Result<PathBuf> {
    let mut builder = DirBuilder::new();
    builder.mode(0o700);
    builder.create(&path)?;
    set_mode(&path, 0o700)?;
    Ok(path)
}

fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions)
}

fn generate_key(path: &Path) -> io::Result<()> {
    let output = Command::new(SSH_KEYGEN)
        .arg("-q")
        .arg("-t")
        .arg("ed25519")
        .arg("-N")
        .arg("")
        .arg("-f")
        .arg(path)
        .output()?;

    check(
        output.status.success(),
        format!("ssh-keygen failed for {}: {output:?}", path.display()),
    )
}

fn write_known_hosts(host_key: &Path, port: u16, path: &Path) -> io::Result<()> {
    let public_key = fs::read_to_string(host_key.with_extension("pub"))?;
    let mut fields = public_key.split_whitespace();
    let key_type = fields
        .next()
        .ok_or_else(|| io::Error::other("generated host public key has no type"))?;
    let key = fields
        .next()
        .ok_or_else(|| io::Error::other("generated host public key has no body"))?;

    fs::write(path, format!("[127.0.0.1]:{port} {key_type} {key}\n"))
}

fn write_client_configuration(
    path: &Path,
    client_key: &Path,
    known_hosts: &Path,
) -> io::Result<()> {
    fs::write(
        path,
        format!(
            "Host *\n\
             \tIdentityFile {}\n\
             \tIdentitiesOnly yes\n\
             \tUserKnownHostsFile {}\n\
             \tGlobalKnownHostsFile none\n\
             \tStrictHostKeyChecking yes\n\
             \tPasswordAuthentication no\n\
             \tKbdInteractiveAuthentication no\n\
             \tGSSAPIAuthentication no\n\
             \tHostbasedAuthentication no\n\
             \tUpdateHostKeys no\n\
             \tAddKeysToAgent no\n",
            client_key.display(),
            known_hosts.display()
        ),
    )
}

fn write_ssh_wrapper(
    path: &Path,
    client_configuration: &Path,
    client_log: &Path,
) -> io::Result<()> {
    fs::write(
        path,
        format!(
            "#!/bin/sh\nexec {SSH} -E '{}' -o LogLevel=VERBOSE -F '{}' \"$@\"\n",
            shell_single_quoted(client_log),
            shell_single_quoted(client_configuration),
        ),
    )?;
    set_mode(path, 0o700)
}

fn shell_single_quoted(path: &Path) -> String {
    path.as_os_str().to_string_lossy().replace('\'', "'\\''")
}

fn read_diagnostic(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| format!("<unavailable: {error}>"))
}

fn write_project(
    project: &Path,
    username: &str,
    sshd_port: u16,
    services: [(&str, u16, u16); 3],
) -> io::Result<()> {
    let mut declaration = String::new();
    let mut publishing = String::new();
    let mut provider = String::new();

    for (service, backend_port, listener_port) in services {
        declaration.push_str(&format!(
            "[[services]]\nname = \"{service}\"\nport = {backend_port}\n\n"
        ));
        publishing.push_str(&format!(
            "[[publications]]\nservice = \"{service}\"\npublisher = \"{PUBLISHER}\"\n\n"
        ));
        provider.push_str(&format!(
            "[[publications]]\n\
             service = \"{service}\"\n\
             host = \"127.0.0.1\"\n\
             user = \"{username}\"\n\
             ssh_port = {sshd_port}\n\
             listen_address = \"127.0.0.1\"\n\
             listen_port = {listener_port}\n\n"
        ));
    }

    fs::write(project.join(DECLARATION_FILE), declaration)?;
    fs::write(project.join(PUBLISHING_FILE), publishing)?;
    fs::write(project.join(PROVIDER_FILE), provider)
}

#[allow(clippy::too_many_arguments)]
fn write_validated_sshd_configuration(
    path: &Path,
    root: &Path,
    host_key: &Path,
    authorized_keys: &Path,
    username: &str,
    sshd_port: u16,
    success_listener_port: u16,
    conflict_listener_port: u16,
) -> io::Result<()> {
    write_sshd_configuration(
        path,
        "none",
        host_key,
        authorized_keys,
        username,
        sshd_port,
        success_listener_port,
        conflict_listener_port,
    )?;
    let none_validation = validate_sshd_configuration(path)?;

    if none_validation.status.success() {
        return Ok(());
    }

    let pid_file = root.join("sshd.pid");
    write_sshd_configuration(
        path,
        &pid_file.to_string_lossy(),
        host_key,
        authorized_keys,
        username,
        sshd_port,
        success_listener_port,
        conflict_listener_port,
    )?;
    let fallback_validation = validate_sshd_configuration(path)?;

    check(
        fallback_validation.status.success(),
        format!(
            "sshd rejected both `PidFile none` and the fixture-local fallback; none={none_validation:?}, fallback={fallback_validation:?}"
        ),
    )
}

#[allow(clippy::too_many_arguments)]
fn write_sshd_configuration(
    path: &Path,
    pid_file: &str,
    host_key: &Path,
    authorized_keys: &Path,
    username: &str,
    sshd_port: u16,
    success_listener_port: u16,
    conflict_listener_port: u16,
) -> io::Result<()> {
    fs::write(
        path,
        format!(
            "AddressFamily inet\n\
             Port {sshd_port}\n\
             ListenAddress 127.0.0.1\n\
             HostKey {}\n\
             PidFile {pid_file}\n\
             AuthorizedKeysFile {}\n\
             StrictModes no\n\
             PubkeyAuthentication yes\n\
             AuthenticationMethods publickey\n\
             PasswordAuthentication no\n\
             KbdInteractiveAuthentication no\n\
             UsePAM no\n\
             AllowUsers {username}\n\
             AllowTcpForwarding remote\n\
             AllowStreamLocalForwarding no\n\
             DisableForwarding no\n\
             GatewayPorts clientspecified\n\
             PermitListen 127.0.0.1:{success_listener_port} 127.0.0.1:{conflict_listener_port}\n\
             AllowAgentForwarding no\n\
             X11Forwarding no\n\
             PermitTTY no\n\
             PermitTunnel no\n\
             PermitUserRC no\n\
             PermitUserEnvironment no\n\
             MaxSessions 0\n\
             PerSourcePenalties no\n\
             PrintMotd no\n\
             PrintLastLog no\n\
             UseDNS no\n\
             LogLevel VERBOSE\n",
            host_key.display(),
            authorized_keys.display()
        ),
    )
}

fn validate_sshd_configuration(path: &Path) -> io::Result<Output> {
    Command::new(SSHD).arg("-t").arg("-f").arg(path).output()
}

fn wait_for_sshd(sshd: &mut RunningSshd, port: u16) -> io::Result<()> {
    wait_until("isolated sshd startup", || {
        sshd.ensure_running()?;
        Ok(TcpStream::connect_timeout(
            &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            Duration::from_millis(100),
        )
        .is_ok())
    })
}

fn reserve_distinct_port(used: &mut HashSet<u16>) -> io::Result<u16> {
    loop {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        drop(listener);

        if used.insert(port) {
            return Ok(port);
        }
    }
}

fn current_username() -> io::Result<String> {
    let uid = fs::metadata("/proc/self")?.uid();
    let passwd = fs::read_to_string("/etc/passwd")?;

    for line in passwd.lines() {
        let fields: Vec<&str> = line.split(':').collect();

        if fields.len() >= 3 && fields[2].parse::<u32>().ok() == Some(uid) {
            return Ok(fields[0].to_owned());
        }
    }

    Err(io::Error::other(format!(
        "current uid {uid} has no /etc/passwd entry"
    )))
}

fn directory_entries(path: &Path) -> io::Result<Vec<OsString>> {
    let mut entries = fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort();
    Ok(entries)
}

fn validate_runtime_state(runtime: &Path, control_path: &Path) -> io::Result<()> {
    for directory in [
        runtime.to_path_buf(),
        runtime.join("aequimuta"),
        runtime.join("aequimuta").join("openssh-reverse-tcp"),
    ] {
        let metadata = fs::symlink_metadata(&directory)?;
        check(
            metadata.file_type().is_dir() && metadata.mode() & 0o777 == 0o700,
            format!(
                "runtime directory is not owner-only: {}",
                directory.display()
            ),
        )?;
    }

    let metadata = fs::symlink_metadata(control_path)?;
    check(
        metadata.file_type().is_socket() && metadata.mode() & 0o077 == 0,
        format!(
            "control path is not a private Unix socket: {}",
            control_path.display()
        ),
    )
}

fn socket_paths(directory: &Path) -> io::Result<Vec<PathBuf>> {
    let mut sockets = Vec::new();

    for entry in fs::read_dir(directory)? {
        let entry = entry?;

        if entry.file_type()?.is_socket() {
            sockets.push(entry.path());
        }
    }

    sockets.sort();
    Ok(sockets)
}

fn count_control_sockets(runtime: &Path) -> io::Result<usize> {
    socket_paths(&runtime.join("aequimuta").join("openssh-reverse-tcp"))
        .map(|sockets| sockets.len())
}

fn discover_control_path(runtime: &Path) -> io::Result<Option<PathBuf>> {
    let directory = runtime.join("aequimuta").join("openssh-reverse-tcp");

    if !directory.exists() {
        return Ok(None);
    }

    let mut sockets = socket_paths(&directory)?;
    check(
        sockets.len() <= 1,
        format!("multiple test-runtime control sockets found: {sockets:?}"),
    )?;
    Ok(sockets.pop())
}

fn check_master(control_path: &Path) -> io::Result<Vec<u8>> {
    let output = Command::new(SSH)
        .arg("-F")
        .arg("none")
        .arg("-S")
        .arg(control_path)
        .arg("-O")
        .arg("check")
        .arg("-o")
        .arg("ControlMaster=no")
        .arg("127.0.0.1")
        .output()?;

    check(
        output.status.success()
            && output.stdout.is_empty()
            && output.stderr.starts_with(b"Master running (pid="),
        format!("test-owned ControlMaster is not live: {output:?}"),
    )?;

    Ok(output.stderr)
}

fn ipv4_listener_count(port: u16) -> io::Result<usize> {
    let source = fs::read_to_string("/proc/net/tcp")?;
    let expected_address = format!("0100007F:{port:04X}");
    let mut count = 0;

    for line in source.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();

        if fields.get(1) == Some(&expected_address.as_str()) && fields.get(3) == Some(&"0A") {
            count += 1;
        }
    }

    Ok(count)
}

fn wait_for_listener_count(port: u16, expected: usize) -> io::Result<()> {
    wait_until(
        &format!("IPv4 listener count {expected} for 127.0.0.1:{port}"),
        || Ok(ipv4_listener_count(port)? == expected),
    )
}

fn wait_until<F>(label: &str, mut condition: F) -> io::Result<()>
where
    F: FnMut() -> io::Result<bool>,
{
    let deadline = Instant::now() + WAIT_TIMEOUT;

    loop {
        if condition()? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out waiting for {label}"),
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn assert_success(output: &Output, expected_stdout: &[u8], label: &str) -> io::Result<()> {
    check(
        output.status.code() == Some(0)
            && output.stdout == expected_stdout
            && output.stderr.is_empty(),
        format!("{label} did not satisfy the CLI success contract: {output:?}"),
    )
}

fn assert_failure(output: &Output, expected_stderr: &[u8], label: &str) -> io::Result<()> {
    check(
        output.status.code() == Some(1)
            && output.stdout.is_empty()
            && output.stderr == expected_stderr,
        format!("{label} did not satisfy the CLI failure contract: {output:?}"),
    )
}

fn record_cleanup_error(first_error: &mut Option<io::Error>, result: io::Result<()>) {
    if first_error.is_none()
        && let Err(error) = result
    {
        *first_error = Some(error);
    }
}

fn check(condition: bool, message: impl Into<String>) -> io::Result<()> {
    if condition {
        Ok(())
    } else {
        Err(io::Error::other(message.into()))
    }
}

#[test]
fn openssh_reverse_tcp_real_session_lifecycle_is_non_destructive() -> TestResult {
    let mut fixture = Fixture::start()?;
    let exercise_result = match fixture.exercise() {
        Ok(()) => fixture.exercise_apply(),
        Err(error) => Err(error),
    };
    let cleanup_result = fixture.cleanup();

    match (exercise_result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(exercise_error), Ok(())) => Err(exercise_error),
        (Ok(()), Err(cleanup_error)) => Err(Box::new(cleanup_error)),
        (Err(exercise_error), Err(cleanup_error)) => Err(io::Error::other(format!(
            "OpenSSH E2E failed: {exercise_error}; cleanup also failed: {cleanup_error}"
        ))
        .into()),
    }
}
