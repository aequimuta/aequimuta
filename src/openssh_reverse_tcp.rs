use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const CONFIGURATION_PATH: &str = "aequimuta.openssh-reverse-tcp.toml";
const SSH_EXECUTABLE: &str = "ssh";
const CONFIGURATION_READ_ERROR: &str = "error: failed to read aequimuta.openssh-reverse-tcp.toml";
const CONFIGURATION_UTF8_ERROR: &str =
    "error: aequimuta.openssh-reverse-tcp.toml is not valid UTF-8";
const INVALID_CONFIGURATION_ERROR: &str =
    "error: aequimuta.openssh-reverse-tcp.toml is not a valid OpenSSH reverse TCP configuration";
const CONFIGURATION_RESOLUTION_ERROR: &str =
    "error: desired OpenSSH reverse TCP publication has no provider configuration";
const DESIRED_SLOT_CONFLICT_ERROR: &str =
    "error: desired OpenSSH reverse TCP publications conflict";
const RUNTIME_DIRECTORY_ERROR: &str =
    "error: XDG_RUNTIME_DIR is not safe for OpenSSH reverse TCP control state";
const SSH_EXECUTABLE_ERROR: &str = "error: ssh executable is not available";
const CONTROL_PATH_ERROR: &str = "error: failed to resolve a safe OpenSSH control path";
const STALE_CONTROL_SOCKET_ERROR: &str = "error: OpenSSH control socket state is stale or unsafe";
const MASTER_CREATION_ERROR: &str = "error: failed to establish the OpenSSH ControlMaster";
const MASTER_CLEANUP_ERROR: &str = "error: failed to clean up an incomplete OpenSSH ControlMaster";
const FORWARD_REQUEST_ERROR: &str = "error: OpenSSH remote forwarding request was not acknowledged";
const UNIX_SOCKET_PATH_CAPACITY: usize = 108;
const OPENSSH_CONTROL_PATH_TEMPORARY_SUFFIX_LENGTH: usize = 17;
const FNV1A_64_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV1A_64_PRIME: u64 = 0x00000100000001b3;
const SSH_CONFIGURATION_TIMEOUT: Duration = Duration::from_secs(5);
const SSH_CONTROL_CHECK_TIMEOUT: Duration = Duration::from_secs(5);
const SSH_FORWARD_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MASTER_START_TIMEOUT: Duration = Duration::from_secs(30);
const MASTER_START_POLL_INTERVAL: Duration = Duration::from_millis(10);
const SSH_COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderConfiguration {
    #[serde(default)]
    publications: Vec<ProviderPublication>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderPublication {
    service: String,
    pub(crate) host: String,
    pub(crate) user: String,
    pub(crate) ssh_port: u16,
    pub(crate) listen_address: String,
    pub(crate) listen_port: u16,
}

pub(crate) fn load_and_resolve(
    selected_service: &str,
    declared_services: &[&str],
    desired_services: &[&str],
) -> Result<ProviderPublication, String> {
    let bytes = fs::read(CONFIGURATION_PATH).map_err(|_| CONFIGURATION_READ_ERROR.to_owned())?;
    let source = std::str::from_utf8(&bytes).map_err(|_| CONFIGURATION_UTF8_ERROR.to_owned())?;
    let configuration: ProviderConfiguration =
        toml::from_str(source).map_err(|_| INVALID_CONFIGURATION_ERROR.to_owned())?;
    let declared_services: HashSet<&str> = declared_services.iter().copied().collect();
    let mut by_service = HashMap::with_capacity(configuration.publications.len());

    for publication in &configuration.publications {
        if !provider_publication_is_valid(publication)
            || !declared_services.contains(publication.service.as_str())
            || by_service
                .insert(publication.service.as_str(), publication)
                .is_some()
        {
            return Err(INVALID_CONFIGURATION_ERROR.to_owned());
        }
    }

    let mut remote_slots = HashSet::with_capacity(desired_services.len());

    for desired_service in desired_services {
        let publication = by_service
            .get(desired_service)
            .ok_or_else(|| CONFIGURATION_RESOLUTION_ERROR.to_owned())?;
        let remote_slot = (
            publication.host.as_str(),
            publication.user.as_str(),
            publication.ssh_port,
            publication.listen_port,
        );

        if !remote_slots.insert(remote_slot) {
            return Err(DESIRED_SLOT_CONFLICT_ERROR.to_owned());
        }
    }

    by_service
        .get(selected_service)
        .copied()
        .cloned()
        .ok_or_else(|| CONFIGURATION_RESOLUTION_ERROR.to_owned())
}

pub(crate) fn ensure(local_port: u16, publication: &ProviderPublication) -> Result<(), String> {
    ensure_local_backend_is_reachable(local_port)?;

    let current_uid = current_uid()?;
    let runtime_directory = prepare_runtime_directory(current_uid)?;
    let control_path_template = runtime_directory.join("cm-%C");
    let control_path = expand_control_path(&control_path_template, publication)?;

    match fs::symlink_metadata(&control_path) {
        Ok(metadata) => {
            validate_control_socket(&metadata, current_uid)?;
            live_master_pid(&control_path, publication, STALE_CONTROL_SOCKET_ERROR)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_or_reuse_master(&control_path, publication, current_uid)?;
        }
        Err(_) => return Err(STALE_CONTROL_SOCKET_ERROR.to_owned()),
    }

    request_forward(&control_path, publication, local_port)
}

fn provider_publication_is_valid(publication: &ProviderPublication) -> bool {
    !publication.service.is_empty()
        && publication.service.trim() == publication.service
        && !publication.service.chars().any(char::is_control)
        && host_is_valid(&publication.host)
        && !publication.user.is_empty()
        && !publication
            .user
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        && publication.ssh_port != 0
        && publication.listen_address.parse::<Ipv4Addr>().is_ok()
        && publication.listen_port >= 1024
}

fn host_is_valid(host: &str) -> bool {
    if host.is_empty()
        || host.starts_with('-')
        || host
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return false;
    }

    if host.parse::<Ipv4Addr>().is_ok() {
        return true;
    }

    if host.contains('.')
        && host
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return false;
    }

    let hostname = host.strip_suffix('.').unwrap_or(host);

    !hostname.is_empty()
        && hostname.len() <= 253
        && hostname.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && matches!(
                    label.as_bytes().first(),
                    Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9')
                )
                && matches!(
                    label.as_bytes().last(),
                    Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9')
                )
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn ensure_local_backend_is_reachable(port: u16) -> Result<(), String> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));

    TcpStream::connect_timeout(&address, Duration::from_secs(1))
        .map(|_| ())
        .map_err(|_| format!("error: local TCP backend 127.0.0.1:{port} is not reachable"))
}

