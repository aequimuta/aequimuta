# Tailscale Serve TCP

[README](../../README.md) · [Design](../design.md) ·
[OpenSSH reverse TCP guide](openssh-reverse-tcp.md) · [Demo](../demo.md)

## Overview

The exact capability token is:

```text
tailscale-serve-tcp
```

This capability publishes a local TCP service through a current-node,
tailnet-private Tailscale Serve raw TCP mapping. It is not Funnel, an HTTP or
HTTPS proxy, TLS termination, or a Tailscale Service VIP.

## Semantics

For a selected service with declared port `<port>`, the concrete relation is:

```text
current Tailscale node TCP <port> -> 127.0.0.1:<port>
```

The listener and local backend use the same declared port. The successful
endpoint reported by `publish` is based on the current node's trusted MagicDNS
name:

```text
tcp://<tailscale-dns-name>:<port>
```

That endpoint is current provider information, not an immutable node identity
or a guarantee that the local application is healthy.

## Prerequisites

Publishing requires:

- a `tailscale` executable available on `PATH`
- a running, online, authenticated local Tailscale client and daemon
- enabled MagicDNS and an observable current-node DNS name
- a TCP listener reachable at `127.0.0.1:<service.port>`
- structured Serve state in which the selected slot can be interpreted safely

No exact Tailscale version is part of the current Aequimuta contract.

The read-only `status` path has a narrower prerequisite set. It invokes only
`tailscale serve status --json`; it does not require a reachable local backend,
look up the MagicDNS endpoint, or inspect application health.

## Project configuration

Declare the service in `aequimuta.toml`:

```toml
[[services]]
name = "web"
port = 8080
```

Select the capability in `aequimuta.publish.toml`:

```toml
[[publications]]
service = "web"
publisher = "tailscale-serve-tcp"
```

There is no separate Tailscale provider configuration file in the current
project contract. Credentials, machine identity, and tailnet state come from
the machine-local Tailscale environment.

## Publishing

Start the local backend, then run:

```sh
aequimuta publish web tailscale-serve-tcp
```

The command validates the two project files, selects the exact desired tuple,
checks Tailscale and the local backend, and observes the selected Serve slot.

If the slot is absent, Aequimuta creates the exact mapping and verifies the
structured post-condition. A successful creation for the example above has
this output shape:

```text
Published web via tailscale-serve-tcp at tcp://<tailscale-dns-name>:8080
```

This is the **Created** outcome even though the literal CLI verb is
`Published`.

If an exact compatible mapping already exists, Aequimuta performs no mutation
and reports:

```text
Publication already satisfied for web via tailscale-serve-tcp at tcp://<tailscale-dns-name>:8080
```

Aequimuta does not claim that it created or owns a mapping found at invocation
time.

## Doctor

Project-wide readiness diagnostics include the Tailscale branch only when at
least one desired publication uses this capability:

```sh
aequimuta doctor
```

For one doctor invocation, Aequimuta queries `tailscale status --json` at most
once for the existing client and endpoint-derivation prerequisites, then
queries `tailscale serve status --json` at most once and reuses that structured
snapshot for all desired Tailscale ports. It does not issue a Serve or Funnel
mutation.

The existing concrete classifier is projected into readiness results: an
absent or exactly satisfied slot is `PASS`, while a conflicting or
indeterminate slot is fail-closed `FAIL`. Doctor displays only the performed
check result; use `status <service> tailscale-serve-tcp` when the exact
four-state relation is needed.

Doctor also performs the project-wide local TCP connect for each unique desired
backend. These TCP and daemon observations can be visible to the application or
provider environment even though the command does not change durable project
or provider configuration. A successful diagnostic does not guarantee a later
apply because state can change after observation.

## Status

Observe the selected desired relation without provider mutation:

```sh
aequimuta status web tailscale-serve-tcp
```

The exact output form is:

```text
Publication status for web via tailscale-serve-tcp: <relation>
```

Status performs one `tailscale serve status --json` observation. It does not
connect to `127.0.0.1:8080`, call endpoint discovery, or change Serve or Funnel
configuration.

## Status values

The current relation has four possible values:

- `absent` — no relevant mapping exists at the selected current-node slot
- `satisfied` — the slot contains the exact tailnet-private raw TCP mapping to
  `127.0.0.1:<service.port>` with no incompatible same-port mode
- `conflict` — the slot contains safely classified incompatible Serve or Funnel
  state
- `indeterminate` — structured state exists, but the selected slot cannot be
  interpreted safely

All four relations are successful observations: stdout contains one status
line, stderr is empty, and the exit code is `0`.

A provider spawn failure, a nonzero provider command, or malformed output from
which no structured JSON can be obtained is a command failure instead. That
path uses an error line on stderr and exit code `1`; it is not reported as one
of the four relations.

Status describes the external provider mapping relation. A mapping can be
`satisfied` while the local backend is stopped. Status does not guarantee
endpoint reachability or application health.

## Safety and conflict behavior

The capability uses narrow, fail-closed behavior:

- exact, case-sensitive service and publisher selection
- rejection of two desired Tailscale publications converging on the same
  declared port
- creation only when the selected slot is observed as absent
- no overwrite of a different target, HTTP, HTTPS, TLS, PROXY protocol,
  enabled Funnel, foreground, or unknown state
- exact structured post-condition verification after creation
- no broad reset, `off`, Funnel mutation, or unrelated slot mutation
- no assumption that unknown or malformed typed state is absent
- no modification of project TOML files or project directory entries

If the provider mutation process fails, Aequimuta may reobserve the slot for a
diagnostic category, but it does not convert that failed operation into success
or run an automatic rollback.

## Lifecycle

The mapping is stored as Tailscale-managed Serve configuration and is not tied
to the lifetime of the Aequimuta CLI process. Aequimuta does not run a daemon or
reconnect engine around it. Reboot and recovery behavior remains provider-
managed rather than an additional Aequimuta guarantee.

External Serve and Funnel state is assumed to have a single writer during the
preflight-to-mutation window. Aequimuta checks before and after mutation, but it
does not eliminate that race atomically.

## Limitations

- tailnet-private raw TCP only
- backend fixed to `127.0.0.1:<service.port>`
- listener port fixed to the same declared service port
- no unpublish or delete command
- no ownership or provenance database
- no absolute concurrent-writer guarantee
- no Funnel mutation or publication
- no HTTP or HTTPS proxy mode
- no Tailscale Services support

## Troubleshooting boundaries

Before publishing, verify the local backend, the local Tailscale client state,
MagicDNS, and the intended current-node Serve slot. Aequimuta reports sanitized
operation failures rather than exposing raw provider output.

For a `conflict` or `indeterminate` result, inspect the provider state and decide
which configuration should remain. Aequimuta will not replace or broadly reset
state it cannot safely interpret. Because there is no Aequimuta unpublish
command, use only narrow, provider-specific cleanup that you have independently
verified; do not use a broad reset as an Aequimuta workflow.
