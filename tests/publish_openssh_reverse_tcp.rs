#[allow(dead_code)]
mod support;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::net::TcpListener;
use std::os::unix::fs::{FileTypeExt, PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use support::TestDirectory;

const DECLARATION_FILE: &str = "aequimuta.toml";
const PUBLISHING_FILE: &str = "aequimuta.publish.toml";
const PROVIDER_FILE: &str = "aequimuta.openssh-reverse-tcp.toml";
const OPENSSH_PUBLISHER: &str = "openssh-reverse-tcp";
const TAILSCALE_PUBLISHER: &str = "tailscale-serve-tcp";
const TEST_HOST: &str = "edge.example.com";
const TEST_USER: &str = "aequimuta";
const TEST_SSH_PORT: u16 = 22;
const TEST_LISTEN_ADDRESS: &str = "0.0.0.0";
const TEST_LISTEN_PORT: u16 = 18_080;
const OPENSSH_CONTROL_FILE: &str = "cm-0123456789abcdef0123456789abcdef01234567";
const FNV1A_64_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV1A_64_PRIME: u64 = 0x00000100000001b3;

type TestResult = Result<(), Box<dyn std::error::Error>>;

static NEXT_RUNTIME_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct ShortRuntimeDirectory {
    path: PathBuf,
    cleaned: bool,
}

impl ShortRuntimeDirectory {
    fn new() -> io::Result<Self> {
        loop {
            let sequence = NEXT_RUNTIME_DIRECTORY.fetch_add(1, Ordering::Relaxed) & 0x3ff;
            let identifier = (u64::from(std::process::id()) & 0x3f_ffff) << 10 | sequence;
            let path = Path::new("/tmp").join(format!("ar{identifier:08x}"));

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

    fn cleanup(mut self) -> io::Result<()> {
        fs::remove_dir_all(&self.path)?;
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for ShortRuntimeDirectory {
    fn drop(&mut self) {
        if !self.cleaned
            && let Err(error) = fs::remove_dir_all(&self.path)
        {
            eprintln!(
                "failed to clean up short runtime directory {}: {error}",
                self.path.display()
            );
        }
    }
}

struct Project {
    directory: TestDirectory,
    declaration: Vec<u8>,
    publishing: Vec<u8>,
    provider: Option<Vec<u8>>,
    entries: Vec<OsString>,
}

impl Project {
    fn new(
        label: &str,
        declaration: &[u8],
        publishing: &[u8],
        provider: Option<&[u8]>,
    ) -> io::Result<Self> {
        let directory = TestDirectory::new(label)?;
        fs::write(directory.path().join(DECLARATION_FILE), declaration)?;
        fs::write(directory.path().join(PUBLISHING_FILE), publishing)?;

        if let Some(provider) = provider {
            fs::write(directory.path().join(PROVIDER_FILE), provider)?;
        }

        Ok(Self {
            entries: directory_entries(directory.path())?,
            directory,
            declaration: declaration.to_vec(),
            publishing: publishing.to_vec(),
            provider: provider.map(<[u8]>::to_vec),
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

        match &self.provider {
            Some(provider) => assert_eq!(
                fs::read(self.path().join(PROVIDER_FILE))?,
                *provider,
                "OpenSSH provider configuration changed"
            ),
            None => assert!(
                !self.path().join(PROVIDER_FILE).exists(),
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

struct SocketEmulator {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<io::Result<()>>>,
}

impl SocketEmulator {
    fn start(master_pids: PathBuf, expected_masters: usize, control_path: PathBuf) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while recorded_pids(&master_pids)?.len() < expected_masters {
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
            eprintln!("failed to stop fake control socket: {error}");
        }
    }
}

struct FakeSsh {
    directory: TestDirectory,
    log: PathBuf,
    state: PathBuf,
    stop: PathBuf,
    exited: PathBuf,
    master_pids: PathBuf,
    openssh_control_path: PathBuf,
    control_path: PathBuf,
    mode: &'static str,
    socket_emulator: Option<SocketEmulator>,
    seeded_master: Option<Child>,
}

impl FakeSsh {
    fn new(label: &str, runtime: &Path, mode: &'static str) -> io::Result<Self> {
        Self::new_for_destination(label, runtime, mode, TEST_HOST, TEST_USER, TEST_SSH_PORT)
    }

    fn new_for_destination(
        label: &str,
        runtime: &Path,
        mode: &'static str,
        host: &str,
        user: &str,
        ssh_port: u16,
    ) -> io::Result<Self> {
        let directory = TestDirectory::new(label)?;
        let executable = directory.path().join("ssh");
        let log = directory.path().join("calls.log");
        let state = directory.path().join("master-live");
        let stop = directory.path().join("stop-master");
        let exited = directory.path().join("master-exited");
        let master_pids = directory.path().join("master-pids.log");
        let openssh_control_path = provider_runtime_directory(runtime).join(OPENSSH_CONTROL_FILE);
        let control_path = expected_control_path(runtime, host, user, ssh_port);

        fs::write(&executable, FAKE_SSH)?;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))?;

        Ok(Self {
            directory,
            log,
            state,
            stop,
            exited,
            master_pids,
            openssh_control_path,
            control_path,
            mode,
            socket_emulator: None,
            seeded_master: None,
        })
    }

    fn path(&self) -> &Path {
        self.directory.path()
    }

    fn environment(&self, runtime: &Path) -> Vec<(&'static str, OsString)> {
        vec![
            ("PATH", self.path().as_os_str().to_owned()),
            ("XDG_RUNTIME_DIR", runtime.as_os_str().to_owned()),
            ("AEQUIMUTA_FAKE_MODE", OsString::from(self.mode)),
            ("AEQUIMUTA_FAKE_LOG", self.log.as_os_str().to_owned()),
            ("AEQUIMUTA_FAKE_STATE", self.state.as_os_str().to_owned()),
            ("AEQUIMUTA_FAKE_STOP", self.stop.as_os_str().to_owned()),
            ("AEQUIMUTA_FAKE_EXITED", self.exited.as_os_str().to_owned()),
            (
                "AEQUIMUTA_FAKE_MASTER_PIDS",
                self.master_pids.as_os_str().to_owned(),
            ),
            (
                "AEQUIMUTA_FAKE_OPENSSH_CONTROL",
                self.openssh_control_path.as_os_str().to_owned(),
            ),
            (
                "AEQUIMUTA_FAKE_CONTROL",
                self.control_path.as_os_str().to_owned(),
            ),
        ]
    }

    fn enable_master_creation(&mut self) {
        self.enable_master_creation_count(1);
    }

    fn enable_master_creation_count(&mut self, expected_masters: usize) {
        self.socket_emulator = Some(SocketEmulator::start(
            self.master_pids.clone(),
            expected_masters,
            self.control_path.clone(),
        ));
    }

    fn make_master_live(&mut self, runtime: &Path) -> io::Result<()> {
        prepare_provider_runtime_directory(runtime)?;
        self.enable_master_creation();
        let mut command = Command::new(self.path().join("ssh"));
        command
            .envs(self.environment(runtime))
            .env("AEQUIMUTA_FAKE_SEED_MASTER", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        self.seeded_master = Some(command.spawn()?);
        wait_for_socket(&self.control_path)
    }

    fn master_pids(&self) -> io::Result<Vec<u32>> {
        recorded_pids(&self.master_pids)
    }

    fn calls(&self) -> io::Result<Vec<Vec<String>>> {
        let source = match fs::read_to_string(&self.log) {
            Ok(source) => source,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut calls = Vec::new();
        let mut current: Option<Vec<String>> = None;

        for line in source.lines() {
            match line {
                "BEGIN" if current.is_none() => current = Some(Vec::new()),
                "END" => {
                    let call = current.take().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "orphan fake call terminator")
                    })?;
                    calls.push(call);
                }
                argument => {
                    let call = current.as_mut().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "orphan fake call argument")
                    })?;
                    call.push(argument.to_owned());
                }
            }
        }

        if current.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unterminated fake call",
            ));
        }

        Ok(calls)
    }

    fn cleanup(mut self) -> io::Result<()> {
        self.stop_master_processes()?;
        if let Some(mut emulator) = self.socket_emulator.take() {
            emulator.stop()?;
        }
        self.directory.cleanup()
    }

    fn cleanup_reaped_master(mut self) -> io::Result<()> {
        let pids = self.master_pids()?;

        if pids.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fake master PID was not recorded",
            ));
        }
        if pids
            .iter()
            .any(|pid| Path::new("/proc").join(pid.to_string()).exists())
        {
            return Err(io::Error::other(
                "fake master process was not reaped by the CLI",
            ));
        }
        if let Some(mut emulator) = self.socket_emulator.take() {
            emulator.stop()?;
        }
        self.directory.cleanup()
    }

    fn stop_master_processes(&mut self) -> io::Result<()> {
        let pids = self.master_pids()?;

        if pids.is_empty() {
            return Ok(());
        }

        let winner_pid = state_pid(&self.state)?;
        if !pids.contains(&winner_pid) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "live fake master PID was not recorded",
            ));
        }

        fs::write(&self.stop, b"stop\n")?;

        if let Some(mut child) = self.seeded_master.take() {
            let status = child.wait()?;
            if !status.success() {
                return Err(io::Error::other("seeded fake master failed"));
            }
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        for pid in pids {
            while Path::new("/proc").join(pid.to_string()).exists() {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("fake master process {pid} did not exit"),
                    ));
                }
                thread::sleep(Duration::from_millis(5));
            }
        }

        if !recorded_pids(&self.exited)?.contains(&winner_pid) {
            return Err(io::Error::other(
                "winning fake master did not acknowledge graceful cleanup",
            ));
        }

        Ok(())
    }
}