fn current_uid() -> Result<u32, String> {
    fs::metadata("/proc/self")
        .map(|metadata| metadata.uid())
        .map_err(|_| RUNTIME_DIRECTORY_ERROR.to_owned())
}

fn prepare_runtime_directory(current_uid: u32) -> Result<PathBuf, String> {
    let xdg_runtime_directory =
        env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| RUNTIME_DIRECTORY_ERROR.to_owned())?;
    let xdg_runtime_directory = PathBuf::from(xdg_runtime_directory);

    if !xdg_runtime_directory.is_absolute() {
        return Err(RUNTIME_DIRECTORY_ERROR.to_owned());
    }

    validate_private_directory(&xdg_runtime_directory, current_uid)?;
    let aequimuta_directory = xdg_runtime_directory.join("aequimuta");
    ensure_private_directory(&aequimuta_directory, current_uid)?;
    let provider_directory = aequimuta_directory.join("openssh-reverse-tcp");
    ensure_private_directory(&provider_directory, current_uid)?;

    Ok(provider_directory)
}

fn ensure_private_directory(path: &Path, current_uid: u32) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(_) => validate_private_directory(path, current_uid),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = DirBuilder::new();
            builder.mode(0o700);
            builder
                .create(path)
                .map_err(|_| RUNTIME_DIRECTORY_ERROR.to_owned())?;
            validate_private_directory(path, current_uid)
        }
        Err(_) => Err(RUNTIME_DIRECTORY_ERROR.to_owned()),
    }
}

