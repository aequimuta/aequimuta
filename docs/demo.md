# Aequimuta demo walkthrough

[README](../README.md) · [Design](design.md) ·
[Tailscale Serve TCP guide](providers/tailscale-serve-tcp.md) ·
[OpenSSH reverse TCP guide](providers/openssh-reverse-tcp.md)

## Demo goal

This walkthrough shows one unchanged service declaration published through two
independent concrete mechanisms:

- tailnet-private Tailscale Serve raw TCP forwarding
- OpenSSH remote TCP forwarding backed by a background SSH session

Allow about three to five minutes after the provider prerequisites are ready.

## What the demo proves

The walkthrough demonstrates that:

- `web` and its local port are declared once
- both exact publisher tokens can select that same service
- OpenSSH-specific values stay outside the service and core publishing intent
- Tailscale and OpenSSH retain different outcomes, status, visibility, and
  lifetime semantics

It does not demonstrate coordinated cutover, project-wide reconciliation, or
public Internet reachability.

## Prerequisites

Build Aequimuta from the repository:

```sh
cargo build --release
```

Make `target/release/aequimuta` available on `PATH` as `aequimuta` for the
commands below.

Prepare both provider environments:

- Tailscale: a running and authenticated local client and daemon, enabled
  MagicDNS, and an available Serve TCP slot for port `8080`
- OpenSSH: a reachable server, non-interactive authentication, established
  strict host-key trust, an allowed fixed remote listener, and a safe
  owner-only `XDG_RUNTIME_DIR`

The example data-path checks use `python3` as a demo-only TCP backend and
`curl` as its client. Equivalent TCP-capable tools can be used instead.

Use only ports and remote listeners you are authorized to configure. The two
publishing paths are independent, so you can demonstrate either one alone if
the other provider is unavailable.

## 1. Start one local service

In one terminal, start a loopback-only demo backend:

```sh
python3 -m http.server 8080 --bind 127.0.0.1
```

Python is only a convenient demo backend, not an Aequimuta dependency.
Aequimuta publishes TCP and has no HTTP-specific behavior.

Confirm the backend locally:

```sh
curl http://127.0.0.1:8080/
```

## 2. Create one service declaration

In a fresh project directory, create `aequimuta.toml`:

```toml
[[services]]
name = "web"
port = 8080
```

This file contains no provider name, remote host, bind address, or credential.

## 3. Create two publishing intents

Create `aequimuta.publish.toml`:

```toml
[[publications]]
service = "web"
publisher = "tailscale-serve-tcp"

[[publications]]
service = "web"
publisher = "openssh-reverse-tcp"
```

The same service and local port are valid with two different publisher tokens.

## 4. Add OpenSSH-specific configuration

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

Replace all example destination and listener values with settings for your SSH
environment. Do not add private keys, passwords, passphrases, tokens, or
`known_hosts` contents to this file.

## 5. Validate

Validate the core service declaration:

```sh
aequimuta validate
```

Then validate the service-to-publishing-intent relationships:

```sh
aequimuta validate-publishing
```

The second command does not validate the OpenSSH-specific file. That file is
validated when the OpenSSH publish path is selected.

## 6. Publish through Tailscale

Run:

```sh
aequimuta publish web tailscale-serve-tcp
```

For an absent selected slot, the output has this shape:

```text
Published web via tailscale-serve-tcp at tcp://<tailscale-dns-name>:8080
```

The returned endpoint identifies the current tailnet-private raw TCP mapping.
Because the demo backend speaks HTTP over that TCP path, a tailnet client can
test it with the returned DNS name and port:

```sh
curl http://<tailscale-dns-name>:8080/
```

This check depends on the client being able to resolve and reach the tailnet
endpoint.

## 7. Observe Tailscale status

Run:

```sh
aequimuta status web tailscale-serve-tcp
```

After the exact mapping is present, the expected relation line is:

```text
Publication status for web via tailscale-serve-tcp: satisfied
```

This observes the provider mapping only. Stop the Python backend and the mapping
can still be `satisfied`; status is not a local health check. Restart the backend
before continuing because both publish paths preflight local TCP connectivity.

## 8. Publish through OpenSSH

With the configured remote server ready, run:

```sh
aequimuta publish web openssh-reverse-tcp
```

Using the example values, the exact success form is:

```text
Ensured web via openssh-reverse-tcp: aequimuta@edge.example.com:22 listen 0.0.0.0:18080 -> 127.0.0.1:8080 (SSH-session-backed; no automatic reconnect)
```

This means the current background SSH session received acknowledgement for the
requested forwarding. It does not mean the listener is publicly reachable or
that the server preserved the requested bind address exactly.

## 9. Verify the OpenSSH TCP data path

From a location that remote SSH policy, routing, and firewall rules allow to
reach the listener, request the demo backend through the remote address:

```sh
curl http://<reachable-remote-address>:18080/
```

Choose `<reachable-remote-address>` from observed server and network behavior,
not merely from `listen_address`. A request for `0.0.0.0` does not configure DNS,
open a firewall, traverse NAT, or guarantee Internet reachability.

## 10. Repeat publish

Repeat Tailscale publication:

```sh
aequimuta publish web tailscale-serve-tcp
```

With the exact mapping already present, it reports the distinct
Already-satisfied outcome:

```text
Publication already satisfied for web via tailscale-serve-tcp at tcp://<tailscale-dns-name>:8080
```

Repeat OpenSSH publication:

```sh
aequimuta publish web openssh-reverse-tcp
```

It reuses the valid destination-specific ControlMaster and requests the exact
forward again. The outcome remains `Ensured`; it does not claim Created or
Already satisfied.

## What changed and what did not

Unchanged:

- `aequimuta.toml`
- service name `web`
- local port `8080`
- local backend `127.0.0.1:8080`

Changed by selection:

- the exact publisher token passed to `publish`
- the concrete provider operation
- provider-specific configuration requirements
- visibility and endpoint meaning
- lifetime, recovery behavior, outcome, and status support

That distinction is the purpose of the demo: the service stays neutral while
the mechanism remains concrete.

## Safety notes

- Aequimuta selects one exact desired `(service, publisher)` relation per
  command.
- It does not overwrite a conflicting Tailscale slot or an occupied OpenSSH
  listener.
- It does not treat unknown provider state as absent.
- The Tailscale create path verifies its post-condition.
- The OpenSSH path waits for forwarding acknowledgement.
- Project TOML files are not modified by `publish` or `status`.
- Neither path provides an absolute transaction or concurrent-writer guarantee.

## Cleanup limitations

Aequimuta currently has no unpublish or delete command.

Stopping the Python process stops only the local backend. It does not remove a
Tailscale Serve mapping or deliberately close the dedicated OpenSSH master.
Use provider-specific, narrowly targeted cleanup procedures approved for your
environment. Do not use a broad reset, and do not assume Aequimuta owns a
pre-existing mapping or listener.