fn private_runtime_directory() -> io::Result<ShortRuntimeDirectory> {
    ShortRuntimeDirectory::new()
}

fn provider_runtime_directory(runtime: &Path) -> PathBuf {
    runtime.join("aequimuta").join("openssh-reverse-tcp")
}

fn control_path_template(runtime: &Path) -> PathBuf {
    provider_runtime_directory(runtime).join("cm-%C")
}

fn expected_control_path(runtime: &Path, host: &str, user: &str, ssh_port: u16) -> PathBuf {
    let mut hash = FNV1A_64_OFFSET_BASIS;

    for byte in host
        .as_bytes()
        .iter()
        .copied()
        .chain(std::iter::once(0))
        .chain(user.as_bytes().iter().copied())
        .chain(std::iter::once(0))
        .chain(ssh_port.to_be_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV1A_64_PRIME);
    }

    let file_name = format!("cm-{hash:016x}{}", &OPENSSH_CONTROL_FILE[19..]);
    provider_runtime_directory(runtime).join(file_name)
}

fn prepare_provider_runtime_directory(runtime: &Path) -> io::Result<()> {
    let aequimuta = runtime.join("aequimuta");
    if !aequimuta.exists() {
        fs::create_dir(&aequimuta)?;
    }
    fs::set_permissions(&aequimuta, fs::Permissions::from_mode(0o700))?;

    let provider = aequimuta.join("openssh-reverse-tcp");
    if !provider.exists() {
        fs::create_dir(&provider)?;
    }
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o700))
}

fn wait_for_socket(path: &Path) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);

    loop {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_socket() => return Ok(()),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fake control path is not a Unix socket",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "fake control socket was not created",
            ));
        }
        thread::sleep(Duration::from_millis(5));
    }
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

fn state_pid(path: &Path) -> io::Result<u32> {
    let source = fs::read_to_string(path)?;
    let trimmed = source.trim_end_matches(['\r', '\n']);
    let pid = trimmed
        .parse::<u32>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid live master PID"))?;

    if pid == 0 || pid.to_string() != trimmed {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "non-canonical live master PID",
        ));
    }
    Ok(pid)
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

fn spawn_aequimuta_with_env<I, K, V>(
    directory: &Path,
    args: &[&str],
    environment: I,
) -> io::Result<Child>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    Command::new(env!("CARGO_BIN_EXE_aequimuta"))
        .args(args)
        .current_dir(directory)
        .envs(environment)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
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

fn provider_configuration(
    service: &str,
    host: &str,
    user: &str,
    ssh_port: u16,
    listen_address: &str,
    listen_port: u16,
) -> Vec<u8> {
    format!(
        "[[publications]]\n\
         service = \"{service}\"\n\
         host = \"{host}\"\n\
         user = \"{user}\"\n\
         ssh_port = {ssh_port}\n\
         listen_address = \"{listen_address}\"\n\
         listen_port = {listen_port}\n"
    )
    .into_bytes()
}

fn canonical_provider_configuration(service: &str) -> Vec<u8> {
    provider_configuration(
        service,
        TEST_HOST,
        TEST_USER,
        TEST_SSH_PORT,
        TEST_LISTEN_ADDRESS,
        TEST_LISTEN_PORT,
    )
}

fn assert_operation_failure(output: &Output, expected_stderr: &[u8], label: &str) {
    assert_eq!(
        output.status.code(),
        Some(1),
        "{label}: unexpected exit status"
    );
    assert!(output.stdout.is_empty(), "{label}: unexpected stdout");
    assert_eq!(output.stderr, expected_stderr, "{label}: unexpected stderr");
    assert_eq!(
        output.stderr.iter().filter(|&&byte| byte == b'\n').count(),
        1,
        "{label}: stderr is not exactly one line"
    );
}

fn expected_success_stdout(service: &str, local_port: u16) -> Vec<u8> {
    expected_success_stdout_for(
        service,
        local_port,
        TEST_HOST,
        TEST_USER,
        TEST_SSH_PORT,
        TEST_LISTEN_ADDRESS,
        TEST_LISTEN_PORT,
    )
}

fn expected_success_stdout_for(
    service: &str,
    local_port: u16,
    host: &str,
    user: &str,
    ssh_port: u16,
    listen_address: &str,
    listen_port: u16,
) -> Vec<u8> {
    format!(
        "Ensured {service} via {OPENSSH_PUBLISHER}: \
         {user}@{host}:{ssh_port} listen \
         {listen_address}:{listen_port} -> 127.0.0.1:{local_port} \
         (SSH-session-backed; no automatic reconnect)\n"
    )
    .into_bytes()
}

fn call_has_pair(call: &[String], first: &str, second: &str) -> bool {
    call.windows(2)
        .any(|arguments| arguments[0] == first && arguments[1] == second)
}

fn call_has_option(call: &[String], option: &str) -> bool {
    call_has_pair(call, "-o", option)
}

fn is_expansion_call(call: &[String]) -> bool {
    call.iter().any(|argument| argument == "-G")
}

fn is_control_call(call: &[String], operation: &str) -> bool {
    call_has_pair(call, "-O", operation)
}

fn is_master_creation_call(call: &[String]) -> bool {
    !is_expansion_call(call) && !is_control_call(call, "check") && !is_control_call(call, "forward")
}

fn assert_master_creation_call(
    call: &[String],
    host: &str,
    user: &str,
    ssh_port: u16,
    control_path: &Path,
) {
    for argument in ["-4", "-M", "-N", "-n", "-T"] {
        assert!(
            call.iter().any(|actual| actual == argument),
            "master creation omitted {argument}: {call:?}"
        );
    }
    for option in [
        "BatchMode=yes",
        "StrictHostKeyChecking=yes",
        "ClearAllForwardings=yes",
        "ForwardAgent=no",
        "ForwardX11=no",
        "RequestTTY=no",
        "SessionType=none",
        "RemoteCommand=none",
        "CanonicalizeHostname=no",
        "ProxyCommand=none",
        "ProxyJump=none",
        "ControlMaster=yes",
        "ControlPersist=no",
        "ForkAfterAuthentication=no",
        "PermitLocalCommand=no",
        "LocalCommand=none",
        "Tunnel=no",
        "UpdateHostKeys=no",
        "AddKeysToAgent=no",
    ] {
        assert!(
            call_has_option(call, option),
            "master creation omitted pinned option {option}: {call:?}"
        );
    }
    assert!(call_has_option(call, &format!("HostName={host}")));
    assert!(call_has_pair(call, "-S", &control_path.to_string_lossy()));
    assert!(call_has_pair(call, "-l", user));
    assert!(call_has_pair(call, "-p", &ssh_port.to_string()));
    assert!(
        !call.iter().any(|argument| argument == "-f"),
        "master creation requested OpenSSH self-backgrounding: {call:?}"
    );
    assert!(
        !call.iter().any(|argument| argument == "-R"),
        "master creation included a remote forward: {call:?}"
    );
}