fn validate_private_directory(path: &Path, current_uid: u32) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|_| RUNTIME_DIRECTORY_ERROR.to_owned())?;
    let canonical_path = fs::canonicalize(path).map_err(|_| RUNTIME_DIRECTORY_ERROR.to_owned())?;

    if !metadata.file_type().is_dir()
        || metadata.uid() != current_uid
        || metadata.mode() & 0o7777 != 0o700
        || canonical_path != path
    {
        return Err(RUNTIME_DIRECTORY_ERROR.to_owned());
    }

    Ok(())
}

fn expand_control_path(
    control_path_template: &Path,
    publication: &ProviderPublication,
) -> Result<PathBuf, String> {
    let mut command = Command::new(SSH_EXECUTABLE);
    command
        .arg("-G")
        .arg("-F")
        .arg("none")
        .arg("-4")
        .arg("-S")
        .arg(control_path_template)
        .arg("-l")
        .arg(&publication.user)
        .arg("-p")
        .arg(publication.ssh_port.to_string())
        .arg("-o")
        .arg(host_name_option(&publication.host))
        .arg("-o")
        .arg("CanonicalizeHostname=no")
        .arg("-o")
        .arg("ProxyCommand=none")
        .arg("-o")
        .arg("ProxyJump=none")
        .arg(&publication.host);
    let output = run_ssh(&mut command, SSH_CONFIGURATION_TIMEOUT, CONTROL_PATH_ERROR)?;

    if !output.status.success() {
        return Err(CONTROL_PATH_ERROR.to_owned());
    }

    let openssh_path =
        parse_control_path(&output.stdout).ok_or_else(|| CONTROL_PATH_ERROR.to_owned())?;
    let expected_parent = control_path_template
        .parent()
        .ok_or_else(|| CONTROL_PATH_ERROR.to_owned())?;
    let openssh_file_name = openssh_path
        .file_name()
        .map(OsStr::as_bytes)
        .ok_or_else(|| CONTROL_PATH_ERROR.to_owned())?;

    if openssh_path.parent() != Some(expected_parent)
        || openssh_file_name.len() != 43
        || !openssh_file_name.starts_with(b"cm-")
        || !openssh_file_name[3..]
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(CONTROL_PATH_ERROR.to_owned());
    }

    let lexical_hash = lexical_destination_hash(publication);
    let mut file_name = format!("cm-{lexical_hash:016x}").into_bytes();
    file_name.extend_from_slice(&openssh_file_name[19..]);
    let control_path = expected_parent.join(OsString::from_vec(file_name));

    if control_path.as_os_str().as_bytes().len() + OPENSSH_CONTROL_PATH_TEMPORARY_SUFFIX_LENGTH
        >= UNIX_SOCKET_PATH_CAPACITY
    {
        return Err(CONTROL_PATH_ERROR.to_owned());
    }

    Ok(control_path)
}

fn lexical_destination_hash(publication: &ProviderPublication) -> u64 {
    let mut hash = FNV1A_64_OFFSET_BASIS;

    for byte in publication
        .host
        .as_bytes()
        .iter()
        .copied()
        .chain(std::iter::once(0))
        .chain(publication.user.as_bytes().iter().copied())
        .chain(std::iter::once(0))
        .chain(publication.ssh_port.to_be_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV1A_64_PRIME);
    }

    hash
}

fn parse_control_path(stdout: &[u8]) -> Option<PathBuf> {
    let mut paths = stdout
        .split(|byte| *byte == b'\n')
        .filter_map(|line| line.strip_prefix(b"controlpath "));
    let path = paths.next()?;

    if path.is_empty() || paths.next().is_some() {
        return None;
    }

    Some(PathBuf::from(OsString::from_vec(path.to_vec())))
}

fn validate_control_socket(metadata: &fs::Metadata, current_uid: u32) -> Result<(), String> {
    if !metadata.file_type().is_socket()
        || metadata.uid() != current_uid
        || metadata.mode() & 0o077 != 0
    {
        return Err(STALE_CONTROL_SOCKET_ERROR.to_owned());
    }

    Ok(())
}

