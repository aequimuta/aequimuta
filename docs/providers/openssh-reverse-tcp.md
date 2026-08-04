# OpenSSH reverse TCP

[README](../../README.md) · [Design](../design.md) ·
[Tailscale Serve TCP guide](tailscale-serve-tcp.md) · [Demo](../demo.md)

## Overview

The exact capability token is:

```text
openssh-reverse-tcp
```

This capability publishes a local TCP service through an OpenSSH remote TCP
forward. It is not a general SSH command runner or an interface to arbitrary
`ssh` features.

## Semantics

For a selected service, Aequimuta requests this relation through SSH:

```text
<listen_address>:<listen_port> on the SSH server side
    -> SSH session
    -> 127.0.0.1:<service.port> on the Aequimuta machine
```

`listen_address` is the requested IPv4 bind address. SSH acknowledgement means
the forwarding request was accepted by the current session. It is not an
authoritative observation of the address the server actually bound or of who
can reach it.

## Prerequisites

Publishing requires:

- an OpenSSH `ssh` client available on `PATH`
- an SSH server reachable over IPv4; the current path pins OpenSSH to IPv4
- non-interactive authentication already available to OpenSSH
- strict host-key trust already established
- server policy that permits remote TCP forwarding and the requested listener
- a TCP listener reachable at `127.0.0.1:<service.port>`
- an absolute, canonical, current-user-owned `XDG_RUNTIME_DIR` with mode `0700`
- the current Unix control-socket and Linux `/proc/self` UID environment used by
  this implementation

Remote policy and network configuration determine the listener that is
actually created and where it is reachable.

## Core publishing intent

The service remains in `aequimuta.toml`:

```toml
[[services]]
name = "web"
port = 8080
```

The concrete capability is selected in `aequimuta.publish.toml`:

```toml
[[publications]]
service = "web"
publisher = "openssh-reverse-tcp"
```

Neither file contains an SSH destination, remote listener, or credential.

## Provider-specific configuration

Create `aequimuta.openssh-reverse-tcp.toml`:

```toml
[[publications]]
service = "web"
host = "edge.example.com"
user = "aequimuta"
ssh_port = 22
listen_address = "0.0.0.0"
listen_port = 18080
```

These are example values. Replace the destination, user, port, and bind values
with settings approved for your SSH environment.

The file supplements the core declaration and publishing intent. It does not
replace either one. `validate-publishing` does not read it; the selected OpenSSH
publish path loads and validates it.

Every active `openssh-reverse-tcp` publication in `aequimuta.publish.toml` must
resolve to one provider entry, even when the command selects only one service.
Every provider entry must reference a declared service, and duplicate entries
for one service invalidate the provider file.

## Field reference

| Field | Meaning and current validation |
| --- | --- |
| `service` | Exact declared service name; nonempty, no surrounding whitespace or control characters; unique in this provider file |
| `host` | SSH hostname or numeric IPv4 address; no leading `-`, whitespace, control characters, or malformed dotted-numeric value |
| `user` | Nonempty SSH user with no whitespace or control characters |
| `ssh_port` | Fixed SSH server port in `1..=65535` |
| `listen_address` | Requested remote bind address; numeric IPv4 only |
| `listen_port` | Fixed, nonprivileged remote listener port in `1024..=65535` |

Unknown root and publication fields are rejected. Dynamic port `0`, privileged
remote ports, IPv6 listeners, and arbitrary address expressions are not
accepted.

Active desired OpenSSH publications must not repeat the same lexical
`(host, user, ssh_port, listen_port)` remote slot. This is rejected even when
their requested bind addresses differ. Aequimuta does not resolve DNS aliases
to infer that differently written hosts are the same machine.

## Credentials and host trust

The Git-trackable provider file has no fields for:

- private keys
- passwords or passphrases
- tokens
- `known_hosts` content
- ssh-agent credentials
- runtime control sockets or process identifiers

Machine-local OpenSSH configuration, keys or agent, and host-key data provide
authentication and trust. Aequimuta uses batch authentication and strict
host-key checking. It does not prompt for a password or act as a secret
manager. Agent forwarding is disabled; a local agent may still be used by the
OpenSSH client for authentication.

## Publishing

Start the local backend and run:

```sh
aequimuta publish web openssh-reverse-tcp
```

For the examples above, the exact success line is:

```text
Ensured web via openssh-reverse-tcp: aequimuta@edge.example.com:22 listen 0.0.0.0:18080 -> 127.0.0.1:8080 (SSH-session-backed; no automatic reconnect)
```

The command returns exit code `0`, writes that one line to stdout, and leaves
stderr empty. Repeating the same publish request uses the same destination-
specific dedicated ControlMaster when it is still valid and returns `Ensured`
again. It does not distinguish Created from Already satisfied.

The publish path first checks TCP connectivity to the local backend. This is a
connectivity preflight, not application-protocol health checking.

## Lifetime

The publication is **background and SSH-session-backed**:

- Aequimuta creates or reuses a dedicated OpenSSH ControlMaster.
- The CLI exits after confirming the master and receiving forwarding
  acknowledgement.
- The forward remains while that master SSH session remains alive.
- If the session ends, the remote listener can disappear.
- There is no automatic reconnect, retry, or supervision.
- There is no reboot or process, network, or server-restart recovery.

The ControlMaster is destination-specific rather than service-specific, so
non-colliding forwards to the same destination may reuse it. It lives in
`$XDG_RUNTIME_DIR/aequimuta/openssh-reverse-tcp/`, not in the project directory.

## Visibility

`listen_address = "0.0.0.0"` requests an all-interface IPv4 bind from the SSH
server. It does not guarantee:

- Internet reachability
- firewall changes
- NAT traversal
- DNS configuration
- remote routing
- acceptance or exact preservation of the requested bind by server policy

OpenSSH `GatewayPorts` policy can restrict, broaden, or otherwise affect bind
behavior while the forwarding request still receives protocol acknowledgement.
Use the configured address as a requested value, not an observed public
endpoint.

A requested remote bind address does not guarantee public reachability. Remote
SSH policy, routing, firewall, and NAT still apply.

## Safety and conflict behavior

The current path uses these boundaries:

- exact service, publisher, destination, listener, and backend values
- strict provider TOML and unknown-field rejection
- local backend connectivity before SSH mutation
- new dedicated-master creation that suppresses user-configured forwarding
  with `ClearAllForwardings=yes`
- a separate, configuration-isolated control request for the exact `-R`
  forwarding specification
- SSH forwarding acknowledgement before reporting success
- current-user-only XDG directories and private Unix control sockets
- no import of unrelated user-configured forwarding into the dedicated master
- preservation of stale or unsafe control entries rather than automatic
  deletion or takeover
- no cancel, remote cleanup, listener takeover, or remote process manipulation
- no project file modification and no secret field in project configuration

If a remote listener is already occupied or policy rejects the request,
Aequimuta reports failure and does not remove or replace that listener. A new
dedicated master may already have been established before the forwarding
request is rejected; the operation is not presented as a transaction that
leaves no runtime state after every failure.

## Status support

The following command is currently unsupported:

```sh
aequimuta status web openssh-reverse-tcp
```

Aequimuta does not have a provider API that returns a non-mutating,
authoritative list of exact remote forwards and their observed bind addresses.
A local master liveness check during `publish` is not such a snapshot.

For that reason, the Tailscale `absent` / `satisfied` / `conflict` /
`indeterminate` model is not reused for OpenSSH. With a valid selected desired
tuple, the current command fails as an unsupported operation without reading
the OpenSSH provider file or invoking SSH.

## Limitations

- IPv4 SSH transport and fixed IPv4 remote bind only
- fixed nonprivileged remote listener port only
- local backend fixed to `127.0.0.1:<service.port>`
- no authoritative remote forwarding status or discovery
- no unpublish, cancel, or remote cleanup command
- no automatic reconnect, retry, supervision, or reboot recovery
- no durable ownership, provenance, PID manifest, or runtime database
- no IPv6, dynamic port, privileged port, bastion, or ProxyJump model
- actual remote bind and end-to-end visibility are not observed

## Remote server checklist

Before publishing, ask the remote administrator to confirm the applicable
policy rather than changing server settings blindly:

- remote TCP forwarding is allowed for the account
- the requested listener is allowed, including any `PermitListen` restriction
- a non-loopback requested bind is compatible with the server's `GatewayPorts`
  policy
- authorized-key restrictions do not disable or narrow the required forwarding
- another process or forwarding does not already occupy the remote port
- routing and firewall rules independently allow the intended clients to reach
  the listener

Successful SSH acknowledgement does not replace these checks or prove external
reachability.