fn assert_expansion_call(
    call: &[String],
    host: &str,
    user: &str,
    ssh_port: u16,
    control_path_template: &Path,
) {
    assert!(call.iter().any(|argument| argument == "-G"));
    assert!(call_has_pair(call, "-F", "none"));
    assert!(call.iter().any(|argument| argument == "-4"));
    assert!(call_has_pair(
        call,
        "-S",
        &control_path_template.to_string_lossy()
    ));
    assert!(call_has_pair(call, "-l", user));
    assert!(call_has_pair(call, "-p", &ssh_port.to_string()));
    for option in [
        "CanonicalizeHostname=no",
        "ProxyCommand=none",
        "ProxyJump=none",
    ] {
        assert!(
            call_has_option(call, option),
            "control-path expansion omitted {option}: {call:?}"
        );
    }
    assert!(call_has_option(call, &format!("HostName={host}")));
    assert!(
        !call.iter().any(|argument| argument == "-R"),
        "control-path expansion requested a forward: {call:?}"
    );
}

fn assert_check_call(call: &[String], host: &str, user: &str, ssh_port: u16, control_path: &Path) {
    assert!(call_has_pair(call, "-F", "none"));
    assert!(call.iter().any(|argument| argument == "-4"));
    assert!(call_has_option(call, "ControlMaster=no"));
    assert!(call_has_option(call, &format!("HostName={host}")));
    assert!(call_has_pair(call, "-S", &control_path.to_string_lossy()));
    assert!(call_has_pair(call, "-l", user));
    assert!(call_has_pair(call, "-p", &ssh_port.to_string()));
    assert!(is_control_call(call, "check"));
    assert!(
        !call.iter().any(|argument| argument == "-R"),
        "master check requested a forward: {call:?}"
    );
}

struct ExpectedForward<'a> {
    host: &'a str,
    user: &'a str,
    ssh_port: u16,
    listen_address: &'a str,
    listen_port: u16,
    local_port: u16,
    control_path: &'a Path,
}

fn assert_forward_call(call: &[String], expected: &ExpectedForward<'_>) {
    assert!(call_has_pair(call, "-F", "none"));
    assert!(call.iter().any(|argument| argument == "-4"));
    assert!(call_has_option(call, "ControlMaster=no"));
    assert!(call_has_option(
        call,
        &format!("HostName={}", expected.host)
    ));
    assert!(call_has_pair(
        call,
        "-S",
        &expected.control_path.to_string_lossy()
    ));
    assert!(call_has_pair(call, "-l", expected.user));
    assert!(call_has_pair(call, "-p", &expected.ssh_port.to_string()));
    assert!(is_control_call(call, "forward"));
    assert!(call_has_pair(
        call,
        "-R",
        &format!(
            "{}:{}:127.0.0.1:{}",
            expected.listen_address, expected.listen_port, expected.local_port
        )
    ));
    assert!(
        !call_has_option(call, "ClearAllForwardings=yes"),
        "control forward erased its own -R request: {call:?}"
    );
}

fn assert_single_created_call_set(
    calls: &[Vec<String>],
    runtime: &Path,
    control_path: &Path,
    expected: &ExpectedForward<'_>,
) {
    assert_eq!(
        calls.iter().filter(|call| is_expansion_call(call)).count(),
        1
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| is_master_creation_call(call))
            .count(),
        1
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| is_control_call(call, "check"))
            .count(),
        1
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| is_control_call(call, "forward"))
            .count(),
        1
    );

    for expansion in calls.iter().filter(|call| is_expansion_call(call)) {
        assert_expansion_call(
            expansion,
            expected.host,
            expected.user,
            expected.ssh_port,
            &control_path_template(runtime),
        );
    }
    for master in calls.iter().filter(|call| is_master_creation_call(call)) {
        assert_master_creation_call(
            master,
            expected.host,
            expected.user,
            expected.ssh_port,
            control_path,
        );
    }
    for check in calls.iter().filter(|call| is_control_call(call, "check")) {
        assert_check_call(
            check,
            expected.host,
            expected.user,
            expected.ssh_port,
            control_path,
        );
    }
    for forward in calls.iter().filter(|call| is_control_call(call, "forward")) {
        assert_forward_call(forward, expected);
    }
}

#[test]
fn selection_and_unsupported_paths_do_not_read_provider_configuration_or_invoke_ssh() -> TestResult
{
    let runtime = private_runtime_directory()?;
    let absent_project = Project::new(
        "openssh-tuple-absent",
        &service_declaration("web", 8_080),
        &publishing_intent("web", "example-publisher"),
        None,
    )?;
    let absent_fake = FakeSsh::new("openssh-tuple-absent-fake", runtime.path(), "success")?;
    let absent_output = run_aequimuta_with_env(
        absent_project.path(),
        &["publish", "web", OPENSSH_PUBLISHER],
        absent_fake.environment(runtime.path()),
    )?;

    assert_operation_failure(
        &absent_output,
        b"error: selected publication is not in desired state\n",
        "desired tuple absent",
    );
    assert!(absent_fake.calls()?.is_empty());
    absent_project.assert_unchanged()?;
    absent_fake.cleanup()?;
    absent_project.cleanup()?;

    let unsupported_project = Project::new(
        "openssh-unsupported",
        &service_declaration("web", 8_080),
        &publishing_intent("web", "example-publisher"),
        Some(b"this is not valid TOML = ["),
    )?;
    let unsupported_fake = FakeSsh::new("openssh-unsupported-fake", runtime.path(), "success")?;
    let unsupported_output = run_aequimuta_with_env(
        unsupported_project.path(),
        &["publish", "web", "example-publisher"],
        unsupported_fake.environment(runtime.path()),
    )?;

    assert_operation_failure(
        &unsupported_output,
        b"error: selected publisher is not supported\n",
        "unsupported publisher",
    );
    assert!(unsupported_fake.calls()?.is_empty());
    unsupported_project.assert_unchanged()?;
    unsupported_fake.cleanup()?;
    unsupported_project.cleanup()?;
    runtime.cleanup()?;
    Ok(())
}