fn live_master_pid(
    control_path: &Path,
    publication: &ProviderPublication,
    error_message: &str,
) -> Result<u32, String> {
    let mut command = isolated_control_command(control_path, publication);
    command
        .arg("-O")
        .arg("check")
        .arg(&publication.host)
        .env("LC_ALL", "C");
    let output = run_ssh(&mut command, SSH_CONTROL_CHECK_TIMEOUT, error_message)?;

    if !output.status.success() || !output.stdout.is_empty() {
        return Err(error_message.to_owned());
    }

    parse_master_pid(&output.stderr).ok_or_else(|| error_message.to_owned())
}

fn parse_master_pid(stderr: &[u8]) -> Option<u32> {
    let line = stderr.strip_suffix(b"\n")?;
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let digits = line
        .strip_prefix(b"Master running (pid=")?
        .strip_suffix(b")")?;

    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }

    let source = std::str::from_utf8(digits).ok()?;
    let pid = source.parse::<u32>().ok()?;

    if pid == 0 || pid.to_string().as_bytes() != digits {
        return None;
    }

    Some(pid)
}

fn create_or_reuse_master(
    control_path: &Path,
    publication: &ProviderPublication,
    current_uid: u32,
) -> Result<(), String> {
    let mut child = spawn_master(control_path, publication)?;
    let spawned_pid = child.id();
    let started_at = Instant::now();

    loop {
        let remaining = MASTER_START_TIMEOUT.saturating_sub(started_at.elapsed());

        if remaining.is_zero() {
            return fail_after_master_cleanup(&mut child, MASTER_CREATION_ERROR.to_owned());
        }

        match fs::symlink_metadata(control_path) {
            Ok(metadata) => {
                if let Err(error) = validate_control_socket(&metadata, current_uid) {
                    return fail_after_master_cleanup(&mut child, error);
                }

                let live_pid =
                    match live_master_pid(control_path, publication, MASTER_CREATION_ERROR) {
                        Ok(pid) => pid,
                        Err(error) => return fail_after_master_cleanup(&mut child, error),
                    };

                if live_pid == spawned_pid {
                    drop(child);
                    return Ok(());
                }

                cleanup_spawned_master(&mut child)?;
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {
                return fail_after_master_cleanup(
                    &mut child,
                    STALE_CONTROL_SOCKET_ERROR.to_owned(),
                );
            }
        }

        match child.try_wait() {
            Ok(Some(_)) => return Err(MASTER_CREATION_ERROR.to_owned()),
            Ok(None) => thread::sleep(MASTER_START_POLL_INTERVAL.min(remaining)),
            Err(_) => {
                return fail_after_master_cleanup(&mut child, MASTER_CREATION_ERROR.to_owned());
            }
        }
    }
}

fn fail_after_master_cleanup(child: &mut Child, error: String) -> Result<(), String> {
    cleanup_spawned_master(child)?;
    Err(error)
}

fn cleanup_spawned_master(child: &mut Child) -> Result<(), String> {
    if let Ok(Some(_)) = child.try_wait() {
        return Ok(());
    }

    match child.kill() {
        Ok(()) => child
            .wait()
            .map(|_| ())
            .map_err(|_| MASTER_CLEANUP_ERROR.to_owned()),
        Err(_) => match child.try_wait() {
            Ok(Some(_)) => Ok(()),
            Ok(None) | Err(_) => Err(MASTER_CLEANUP_ERROR.to_owned()),
        },
    }
}

fn spawn_master(control_path: &Path, publication: &ProviderPublication) -> Result<Child, String> {
    let mut command = Command::new(SSH_EXECUTABLE);
    command
        .arg("-4")
        .arg("-M")
        .arg("-N")
        .arg("-n")
        .arg("-T")
        .arg("-S")
        .arg(control_path)
        .arg("-l")
        .arg(&publication.user)
        .arg("-p")
        .arg(publication.ssh_port.to_string())
        .arg("-o")
        .arg(host_name_option(&publication.host))
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("StrictHostKeyChecking=yes")
        .arg("-o")
        .arg("ClearAllForwardings=yes")
        .arg("-o")
        .arg("ForwardAgent=no")
        .arg("-o")
        .arg("ForwardX11=no")
        .arg("-o")
        .arg("RequestTTY=no")
        .arg("-o")
        .arg("SessionType=none")
        .arg("-o")
        .arg("RemoteCommand=none")
        .arg("-o")
        .arg("CanonicalizeHostname=no")
        .arg("-o")
        .arg("ProxyCommand=none")
        .arg("-o")
        .arg("ProxyJump=none")
        .arg("-o")
        .arg("ControlMaster=yes")
        .arg("-o")
        .arg("ControlPersist=no")
        .arg("-o")
        .arg("ForkAfterAuthentication=no")
        .arg("-o")
        .arg("PermitLocalCommand=no")
        .arg("-o")
        .arg("LocalCommand=none")
        .arg("-o")
        .arg("Tunnel=no")
        .arg("-o")
        .arg("UpdateHostKeys=no")
        .arg("-o")
        .arg("AddKeysToAgent=no")
        .arg(&publication.host)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    match command.spawn() {
        Ok(child) => Ok(child),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Err(SSH_EXECUTABLE_ERROR.to_owned())
        }
        Err(_) => Err(MASTER_CREATION_ERROR.to_owned()),
    }
}

