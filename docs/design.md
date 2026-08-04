# Aequimuta design

[README](../README.md) ·
[Tailscale Serve TCP guide](providers/tailscale-serve-tcp.md) ·
[OpenSSH reverse TCP guide](providers/openssh-reverse-tcp.md) ·
[Demo](demo.md)

## Design goals

Aequimuta is designed around a narrow boundary: describe a local service
without embedding the mechanism that exposes it.

> **Neutrality applies to the service declaration, not to the operational
> semantics of every publisher.**

The current goals are:

- keep the service declaration stable and publisher-neutral
- make the concrete publishing mechanism replaceable without redefining the
  service
- preserve provider-specific lifecycle, visibility, status, and conflict
  semantics
- add capabilities as small, observable, testable changes
- validate project input strictly
- refuse destructive or ambiguous operations by default
- keep credentials and trust in machine-local provider environments

These goals define the boundary of the current CLI. They do not imply that all
publishers implement one universal runtime model.

## The coupling problem

A provider-specific publishing workflow often combines all of the following:

- the service name
- the local listener port
- a provider command
- a public or private listener address
- credential and trust configuration
- process or provider lifetime
- collision and replacement behavior

That combination couples provider-specific concerns to the service's
operational definition and workflow. Replacing the provider then requires
changing publishing configuration and operations even when the local service
itself has not changed.

Aequimuta separates the stable service identity from the concrete publication
mechanism. It does not erase the operational differences that remain.

## Three layers

### 1. Service declaration

`aequimuta.toml` answers: **What local service is being published?**

```toml
[[services]]
name = "web"
port = 8080
```

The current service contract contains only an exact name and a local TCP port.
It has no publisher-specific field, public port, protocol selector, credential,
or remote address.

### 2. Publishing intent

`aequimuta.publish.toml` answers: **Which concrete capability should publish
this service?**

```toml
[[publications]]
service = "web"
publisher = "tailscale-serve-tcp"

[[publications]]
service = "web"
publisher = "openssh-reverse-tcp"
```

Each entry is an exact `(service, publisher)` relation. The same service may
select both current publishers. The file contains desired intent, not observed
provider state, credentials, ownership, or a request to apply every entry at
once.

### 3. Provider-specific configuration and runtime effects

Concrete mechanisms may require information that does not belong in the core
service or publishing intent.

Tailscale Serve TCP currently needs no separate project configuration file. It
uses the local Tailscale environment and maps the declared port on the current
node to `127.0.0.1:<service.port>`.

OpenSSH needs an SSH destination, remote user, requested remote bind, and fixed
remote port. Its nonsecret configuration therefore lives in
`aequimuta.openssh-reverse-tcp.toml`. Credentials and host trust remain in the
machine-local OpenSSH environment. At runtime, an owner-only XDG control socket
represents a dedicated background ControlMaster session; this is private
session state, not a project runtime database.

## Service declaration

The service schema is intentionally small:

- `name` is a nonempty exact string without leading or trailing whitespace or
  control characters
- `port` is an integer from `1` through `65535`
- names must be unique by exact string equality
- different services may declare the same port at the core layer
- unknown root and service fields are rejected

Different concrete mechanisms can impose different applicability constraints.
For example, two desired Tailscale Serve TCP publications using the same
declared port converge on the same node slot and are rejected at operation
time. That provider-specific collision rule does not change the core service
schema.

## Publishing intent

The publishing intent schema is also small:

- `service` must exactly reference a declared service name
- `publisher` is a lowercase ASCII capability token
- exact `(service, publisher)` duplicates are rejected
- unknown root and publication fields are rejected

Lexically valid publisher tokens are not proof of runtime support.
`validate-publishing` checks syntax and service relationships; `publish` or
`status` rejects unsupported operational tokens. This keeps parsing a desired
intent separate from dispatching a concrete mechanism.

## Concrete provider configuration and runtime effects

Provider-specific configuration exists only when the selected mechanism needs
it.

For OpenSSH, every active `openssh-reverse-tcp` desired service must resolve to
one provider configuration entry. The provider file records reproducible,
nonsecret inputs, while authentication material and host trust stay outside the
project. The selected publish operation creates or reuses a dedicated SSH
ControlMaster and asks that session to add an exact remote forward.

For Tailscale, the concrete operation observes and, only when absent, changes a
single current-node Serve TCP slot. The resulting Serve configuration belongs
to the local Tailscale environment rather than to an Aequimuta process.

These runtime effects differ because the providers differ. No hidden runtime
state database is used to make them appear identical.

## Neutrality without false equivalence

Aequimuta keeps the **service definition** neutral. Provider behavior remains
concrete.

The current mechanisms already differ in important ways:

| Concern | Tailscale Serve TCP | OpenSSH reverse TCP |
| --- | --- | --- |
| External resource identity | Current node and declared TCP port | SSH destination and fixed remote listener port |
| Visibility | Tailnet-private | Determined by requested bind, SSH policy, and remote networking |
| Lifetime | Tailscale-managed Serve configuration | Background SSH session |
| Observation | Structured Serve state is available | No authoritative exact remote-forward snapshot |
| Existing exact state | Can be classified as already satisfied | Repeat request is acknowledged as ensured |
| Success outcome | Created or Already satisfied | Ensured |
| Collision identity | Current-node TCP port | Lexical SSH destination plus fixed listener port |
| Extra project configuration | None | Separate nonsecret OpenSSH file |