#[test]
fn provider_configuration_is_required_strict_semantic_and_unique_by_service() -> TestResult {
    const INVALID_CONFIGURATION: &[u8] =
        b"error: aequimuta.openssh-reverse-tcp.toml is not a valid OpenSSH reverse TCP configuration\n";
    let runtime = private_runtime_directory()?;
    let declaration = service_declaration("web", 8_080);
    let publishing = publishing_intent("web", OPENSSH_PUBLISHER);

    let missing_project =
        Project::new("openssh-provider-missing", &declaration, &publishing, None)?;
    let missing_fake = FakeSsh::new("openssh-provider-missing-fake", runtime.path(), "success")?;
    let missing_output = run_aequimuta_with_env(
        missing_project.path(),
        &["publish", "web", OPENSSH_PUBLISHER],
        missing_fake.environment(runtime.path()),
    )?;
    assert_operation_failure(
        &missing_output,
        b"error: failed to read aequimuta.openssh-reverse-tcp.toml\n",
        "missing provider configuration",
    );
    assert!(missing_fake.calls()?.is_empty());
    missing_project.assert_unchanged()?;
    missing_fake.cleanup()?;
    missing_project.cleanup()?;

    let invalid_cases: &[(&str, Vec<u8>)] = &[
        (
            "unknown-root-field",
            b"future = true\n\
              [[publications]]\n\
              service = \"web\"\n\
              host = \"edge.example.com\"\n\
              user = \"aequimuta\"\n\
              ssh_port = 22\n\
              listen_address = \"127.0.0.1\"\n\
              listen_port = 18080\n"
                .to_vec(),
        ),
        (
            "unknown-publication-field",
            b"[[publications]]\n\
              service = \"web\"\n\
              host = \"edge.example.com\"\n\
              user = \"aequimuta\"\n\
              ssh_port = 22\n\
              listen_address = \"127.0.0.1\"\n\
              listen_port = 18080\n\
              identity_file = \"/private/key\"\n"
                .to_vec(),
        ),
        (
            "option-like-host",
            provider_configuration(
                "web",
                "-edge.example.com",
                TEST_USER,
                22,
                "127.0.0.1",
                18_080,
            ),
        ),
        (
            "whitespace-user",
            provider_configuration("web", TEST_HOST, "bad user", 22, "127.0.0.1", 18_080),
        ),
        (
            "zero-ssh-port",
            provider_configuration("web", TEST_HOST, TEST_USER, 0, "127.0.0.1", 18_080),
        ),
        (
            "non-ipv4-listen-address",
            provider_configuration("web", TEST_HOST, TEST_USER, 22, "localhost", 18_080),
        ),
        (
            "ipv6-listen-address",
            provider_configuration("web", TEST_HOST, TEST_USER, 22, "::1", 18_080),
        ),
        (
            "privileged-listen-port",
            provider_configuration("web", TEST_HOST, TEST_USER, 22, "127.0.0.1", 1_023),
        ),
    ];

    for (label, provider) in invalid_cases {
        let project = Project::new(label, &declaration, &publishing, Some(provider))?;
        let fake = FakeSsh::new(label, runtime.path(), "success")?;
        let output = run_aequimuta_with_env(
            project.path(),
            &["publish", "web", OPENSSH_PUBLISHER],
            fake.environment(runtime.path()),
        )?;

        assert_operation_failure(&output, INVALID_CONFIGURATION, label);
        assert!(fake.calls()?.is_empty(), "{label}: ssh was invoked");
        project.assert_unchanged()?;
        fake.cleanup()?;
        project.cleanup()?;
    }

    let first = canonical_provider_configuration("web");
    let second = provider_configuration(
        "web",
        "other.example.com",
        TEST_USER,
        2222,
        "127.0.0.1",
        19_090,
    );
    let duplicate = [first, b"\n".to_vec(), second].concat();
    let duplicate_project = Project::new(
        "openssh-provider-duplicate",
        &declaration,
        &publishing,
        Some(&duplicate),
    )?;
    let duplicate_fake =
        FakeSsh::new("openssh-provider-duplicate-fake", runtime.path(), "success")?;
    let duplicate_output = run_aequimuta_with_env(
        duplicate_project.path(),
        &["publish", "web", OPENSSH_PUBLISHER],
        duplicate_fake.environment(runtime.path()),
    )?;

    assert_operation_failure(
        &duplicate_output,
        INVALID_CONFIGURATION,
        "duplicate provider service",
    );
    assert!(duplicate_fake.calls()?.is_empty());
    duplicate_project.assert_unchanged()?;
    duplicate_fake.cleanup()?;
    duplicate_project.cleanup()?;

    let unresolved_project = Project::new(
        "openssh-provider-unresolved",
        &declaration,
        &publishing,
        Some(b"# no provider publications\n"),
    )?;
    let unresolved_fake = FakeSsh::new(
        "openssh-provider-unresolved-fake",
        runtime.path(),
        "success",
    )?;
    let unresolved_output = run_aequimuta_with_env(
        unresolved_project.path(),
        &["publish", "web", OPENSSH_PUBLISHER],
        unresolved_fake.environment(runtime.path()),
    )?;
    assert_operation_failure(
        &unresolved_output,
        b"error: desired OpenSSH reverse TCP publication has no provider configuration\n",
        "unresolved provider service",
    );
    assert!(unresolved_fake.calls()?.is_empty());
    unresolved_project.assert_unchanged()?;
    unresolved_fake.cleanup()?;
    unresolved_project.cleanup()?;
    runtime.cleanup()?;
    Ok(())
}

#[test]
fn desired_remote_slot_ambiguity_is_rejected_before_ssh_even_with_different_bind_addresses()
-> TestResult {
    let runtime = private_runtime_directory()?;
    let declaration = b"[[services]]\n\
                        name = \"web\"\n\
                        port = 8080\n\
                        \n\
                        [[services]]\n\
                        name = \"api\"\n\
                        port = 9090\n";
    let publishing = b"[[publications]]\n\
                       service = \"web\"\n\
                       publisher = \"openssh-reverse-tcp\"\n\
                       \n\
                       [[publications]]\n\
                       service = \"api\"\n\
                       publisher = \"openssh-reverse-tcp\"\n";
    let provider = b"[[publications]]\n\
                     service = \"web\"\n\
                     host = \"edge.example.com\"\n\
                     user = \"aequimuta\"\n\
                     ssh_port = 22\n\
                     listen_address = \"127.0.0.1\"\n\
                     listen_port = 18080\n\
                     \n\
                     [[publications]]\n\
                     service = \"api\"\n\
                     host = \"edge.example.com\"\n\
                     user = \"aequimuta\"\n\
                     ssh_port = 22\n\
                     listen_address = \"0.0.0.0\"\n\
                     listen_port = 18080\n";
    let project = Project::new(
        "openssh-remote-slot-ambiguity",
        declaration,
        publishing,
        Some(provider),
    )?;
    let fake = FakeSsh::new(
        "openssh-remote-slot-ambiguity-fake",
        runtime.path(),
        "success",
    )?;
    let output = run_aequimuta_with_env(
        project.path(),
        &["publish", "web", OPENSSH_PUBLISHER],
        fake.environment(runtime.path()),
    )?;

    assert_operation_failure(
        &output,
        b"error: desired OpenSSH reverse TCP publications conflict\n",
        "remote-slot ambiguity",
    );
    assert!(fake.calls()?.is_empty());
    project.assert_unchanged()?;
    fake.cleanup()?;
    project.cleanup()?;
    runtime.cleanup()?;
    Ok(())
}

