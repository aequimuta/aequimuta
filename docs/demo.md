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

It does not demonstrate coordinated cutover, a background convergence loop,
removal of undeclared provider resources, or public Internet reachability.

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

Use only ports and remote listeners you are authorized to configure. The
project-wide `apply` demonstration requires both provider environments. The two
publishing paths remain independent, so you can instead demonstrate either one
with its individual `publish` command if the other provider is unavailable.

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
validated by an individual OpenSSH publish, or during project-wide preflight
when `apply` finds at least one desired OpenSSH publication.

## 6. Apply all desired publications once

Run the zero-argument project-wide operation:

```sh
aequimuta apply
```

Before any provider mutation, Phase A validates the project, operational
publisher support, both provider-specific ambiguity rules, the required
OpenSSH configuration, and TCP reachability of the unique local backend. It
also checks the owner-only OpenSSH XDG runtime directory without starting an
SSH process or requesting a forwarding. Phase B then ensures the two entries
sequentially in the exact order written in `aequimuta.publish.toml`.

With an absent Tailscale slot and a successful OpenSSH forwarding request, the
output has this shape:

```text
Published web via tailscale-serve-tcp at tcp://<tailscale-dns-name>:8080
Ensured web via openssh-reverse-tcp: aequimuta@edge.example.com:22 listen 0.0.0.0:18080 -> 127.0.0.1:8080 (SSH-session-backed; no automatic reconnect)
Applied 2 desired publications
```

The first line is Tailscale's **Created** outcome; an exact pre-existing mapping
would instead produce its **Already satisfied** line. OpenSSH continues to
report **Ensured** rather than adopting those Tailscale outcome labels.

A valid publishing intent with zero entries is also successful and prints only
`No desired publications to apply`. If a runtime provider operation fails,
`apply` stops before later entries and omits the final summary. Earlier
successful effects are not rolled back.

## 7. Publish through Tailscale individually

The exact individual operation remains available:

```sh
aequimuta publish web tailscale-serve-tcp
```

If this command is used instead of Step 6 with an absent selected slot, its
Created output has this shape:

```text
Published web via tailscale-serve-tcp at tcp://<tailscale-dns-name>:8080
```

After Step 6, the same individual command reports the distinct
Already-satisfied outcome:

```text
Publication already satisfied for web via tailscale-serve-tcp at tcp://<tailscale-dns-name>:8080
```

The returned endpoint identifies the current tailnet-private raw TCP mapping.
Because the demo backend speaks HTTP over that TCP path, a tailnet client can
test it with the returned DNS name and port:

```sh
curl http://<tailscale-dns-name>:8080/
```

This check depends on the client being able to resolve and reach the tailnet
endpoint.

## 8. Observe Tailscale status

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

## 9. Publish through OpenSSH individually

The exact individual operation also remains available. With the configured
remote server ready, run:

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

## 10. Verify the OpenSSH TCP data path

From a location that remote SSH policy, routing, and firewall rules allow to
reach the listener, request the demo backend through the remote address:

```sh
curl http://<reachable-remote-address>:18080/
```

Choose `<reachable-remote-address>` from observed server and network behavior,
not merely from `listen_address`. A request for `0.0.0.0` does not configure DNS,
open a firewall, traverse NAT, or guarantee Internet reachability.

## 11. Repeat apply

Run the same project-wide operation again:

```sh
aequimuta apply
```

With the exact Tailscale mapping and valid OpenSSH session already present, the
output keeps each provider's concrete meaning and ends with a new completed-run
summary:

```text
Publication already satisfied for web via tailscale-serve-tcp at tcp://<tailscale-dns-name>:8080
Ensured web via openssh-reverse-tcp: aequimuta@edge.example.com:22 listen 0.0.0.0:18080 -> 127.0.0.1:8080 (SSH-session-backed; no automatic reconnect)
Applied 2 desired publications
```

Tailscale performs no duplicate mutation for the already-satisfied mapping.
OpenSSH reuses the valid destination-specific ControlMaster and requests the
exact forward again. Its outcome remains `Ensured`; it does not claim Created
or Already satisfied.

## What changed and what did not

Unchanged:

- `aequimuta.toml`
- service name `web`
- local port `8080`
- local backend `127.0.0.1:8080`

Chosen outside the service declaration:

- the declaration-ordered publishing intents consumed by `apply`
- the exact publisher token passed to an individual `publish`
- the concrete provider operation
- provider-specific configuration requirements
- visibility and endpoint meaning
- lifetime, recovery behavior, outcome, and status support

That distinction is the purpose of the demo: the service stays neutral while
the mechanism remains concrete.

## Safety notes

- Individual `publish` and `status` commands select one exact desired
  `(service, publisher)` relation. `apply` consumes every desired relation once
  in declaration order.
- `apply` completes project/static preflight before provider mutation and then
  executes provider operations sequentially.
- It does not overwrite a conflicting Tailscale slot or an occupied OpenSSH
  listener.
- It does not treat unknown provider state as absent.
- The Tailscale create path verifies its post-condition.
- The OpenSSH path waits for forwarding acknowledgement.
- Project TOML files are not modified by `publish`, `status`, or `apply`.
- Neither path provides an absolute transaction or concurrent-writer guarantee.
- A failed `apply` does not roll back earlier successes or execute later
  entries.
- `apply` does not delete resources absent from the desired entries and does
  not run continuously in the background.

## Cleanup limitations

Aequimuta currently has no unpublish or delete command.

Stopping the Python process stops only the local backend. It does not remove a
Tailscale Serve mapping or deliberately close the dedicated OpenSSH master.
Use provider-specific, narrowly targeted cleanup procedures approved for your
environment. Do not use a broad reset, and do not assume Aequimuta owns a
pre-existing mapping or listener.