Tailscale's `Created` and `Already satisfied` distinction is not projected onto
OpenSSH. OpenSSH's `Ensured` result means that the fixed forward request received
SSH protocol acknowledgement through the current session. It does not prove
that the listener was newly created, that Aequimuta owns it, or that the
requested address is the address the server actually bound.

Likewise, the Tailscale four-state relation is not presented as an OpenSSH
status model. Different visibility and lifetime semantics are part of the
public behavior, not details to hide behind a common label.

## Desired state and observed state

The current CLI distinguishes three concepts:

1. `aequimuta.toml` describes the desired local service identity and port.
2. `aequimuta.publish.toml` describes desired exact publication relations.
3. Tailscale `status` observes one selected relation between that intent and a
   concrete Serve mapping.

For `tailscale-serve-tcp`, the observed relation is one of `absent`,
`satisfied`, `conflict`, or `indeterminate`. This relation says nothing about
local application health or end-to-end reachability.

OpenSSH currently has no equivalent authoritative observed relation. Checking
that a local multiplexed SSH master is alive while publishing is not the same
as obtaining a non-mutating snapshot of exact remote forwarding state.

Aequimuta does not currently run a universal reconciliation loop or infer that
every desired entry has been applied.

## Safety and non-destructive behavior

The current safety model is based on exact identity and refusal:

- parse known TOML fields strictly
- select exact, case-sensitive service and publisher identities
- reject ambiguous desired provider slots before mutation
- reject unsupported operational publisher tokens
- classify unknown Tailscale state as indeterminate rather than absent
- avoid overwriting incompatible Serve, Funnel, or remote listener state
- verify the exact Tailscale post-condition after creation
- require OpenSSH forwarding acknowledgement before success
- avoid broad reset, cleanup, takeover, or false ownership claims
- preserve project TOML bytes and project directory entries during `publish`
  and `status`

Tailscale state can still change between Aequimuta's observation and provider
mutation. The current implementation assumes a single writer during that
window and does not claim an atomic transaction.

OpenSSH control state is private to the current user, but publication is still
bound to an SSH session. Aequimuta does not use that socket as proof of durable
remote ownership.

## Why there is no generic Publisher abstraction yet

The current code has two concrete mechanisms and no generic Publisher trait,
registry, plugin loader, or shared result algebra. This is deliberate, but not
an unconditional rejection of abstraction.

Implementing the concrete paths first reveals which behavior is actually
shared and which remains mechanism-specific. The current differences include:

- external resource identity
- status observability
- lifetime and recovery behavior
- endpoint meaning and visibility
- collision rules
- success outcomes
- provider-specific configuration

An abstraction introduced before these differences were observed would risk
making weak promises or erasing useful information. Whether a smaller shared
boundary is justified remains a design question.

## Current concrete mechanisms

### `tailscale-serve-tcp`

Publishes a current-node, tailnet-private raw TCP Serve mapping from the
declared port to `127.0.0.1:<service.port>`. It supports exact mapping status and
distinguishes a created mapping from an already-satisfied mapping.

See the [Tailscale Serve TCP guide](providers/tailscale-serve-tcp.md).

### `openssh-reverse-tcp`

Requests a fixed IPv4 remote listener and forwards it through a dedicated
background OpenSSH ControlMaster to `127.0.0.1:<service.port>`. Success is
session-backed `Ensured`; authoritative read-only status, automatic reconnect,
and reboot recovery are not provided.

See the [OpenSSH reverse TCP guide](providers/openssh-reverse-tcp.md).

## Explicit non-goals

The current design does not provide:

- a generic Publisher plugin framework
- project-wide apply, reconciliation, or lifecycle management
- ownership or provenance inference for existing provider resources
- cross-provider cutover, continuity guarantees, or conflict resolution
- credential or secret storage in project files
- network policy, routing, firewall, NAT, or DNS automation
- HTTP, HTTPS, UDP, Funnel, or arbitrary command publishing

## Current limitations

- only local TCP services are modeled
- both current backends are fixed to IPv4 loopback
- one exact publication is selected per `publish` or `status` command
- there is no unpublish or delete command
- there is no durable ownership or runtime database
- status exists only for Tailscale Serve TCP
- OpenSSH publication has no reconnect, retry, supervision, or reboot recovery
- no third publisher is implemented

## Future questions, without promises

The following remain design questions rather than commitments:

- whether concrete experience supports a useful shared Publisher boundary
- whether durable ownership is necessary for safe removal or replacement
- whether project-wide observation or reconciliation can preserve exact
  provider semantics
- which additional service fields are justified by concrete capabilities
- how lifecycle and diagnostic output should evolve without inventing false
  equivalence

Any answer must be based on implemented, observable behavior rather than a
promise about an unimplemented provider.