#[test]
fn backend_runtime_and_executable_preflights_fail_before_ssh_mutation() -> TestResult {
    let closed_listener = TcpListener::bind(("127.0.0.1", 0))?;
    let closed_port = closed_listener.local_addr()?.port();
    drop(closed_listener);
    let runtime = private_runtime_directory()?;
    let closed_project = Project::new(
        "openssh-backend-unreachable",
        &service_declaration("web", closed_port),
        &publishing_intent("web", OPENSSH_PUBLISHER),
        Some(&canonical_provider_configuration("web")),
    )?;
    let closed_fake = FakeSsh::new(
        "openssh-backend-unreachable-fake",
        runtime.path(),
        "success",
    )?;
    let closed_output = run_aequimuta_with_env(
        closed_project.path(),
        &["publish", "web", OPENSSH_PUBLISHER],
        closed_fake.environment(runtime.path()),
    )?;

    assert_operation_failure(
        &closed_output,
        format!("error: local TCP backend 127.0.0.1:{closed_port} is not reachable\n").as_bytes(),
        "unreachable backend",
    );
    assert!(closed_fake.calls()?.is_empty());
    assert!(directory_entries(runtime.path())?.is_empty());
    closed_project.assert_unchanged()?;
    closed_fake.cleanup()?;
    closed_project.cleanup()?;
    runtime.cleanup()?;

    let backend = TcpListener::bind(("127.0.0.1", 0))?;
    let port = backend.local_addr()?.port();
    let declaration = service_declaration("web", port);
    let publishing = publishing_intent("web", OPENSSH_PUBLISHER);
    let provider = canonical_provider_configuration("web");
    let missing_runtime_project = Project::new(
        "openssh-runtime-missing",
        &declaration,
        &publishing,
        Some(&provider),
    )?;
    let fake_directory = TestDirectory::new("openssh-runtime-missing-fake")?;
    let fake_executable = fake_directory.path().join("ssh");
    fs::write(&fake_executable, FAKE_SSH)?;
    fs::set_permissions(&fake_executable, fs::Permissions::from_mode(0o700))?;
    let missing_runtime_output = Command::new(env!("CARGO_BIN_EXE_aequimuta"))
        .args(["publish", "web", OPENSSH_PUBLISHER])
        .current_dir(missing_runtime_project.path())
        .env("PATH", fake_directory.path())
        .env_remove("XDG_RUNTIME_DIR")
        .output()?;
    assert_operation_failure(
        &missing_runtime_output,
        b"error: XDG_RUNTIME_DIR is not safe for OpenSSH reverse TCP control state\n",
        "missing XDG_RUNTIME_DIR",
    );
    assert!(
        !fake_directory.path().join("calls.log").exists(),
        "missing runtime invoked ssh"
    );
    missing_runtime_project.assert_unchanged()?;
    fake_directory.cleanup()?;
    missing_runtime_project.cleanup()?;

    let unsafe_runtime = private_runtime_directory()?;
    fs::set_permissions(unsafe_runtime.path(), fs::Permissions::from_mode(0o755))?;
    let unsafe_project = Project::new(
        "openssh-runtime-unsafe",
        &declaration,
        &publishing,
        Some(&provider),
    )?;
    let unsafe_fake = FakeSsh::new(
        "openssh-runtime-unsafe-fake",
        unsafe_runtime.path(),
        "success",
    )?;
    let unsafe_output = run_aequimuta_with_env(
        unsafe_project.path(),
        &["publish", "web", OPENSSH_PUBLISHER],
        unsafe_fake.environment(unsafe_runtime.path()),
    )?;
    assert_operation_failure(
        &unsafe_output,
        b"error: XDG_RUNTIME_DIR is not safe for OpenSSH reverse TCP control state\n",
        "unsafe XDG_RUNTIME_DIR",
    );
    assert!(unsafe_fake.calls()?.is_empty());
    unsafe_project.assert_unchanged()?;
    unsafe_fake.cleanup()?;
    unsafe_project.cleanup()?;
    unsafe_runtime.cleanup()?;

    let symlink_target = private_runtime_directory()?;
    let real_xdg_runtime = symlink_target.path().join("xdg");
    fs::create_dir(&real_xdg_runtime)?;
    fs::set_permissions(&real_xdg_runtime, fs::Permissions::from_mode(0o700))?;
    let symlink_container = private_runtime_directory()?;
    let intermediate_symlink = symlink_container.path().join("intermediate");
    symlink(symlink_target.path(), &intermediate_symlink)?;
    let lexical_xdg_runtime = intermediate_symlink.join("xdg");
    assert_ne!(fs::canonicalize(&lexical_xdg_runtime)?, lexical_xdg_runtime);
    let symlink_project = Project::new(
        "openssh-runtime-intermediate-symlink",
        &declaration,
        &publishing,
        Some(&provider),
    )?;
    let symlink_fake = FakeSsh::new(
        "openssh-runtime-intermediate-symlink-fake",
        &lexical_xdg_runtime,
        "success",
    )?;
    let symlink_output = run_aequimuta_with_env(
        symlink_project.path(),
        &["publish", "web", OPENSSH_PUBLISHER],
        symlink_fake.environment(&lexical_xdg_runtime),
    )?;
    assert_operation_failure(
        &symlink_output,
        b"error: XDG_RUNTIME_DIR is not safe for OpenSSH reverse TCP control state\n",
        "intermediate runtime symlink",
    );
    assert!(symlink_fake.calls()?.is_empty());
    symlink_project.assert_unchanged()?;
    symlink_fake.cleanup()?;
    symlink_project.cleanup()?;
    fs::remove_file(&intermediate_symlink)?;
    symlink_container.cleanup()?;
    symlink_target.cleanup()?;

    let executable_runtime = private_runtime_directory()?;
    let executable_project = Project::new(
        "openssh-executable-missing",
        &declaration,
        &publishing,
        Some(&provider),
    )?;
    let empty_path = TestDirectory::new("openssh-empty-path")?;
    let executable_output = run_aequimuta_with_env(
        executable_project.path(),
        &["publish", "web", OPENSSH_PUBLISHER],
        [
            ("PATH", empty_path.path().as_os_str()),
            ("XDG_RUNTIME_DIR", executable_runtime.path().as_os_str()),
        ],
    )?;
    assert_operation_failure(
        &executable_output,
        b"error: ssh executable is not available\n",
        "missing ssh executable",
    );
    executable_project.assert_unchanged()?;
    empty_path.cleanup()?;
    executable_project.cleanup()?;
    executable_runtime.cleanup()?;
    drop(backend);
    Ok(())
}

#[test]
fn absent_master_is_created_with_pinned_options_and_repeat_uses_one_master() -> TestResult {
    let backend = TcpListener::bind(("127.0.0.1", 0))?;
    let local_port = backend.local_addr()?.port();
    let runtime = private_runtime_directory()?;
    let project = Project::new(
        "openssh-master-create-repeat",
        &service_declaration("web", local_port),
        &publishing_intent("web", OPENSSH_PUBLISHER),
        Some(&canonical_provider_configuration("web")),
    )?;
    let mut fake = FakeSsh::new(
        "openssh-master-create-repeat-fake",
        runtime.path(),
        "success",
    )?;
    fake.enable_master_creation();

    for iteration in 1..=2 {
        let output = run_aequimuta_with_env(
            project.path(),
            &["publish", "web", OPENSSH_PUBLISHER],
            fake.environment(runtime.path()),
        )?;
        assert_eq!(
            output.status.code(),
            Some(0),
            "iteration {iteration}: unexpected status"
        );
        assert_eq!(
            output.stdout,
            expected_success_stdout("web", local_port),
            "iteration {iteration}: unexpected stdout"
        );
        assert!(
            output.stderr.is_empty(),
            "iteration {iteration}: unexpected stderr"
        );
    }

    let calls = fake.calls()?;
    assert_eq!(
        calls.iter().filter(|call| is_expansion_call(call)).count(),
        2
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| is_master_creation_call(call))
            .count(),
        1
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| is_control_call(call, "check"))
            .count(),
        2
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| is_control_call(call, "forward"))
            .count(),
        2
    );

    let master = calls
        .iter()
        .find(|call| is_master_creation_call(call))
        .ok_or("master creation call was not recorded")?;
    assert_master_creation_call(
        master,
        TEST_HOST,
        TEST_USER,
        TEST_SSH_PORT,
        &fake.control_path,
    );
    for expansion in calls.iter().filter(|call| is_expansion_call(call)) {
        assert_expansion_call(
            expansion,
            TEST_HOST,
            TEST_USER,
            TEST_SSH_PORT,
            &control_path_template(runtime.path()),
        );
    }
    for check in calls.iter().filter(|call| is_control_call(call, "check")) {
        assert_check_call(
            check,
            TEST_HOST,
            TEST_USER,
            TEST_SSH_PORT,
            &fake.control_path,
        );
    }
    let expected_forward = ExpectedForward {
        host: TEST_HOST,
        user: TEST_USER,
        ssh_port: TEST_SSH_PORT,
        listen_address: TEST_LISTEN_ADDRESS,
        listen_port: TEST_LISTEN_PORT,
        local_port,
        control_path: &fake.control_path,
    };
    for forward in calls.iter().filter(|call| is_control_call(call, "forward")) {
        assert_forward_call(forward, &expected_forward);
    }

    let control_metadata = fs::symlink_metadata(&fake.control_path)?;
    assert!(control_metadata.file_type().is_socket());
    assert_eq!(control_metadata.permissions().mode() & 0o077, 0);
    assert_eq!(
        fs::metadata(runtime.path().join("aequimuta"))?
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(provider_runtime_directory(runtime.path()))?
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        directory_entries(&provider_runtime_directory(runtime.path()))?
            .iter()
            .filter(|entry| Some(entry.as_os_str()) == fake.control_path.file_name())
            .count(),
        1
    );
    project.assert_unchanged()?;

    fake.cleanup()?;
    project.cleanup()?;
    runtime.cleanup()?;
    drop(backend);
    Ok(())
}