fn request_forward(
    control_path: &Path,
    publication: &ProviderPublication,
    local_port: u16,
) -> Result<(), String> {
    let forward = format!(
        "{}:{}:127.0.0.1:{local_port}",
        publication.listen_address, publication.listen_port
    );
    let mut command = isolated_control_command(control_path, publication);
    command
        .arg("-O")
        .arg("forward")
        .arg("-R")
        .arg(forward)
        .arg(&publication.host);
    let output = run_ssh(
        &mut command,
        SSH_FORWARD_REQUEST_TIMEOUT,
        FORWARD_REQUEST_ERROR,
    )?;

    if output.status.success() {
        Ok(())
    } else {
        Err(FORWARD_REQUEST_ERROR.to_owned())
    }
}

fn isolated_control_command(control_path: &Path, publication: &ProviderPublication) -> Command {
    let mut command = Command::new(SSH_EXECUTABLE);
    command
        .arg("-F")
        .arg("none")
        .arg("-4")
        .arg("-S")
        .arg(control_path)
        .arg("-o")
        .arg("ControlMaster=no")
        .arg("-l")
        .arg(&publication.user)
        .arg("-p")
        .arg(publication.ssh_port.to_string())
        .arg("-o")
        .arg(host_name_option(&publication.host));
    command
}

fn host_name_option(host: &str) -> String {
    format!("HostName={host}")
}

fn run_ssh(
    command: &mut Command,
    timeout: Duration,
    error_message: &str,
) -> Result<Output, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(SSH_EXECUTABLE_ERROR.to_owned());
        }
        Err(_) => return Err(error_message.to_owned()),
    };
    let started_at = Instant::now();

    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child
                    .wait_with_output()
                    .map_err(|_| error_message.to_owned());
            }
            Ok(None) => {}
            Err(_) => {
                cleanup_timed_out_ssh_command(&mut child, error_message)?;
                return Err(error_message.to_owned());
            }
        }

        let remaining = timeout.saturating_sub(started_at.elapsed());

        if remaining.is_zero() {
            cleanup_timed_out_ssh_command(&mut child, error_message)?;
            return Err(error_message.to_owned());
        }

        thread::sleep(SSH_COMMAND_POLL_INTERVAL.min(remaining));
    }
}

fn cleanup_timed_out_ssh_command(child: &mut Child, error_message: &str) -> Result<(), String> {
    if let Ok(Some(_)) = child.try_wait() {
        return Ok(());
    }

    match child.kill() {
        Ok(()) => child
            .wait()
            .map(|_| ())
            .map_err(|_| error_message.to_owned()),
        Err(_) => match child.try_wait() {
            Ok(Some(_)) => Ok(()),
            Ok(None) | Err(_) => Err(error_message.to_owned()),
        },
    }
}
