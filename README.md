<p align="center">
  <img src="assets/banner.svg" alt="Soulsteads Model Router" width="800" />
</p>

<p align="center">
  <strong>Your local models. One endpoint. Blazing fast. No BS.</strong><br/>
  Route OpenAI-compatible requests across your own inference servers without overbooking them.
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache%202.0-blue" alt="License: Apache 2.0" /></a>
  <img src="https://img.shields.io/badge/version-0.1.0-orange" alt="Version 0.1.0" />
  <img src="https://img.shields.io/badge/rust-2024-informational?logo=rust" alt="Rust 2024" />
  <img src="https://img.shields.io/badge/status-early%20release-yellow" alt="Status: early release" />
</p>

## Why this router?

You have more than one local inference server. Your clients should not need to
know which machine is free, which backend supports a tool call, or how many
requests that GPU can safely handle.

**Soulsteads Model Router** puts stable model aliases in front of your servers.
Its managed routing checks capabilities and available capacity before choosing
a backend. It is written in Rust and builds as the `slotline` binary.

```text
Agents and OpenAI-compatible clients
                 │
       Soulsteads Model Router
       auth · capabilities · capacity
          ┌──────┼──────┐
       server A server B embeddings
```

- **One catalog:** restrict each client key to the aliases and routes it needs.
- **Capacity-aware routing:** prefer idle backends, then fewer occupied slots
  and more free slots. Adapters for one physical backend share its limit.
- **Capability checks:** route supported tools, structured output, inline images,
  Responses, and embeddings only to eligible backends.
- **Bounded retries:** retry eligible failures before response exposure, never
  replay a stream after handoff.
- **Private credentials:** use protected key files or an Argon2id verifier store
  with issue, rotation, and revocation.
- **Operator visibility:** inspect health, admission, affinity, telemetry, and
  request timing without logging prompt text by default.

It does **not** host models, manage model loading, or provide cloud subscriptions.
It does not depend on LiteLLM. The optional [passthrough mode](docs/full-passthrough.md)
**bypasses managed capacity and safety checks**; leave it off unless you need and
accept its different contract.

## Before you start

> [!IMPORTANT]
> **Early release, not a universal OpenAI drop-in.** You need a Linux/Unix host,
> an existing local inference server, and an assigned Tailnet IPv4 address.
> The current validator rejects loopback, LAN, and wildcard listener binds and
> reserves port `4001`. The gateway serves HTTP, not built-in TLS: use a protected
> network and do not expose it directly to the internet.

Support depends on your exact backend and declared route capabilities. See
[compatibility and limits](docs/COMPATIBILITY.md) before connecting clients.

## Get started

### Ask your agent

Paste this into an agent that can work in the repository checkout:

```text
Set up Soulsteads Model Router from this repository.

1. Read README.md, docs/SETUP.md, and config/slotline.example.yaml.
2. Check the prerequisites. Ask which existing local inference server to use.
3. Build with Cargo.lock. Use the backend's exact model ID and tested
   context/concurrency limits. Declare only capabilities we have verified.
4. Generate credentials into a private file, never into chat or Git. Validate
   the configuration before starting the gateway.
5. Ask before installing or starting a service or changing anything outside
   this checkout. Do not replace an existing proxy or migrate clients silently.
6. Verify authentication, the catalog, and a real completion. Report what
   passed and what remains untested.
```

### Set it up yourself

Follow the **[setup guide](docs/SETUP.md)** to build, prepare private credentials,
configure a backend, and make your first request. The
[example policy](config/slotline.example.yaml) starts with one enabled text route;
it needs your actual bind address, key path, model ID, and verified limits.
No model server or prebuilt binary is bundled.

## Documentation

- [Setup](docs/SETUP.md): build, configuration, first request, optional user service.
- [Compatibility](docs/COMPATIBILITY.md): endpoints, supported request shapes, limits.
- [Operations](docs/OPERATIONS.md): status, health, deadlines, reload, safe updates.
- [Keys](KEYS.md): verifier store, credential lifecycle, backup, and restore.
- [Development](docs/DEVELOPMENT.md): tests, known fixture limitations, release checks.
- [Migration](MIGRATION.md): move an existing client safely, with rollback.
- [All documentation](docs/README.md): observability, advanced modes, and integrations.

## License and release status

Licensed under the [Apache License 2.0](LICENSE).

This branch prepares the project for public release. A
[Git-history privacy review](docs/DEVELOPMENT.md#before-publishing) remains required;
cleaning current files does not remove data from earlier commits or hosted copies.