#[test]
fn lexical_host_case_uses_distinct_composite_control_socket_identities() -> TestResult {
    const UPPER_HOST: &str = "EDGE.EXAMPLE.COM";

    let backend = TcpListener::bind(("127.0.0.1", 0))?;
    let local_port = backend.local_addr()?.port();
    let runtime = private_runtime_directory()?;
    let declaration = service_declaration("web", local_port);
    let publishing = publishing_intent("web", OPENSSH_PUBLISHER);

    let lower_project = Project::new(
        "openssh-lexical-host-lower",
        &declaration,
        &publishing,
        Some(&canonical_provider_configuration("web")),
    )?;
    let mut lower_fake = FakeSsh::new_for_destination(
        "openssh-lexical-host-lower-fake",
        runtime.path(),
        "success",
        TEST_HOST,
        TEST_USER,
        TEST_SSH_PORT,
    )?;
    lower_fake.enable_master_creation();

    let upper_provider = provider_configuration(
        "web",
        UPPER_HOST,
        TEST_USER,
        TEST_SSH_PORT,
        TEST_LISTEN_ADDRESS,
        TEST_LISTEN_PORT,
    );
    let upper_project = Project::new(
        "openssh-lexical-host-upper",
        &declaration,
        &publishing,
        Some(&upper_provider),
    )?;
    let mut upper_fake = FakeSsh::new_for_destination(
        "openssh-lexical-host-upper-fake",
        runtime.path(),
        "success",
        UPPER_HOST,
        TEST_USER,
        TEST_SSH_PORT,
    )?;
    upper_fake.enable_master_creation();

    let lower_output = run_aequimuta_with_env(
        lower_project.path(),
        &["publish", "web", OPENSSH_PUBLISHER],
        lower_fake.environment(runtime.path()),
    )?;
    let upper_output = run_aequimuta_with_env(
        upper_project.path(),
        &["publish", "web", OPENSSH_PUBLISHER],
        upper_fake.environment(runtime.path()),
    )?;

    assert_eq!(lower_output.status.code(), Some(0));
    assert_eq!(
        lower_output.stdout,
        expected_success_stdout("web", local_port)
    );
    assert!(lower_output.stderr.is_empty());
    assert_eq!(upper_output.status.code(), Some(0));
    assert_eq!(
        upper_output.stdout,
        expected_success_stdout_for(
            "web",
            local_port,
            UPPER_HOST,
            TEST_USER,
            TEST_SSH_PORT,
            TEST_LISTEN_ADDRESS,
            TEST_LISTEN_PORT,
        )
    );
    assert!(upper_output.stderr.is_empty());

    assert_eq!(
        lower_fake.openssh_control_path, upper_fake.openssh_control_path,
        "the fake OpenSSH %C base should be identical for this regression"
    );
    assert_ne!(
        lower_fake.control_path, upper_fake.control_path,
        "lexically distinct configured hosts shared a control socket identity"
    );
    assert_eq!(
        lower_fake.control_path,
        expected_control_path(runtime.path(), TEST_HOST, TEST_USER, TEST_SSH_PORT)
    );
    assert_eq!(
        upper_fake.control_path,
        expected_control_path(runtime.path(), UPPER_HOST, TEST_USER, TEST_SSH_PORT)
    );
    assert!(
        fs::symlink_metadata(&lower_fake.control_path)?
            .file_type()
            .is_socket()
    );
    assert!(
        fs::symlink_metadata(&upper_fake.control_path)?
            .file_type()
            .is_socket()
    );

    let lower_expected = ExpectedForward {
        host: TEST_HOST,
        user: TEST_USER,
        ssh_port: TEST_SSH_PORT,
        listen_address: TEST_LISTEN_ADDRESS,
        listen_port: TEST_LISTEN_PORT,
        local_port,
        control_path: &lower_fake.control_path,
    };
    assert_single_created_call_set(
        &lower_fake.calls()?,
        runtime.path(),
        &lower_fake.control_path,
        &lower_expected,
    );
    let upper_expected = ExpectedForward {
        host: UPPER_HOST,
        user: TEST_USER,
        ssh_port: TEST_SSH_PORT,
        listen_address: TEST_LISTEN_ADDRESS,
        listen_port: TEST_LISTEN_PORT,
        local_port,
        control_path: &upper_fake.control_path,
    };
    assert_single_created_call_set(
        &upper_fake.calls()?,
        runtime.path(),
        &upper_fake.control_path,
        &upper_expected,
    );
    lower_project.assert_unchanged()?;
    upper_project.assert_unchanged()?;

    lower_fake.cleanup()?;
    upper_fake.cleanup()?;
    lower_project.cleanup()?;
    upper_project.cleanup()?;
    runtime.cleanup()?;
    drop(backend);
    Ok(())
}

#[test]
fn live_master_child_without_a_socket_times_out_and_is_reaped() -> TestResult {
    let backend = TcpListener::bind(("127.0.0.1", 0))?;
    let local_port = backend.local_addr()?.port();
    let runtime = private_runtime_directory()?;
    let project = Project::new(
        "openssh-master-live-without-socket",
        &service_declaration("web", local_port),
        &publishing_intent("web", OPENSSH_PUBLISHER),
        Some(&canonical_provider_configuration("web")),
    )?;
    let fake = FakeSsh::new(
        "openssh-master-live-without-socket-fake",
        runtime.path(),
        "master-live-without-socket",
    )?;
    let started_at = Instant::now();
    let mut child = spawn_aequimuta_with_env(
        project.path(),
        &["publish", "web", OPENSSH_PUBLISHER],
        fake.environment(runtime.path()),
    )?;
    let harness_deadline = started_at + Duration::from_secs(45);

    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= harness_deadline {
            child.kill()?;
            child.wait()?;
            fake.cleanup()?;
            project.cleanup()?;
            runtime.cleanup()?;
            drop(backend);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "publish did not honor the production master-start timeout",
            )
            .into());
        }
        thread::sleep(Duration::from_millis(10));
    }

    let output = child.wait_with_output()?;
    let elapsed = started_at.elapsed();
    assert!(
        elapsed >= Duration::from_secs(29),
        "master start failed before the production 30-second deadline: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(45),
        "master start did not fail within the test harness deadline: {elapsed:?}"
    );
    assert_operation_failure(
        &output,
        b"error: failed to establish the OpenSSH ControlMaster\n",
        "live master child without socket",
    );

    let calls = fake.calls()?;
    assert_eq!(calls.len(), 2);
    let expansion = calls
        .iter()
        .find(|call| is_expansion_call(call))
        .ok_or("control-path expansion was not recorded")?;
    assert_expansion_call(
        expansion,
        TEST_HOST,
        TEST_USER,
        TEST_SSH_PORT,
        &control_path_template(runtime.path()),
    );
    let master = calls
        .iter()
        .find(|call| is_master_creation_call(call))
        .ok_or("master creation was not recorded")?;
    assert_master_creation_call(
        master,
        TEST_HOST,
        TEST_USER,
        TEST_SSH_PORT,
        &fake.control_path,
    );
    assert!(
        calls
            .iter()
            .all(|call| !is_control_call(call, "check") && !is_control_call(call, "forward"))
    );
    assert!(
        !fake.control_path.exists(),
        "the no-socket fixture unexpectedly created a control socket"
    );

    let master_pids = fake.master_pids()?;
    assert_eq!(master_pids.len(), 1);
    assert!(
        !Path::new("/proc").join(master_pids[0].to_string()).exists(),
        "the timed-out master child was not killed and reaped"
    );
    project.assert_unchanged()?;

    fake.cleanup_reaped_master()?;
    assert!(
        master_pids
            .iter()
            .all(|pid| !Path::new("/proc").join(pid.to_string()).exists()),
        "the timed-out fake master remained after fixture cleanup"
    );
    project.cleanup()?;
    runtime.cleanup()?;
    drop(backend);
    Ok(())
}

#[test]
fn master_creation_failure_is_reported_without_forwarding_or_raw_output() -> TestResult {
    let backend = TcpListener::bind(("127.0.0.1", 0))?;
    let local_port = backend.local_addr()?.port();
    let runtime = private_runtime_directory()?;
    let project = Project::new(
        "openssh-master-failure",
        &service_declaration("web", local_port),
        &publishing_intent("web", OPENSSH_PUBLISHER),
        Some(&canonical_provider_configuration("web")),
    )?;
    let fake = FakeSsh::new(
        "openssh-master-failure-fake",
        runtime.path(),
        "master-failure",
    )?;
    let output = run_aequimuta_with_env(
        project.path(),
        &["publish", "web", OPENSSH_PUBLISHER],
        fake.environment(runtime.path()),
    )?;

    assert_operation_failure(
        &output,
        b"error: failed to establish the OpenSSH ControlMaster\n",
        "master creation failure",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("super-secret"));
    assert!(!stderr.contains("/private/test-key"));
    let calls = fake.calls()?;
    assert_eq!(calls.len(), 2);
    let expansion = calls
        .iter()
        .find(|call| is_expansion_call(call))
        .ok_or("control-path expansion was not recorded")?;
    assert_expansion_call(
        expansion,
        TEST_HOST,
        TEST_USER,
        TEST_SSH_PORT,
        &control_path_template(runtime.path()),
    );
    let master = calls
        .iter()
        .find(|call| is_master_creation_call(call))
        .ok_or("master creation was not recorded")?;
    assert_master_creation_call(
        master,
        TEST_HOST,
        TEST_USER,
        TEST_SSH_PORT,
        &fake.control_path,
    );
    assert!(
        calls
            .iter()
            .all(|call| !is_control_call(call, "check") && !is_control_call(call, "forward"))
    );
    project.assert_unchanged()?;

    fake.cleanup()?;
    project.cleanup()?;
    runtime.cleanup()?;
    drop(backend);
    Ok(())
}

#[test]
fn live_master_is_reused_and_forward_failure_is_acknowledged_without_raw_output() -> TestResult {
    let backend = TcpListener::bind(("127.0.0.1", 0))?;
    let local_port = backend.local_addr()?.port();
    let declaration = service_declaration("web", local_port);
    let publishing = publishing_intent("web", OPENSSH_PUBLISHER);
    let provider = canonical_provider_configuration("web");

    let live_runtime = private_runtime_directory()?;
    let live_project = Project::new(
        "openssh-live-master",
        &declaration,
        &publishing,
        Some(&provider),
    )?;
    let mut live_fake = FakeSsh::new("openssh-live-master-fake", live_runtime.path(), "success")?;
    live_fake.make_master_live(live_runtime.path())?;
    let live_output = run_aequimuta_with_env(
        live_project.path(),
        &["publish", "web", OPENSSH_PUBLISHER],
        live_fake.environment(live_runtime.path()),
    )?;

    assert_eq!(live_output.status.code(), Some(0));
    assert_eq!(
        live_output.stdout,
        expected_success_stdout("web", local_port)
    );
    assert!(live_output.stderr.is_empty());
    let live_calls = live_fake.calls()?;
    assert_eq!(live_calls.len(), 3);
    assert!(live_calls.iter().any(|call| is_expansion_call(call)));
    assert!(live_calls.iter().any(|call| is_control_call(call, "check")));
    assert!(
        live_calls
            .iter()
            .any(|call| is_control_call(call, "forward"))
    );
    assert!(live_calls.iter().all(|call| !is_master_creation_call(call)));
    let live_forward = live_calls
        .iter()
        .find(|call| is_control_call(call, "forward"))
        .ok_or("live-master forward was not recorded")?;
    let live_expansion = live_calls
        .iter()
        .find(|call| is_expansion_call(call))
        .ok_or("live-master expansion was not recorded")?;
    assert_expansion_call(
        live_expansion,
        TEST_HOST,
        TEST_USER,
        TEST_SSH_PORT,
        &control_path_template(live_runtime.path()),
    );
    let live_check = live_calls
        .iter()
        .find(|call| is_control_call(call, "check"))
        .ok_or("live-master check was not recorded")?;
    assert_check_call(
        live_check,
        TEST_HOST,
        TEST_USER,
        TEST_SSH_PORT,
        &live_fake.control_path,
    );
    assert_forward_call(
        live_forward,
        &ExpectedForward {
            host: TEST_HOST,
            user: TEST_USER,
            ssh_port: TEST_SSH_PORT,
            listen_address: TEST_LISTEN_ADDRESS,
            listen_port: TEST_LISTEN_PORT,
            local_port,
            control_path: &live_fake.control_path,
        },
    );
    live_project.assert_unchanged()?;
    live_fake.cleanup()?;
    live_project.cleanup()?;
    live_runtime.cleanup()?;

    let failed_runtime = private_runtime_directory()?;
    let failed_project = Project::new(
        "openssh-forward-failure",
        &declaration,
        &publishing,
        Some(&provider),
    )?;
    let mut failed_fake = FakeSsh::new(
        "openssh-forward-failure-fake",
        failed_runtime.path(),
        "forward-failure",
    )?;
    failed_fake.make_master_live(failed_runtime.path())?;
    let failed_output = run_aequimuta_with_env(
        failed_project.path(),
        &["publish", "web", OPENSSH_PUBLISHER],
        failed_fake.environment(failed_runtime.path()),
    )?;

    assert_operation_failure(
        &failed_output,
        b"error: OpenSSH remote forwarding request was not acknowledged\n",
        "forward acknowledgement failure",
    );
    let stderr = String::from_utf8_lossy(&failed_output.stderr);
    assert!(!stderr.contains("super-secret"));
    assert!(!stderr.contains("/private/test-key"));
    let failed_calls = failed_fake.calls()?;
    assert_eq!(
        failed_calls
            .iter()
            .filter(|call| is_control_call(call, "forward"))
            .count(),
        1
    );
    assert!(
        failed_calls
            .iter()
            .all(|call| !is_master_creation_call(call))
    );
    let failed_expansion = failed_calls
        .iter()
        .find(|call| is_expansion_call(call))
        .ok_or("failed-forward expansion was not recorded")?;
    assert_expansion_call(
        failed_expansion,
        TEST_HOST,
        TEST_USER,
        TEST_SSH_PORT,
        &control_path_template(failed_runtime.path()),
    );
    let failed_check = failed_calls
        .iter()
        .find(|call| is_control_call(call, "check"))
        .ok_or("failed-forward master check was not recorded")?;
    assert_check_call(
        failed_check,
        TEST_HOST,
        TEST_USER,
        TEST_SSH_PORT,
        &failed_fake.control_path,
    );
    let failed_forward = failed_calls
        .iter()
        .find(|call| is_control_call(call, "forward"))
        .ok_or("failed forward was not recorded")?;
    assert_forward_call(
        failed_forward,
        &ExpectedForward {
            host: TEST_HOST,
            user: TEST_USER,
            ssh_port: TEST_SSH_PORT,
            listen_address: TEST_LISTEN_ADDRESS,
            listen_port: TEST_LISTEN_PORT,
            local_port,
            control_path: &failed_fake.control_path,
        },
    );
    failed_project.assert_unchanged()?;
    failed_fake.cleanup()?;
    failed_project.cleanup()?;
    failed_runtime.cleanup()?;
    drop(backend);
    Ok(())
}

#[test]
fn stale_socket_and_unsafe_control_entry_fail_closed_and_are_preserved() -> TestResult {
    let backend = TcpListener::bind(("127.0.0.1", 0))?;
    let local_port = backend.local_addr()?.port();
    let declaration = service_declaration("web", local_port);
    let publishing = publishing_intent("web", OPENSSH_PUBLISHER);
    let provider = canonical_provider_configuration("web");

    let stale_runtime = private_runtime_directory()?;
    prepare_provider_runtime_directory(stale_runtime.path())?;
    let stale_project = Project::new(
        "openssh-stale-socket",
        &declaration,
        &publishing,
        Some(&provider),
    )?;
    let stale_fake = FakeSsh::new("openssh-stale-socket-fake", stale_runtime.path(), "success")?;
    let stale_listener = UnixListener::bind(&stale_fake.control_path)?;
    fs::set_permissions(&stale_fake.control_path, fs::Permissions::from_mode(0o600))?;
    drop(stale_listener);
    let stale_output = run_aequimuta_with_env(
        stale_project.path(),
        &["publish", "web", OPENSSH_PUBLISHER],
        stale_fake.environment(stale_runtime.path()),
    )?;

    assert_operation_failure(
        &stale_output,
        b"error: OpenSSH control socket state is stale or unsafe\n",
        "stale socket",
    );
    assert!(
        fs::symlink_metadata(&stale_fake.control_path)?
            .file_type()
            .is_socket(),
        "stale socket was removed or replaced"
    );
    let stale_calls = stale_fake.calls()?;
    assert!(
        stale_calls
            .iter()
            .any(|call| is_control_call(call, "check"))
    );
    assert!(
        stale_calls
            .iter()
            .all(|call| !is_master_creation_call(call) && !is_control_call(call, "forward"))
    );
    let stale_check = stale_calls
        .iter()
        .find(|call| is_control_call(call, "check"))
        .ok_or("stale master check was not recorded")?;
    assert_check_call(
        stale_check,
        TEST_HOST,
        TEST_USER,
        TEST_SSH_PORT,
        &stale_fake.control_path,
    );
    stale_project.assert_unchanged()?;
    fs::remove_file(&stale_fake.control_path)?;
    stale_fake.cleanup()?;
    stale_project.cleanup()?;
    stale_runtime.cleanup()?;

    let unsafe_runtime = private_runtime_directory()?;
    prepare_provider_runtime_directory(unsafe_runtime.path())?;
    let unsafe_project = Project::new(
        "openssh-unsafe-control-entry",
        &declaration,
        &publishing,
        Some(&provider),
    )?;
    let unsafe_fake = FakeSsh::new(
        "openssh-unsafe-control-entry-fake",
        unsafe_runtime.path(),
        "success",
    )?;
    fs::write(&unsafe_fake.control_path, b"preserve this entry\n")?;
    fs::set_permissions(&unsafe_fake.control_path, fs::Permissions::from_mode(0o600))?;
    let unsafe_output = run_aequimuta_with_env(
        unsafe_project.path(),
        &["publish", "web", OPENSSH_PUBLISHER],
        unsafe_fake.environment(unsafe_runtime.path()),
    )?;

    assert_operation_failure(
        &unsafe_output,
        b"error: OpenSSH control socket state is stale or unsafe\n",
        "unsafe control entry",
    );
    assert_eq!(
        fs::read(&unsafe_fake.control_path)?,
        b"preserve this entry\n"
    );
    let unsafe_calls = unsafe_fake.calls()?;
    assert_eq!(
        unsafe_calls
            .iter()
            .filter(|call| is_expansion_call(call))
            .count(),
        1
    );
    assert!(
        unsafe_calls
            .iter()
            .all(|call| !is_master_creation_call(call) && !is_control_call(call, "forward"))
    );
    let unsafe_expansion = unsafe_calls
        .iter()
        .find(|call| is_expansion_call(call))
        .ok_or("unsafe-entry expansion was not recorded")?;
    assert_expansion_call(
        unsafe_expansion,
        TEST_HOST,
        TEST_USER,
        TEST_SSH_PORT,
        &control_path_template(unsafe_runtime.path()),
    );
    unsafe_project.assert_unchanged()?;
    fs::remove_file(&unsafe_fake.control_path)?;
    unsafe_fake.cleanup()?;
    unsafe_project.cleanup()?;
    unsafe_runtime.cleanup()?;
    drop(backend);
    Ok(())
}

#[test]
fn tailscale_publish_ignores_invalid_openssh_configuration_and_openssh_status_stays_unsupported()
-> TestResult {
    let backend = TcpListener::bind(("127.0.0.1", 0))?;
    let port = backend.local_addr()?.port();
    let tailscale_project = Project::new(
        "tailscale-ignores-openssh-provider",
        &service_declaration("web", port),
        &publishing_intent("web", TAILSCALE_PUBLISHER),
        Some(b"this is not valid TOML = [\nprivate_key = \"must-not-be-read\"\n"),
    )?;
    let fake_tailscale = TestDirectory::new("tailscale-ignores-openssh-fake")?;
    let tailscale_executable = fake_tailscale.path().join("tailscale");
    fs::write(&tailscale_executable, FAKE_TAILSCALE)?;
    fs::set_permissions(&tailscale_executable, fs::Permissions::from_mode(0o700))?;
    let tailscale_output = run_aequimuta_with_env(
        tailscale_project.path(),
        &["publish", "web", TAILSCALE_PUBLISHER],
        [
            ("PATH", fake_tailscale.path().as_os_str().to_owned()),
            ("AEQUIMUTA_FAKE_PORT", OsString::from(port.to_string())),
        ],
    )?;

    assert_eq!(tailscale_output.status.code(), Some(0));
    assert_eq!(
        tailscale_output.stdout,
        format!(
            "Publication already satisfied for web via {TAILSCALE_PUBLISHER} \
             at tcp://publish-test.example.ts.net:{port}\n"
        )
        .as_bytes()
    );
    assert!(tailscale_output.stderr.is_empty());
    tailscale_project.assert_unchanged()?;
    fake_tailscale.cleanup()?;
    tailscale_project.cleanup()?;
    drop(backend);

    let status_declaration = b"[[services]]\n\
                               name = \"web\"\n\
                               port = 8080\n\
                               \n\
                               [[services]]\n\
                               name = \"api\"\n\
                               port = 8080\n";
    let status_publishing = b"[[publications]]\n\
                              service = \"web\"\n\
                              publisher = \"openssh-reverse-tcp\"\n\
                              \n\
                              [[publications]]\n\
                              service = \"web\"\n\
                              publisher = \"tailscale-serve-tcp\"\n\
                              \n\
                              [[publications]]\n\
                              service = \"api\"\n\
                              publisher = \"tailscale-serve-tcp\"\n";
    let status_project = Project::new(
        "openssh-status-unsupported-with-tailscale-ambiguity",
        status_declaration,
        status_publishing,
        None,
    )?;
    let empty_path = TestDirectory::new("openssh-status-empty-path")?;
    let status_output = Command::new(env!("CARGO_BIN_EXE_aequimuta"))
        .args(["status", "web", OPENSSH_PUBLISHER])
        .current_dir(status_project.path())
        .env("PATH", empty_path.path())
        .env_remove("XDG_RUNTIME_DIR")
        .output()?;

    assert_operation_failure(
        &status_output,
        b"error: selected publisher is not supported\n",
        "OpenSSH status",
    );
    status_project.assert_unchanged()?;
    empty_path.cleanup()?;
    status_project.cleanup()?;
    Ok(())
}

const FAKE_SSH: &[u8] = br#"#!/bin/sh
set -eu

if [ "${AEQUIMUTA_FAKE_SEED_MASTER-0}" != "1" ]; then
    while ! /bin/mkdir "$AEQUIMUTA_FAKE_LOG.lock" 2>/dev/null; do
        /bin/sleep 0.005
    done
    {
        printf 'BEGIN\n'
        for argument do
            printf '%s\n' "$argument"
        done
        printf 'END\n'
    } >> "$AEQUIMUTA_FAKE_LOG"
    /bin/rmdir "$AEQUIMUTA_FAKE_LOG.lock"

    case " $* " in
        *" -G "*)
            printf 'controlpath %s\n' "$AEQUIMUTA_FAKE_OPENSSH_CONTROL"
            exit 0
            ;;
        *" -O check "*)
            if [ "${LC_ALL-}" != "C" ]; then
                printf 'master check locale was not pinned\n' >&2
                exit 97
            fi
            if [ -f "$AEQUIMUTA_FAKE_STATE" ]; then
                IFS= read -r master_pid < "$AEQUIMUTA_FAKE_STATE"
                printf 'Master running (pid=%s)\r\n' "$master_pid" >&2
                exit 0
            fi
            printf 'Control socket is not live\n' >&2
            exit 255
            ;;
        *" -O forward "*)
            if [ "$AEQUIMUTA_FAKE_MODE" = "forward-failure" ]; then
                printf 'super-secret provider diagnostic\n/private/test-key\n' >&2
                exit 42
            fi
            exit 0
            ;;
    esac

    if [ "$AEQUIMUTA_FAKE_MODE" = "master-failure" ]; then
        printf 'super-secret master diagnostic\n/private/test-key\n' >&2
        exit 41
    fi
fi

printf '%s\n' "$$" > "$AEQUIMUTA_FAKE_STATE"
printf '%s\n' "$$" >> "$AEQUIMUTA_FAKE_MASTER_PIDS"
if [ "$AEQUIMUTA_FAKE_MODE" = "master-live-without-socket" ]; then
    while [ ! -f "$AEQUIMUTA_FAKE_STOP" ]; do
        /bin/sleep 0.01
    done
    printf '%s\n' "$$" >> "$AEQUIMUTA_FAKE_EXITED"
    exit 0
fi

attempt=0
while [ ! -S "$AEQUIMUTA_FAKE_CONTROL" ]; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 200 ]; then
        printf 'fake control socket was not created\n' >&2
        exit 43
    fi
    /bin/sleep 0.01
done

while [ ! -f "$AEQUIMUTA_FAKE_STOP" ]; do
    /bin/sleep 0.01
done
printf '%s\n' "$$" >> "$AEQUIMUTA_FAKE_EXITED"
exit 0
"#;

const FAKE_TAILSCALE: &[u8] = br#"#!/bin/sh
set -eu

if [ "$#" -eq 2 ] && [ "$1" = "status" ] && [ "$2" = "--json" ]; then
    printf '{"BackendState":"Running","Self":{"DNSName":"publish-test.example.ts.net.","Online":true},"CurrentTailnet":{"MagicDNSEnabled":true}}\n'
    exit 0
fi

if [ "$#" -eq 3 ] && [ "$1" = "serve" ] && [ "$2" = "status" ] && [ "$3" = "--json" ]; then
    printf '{"TCP":{"%s":{"TCPForward":"127.0.0.1:%s"}}}\n' "$AEQUIMUTA_FAKE_PORT" "$AEQUIMUTA_FAKE_PORT"
    exit 0
fi

printf 'unexpected fake tailscale command\n' >&2
exit 99
"#;
