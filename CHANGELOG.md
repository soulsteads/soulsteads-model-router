# Changelog

## 0.3.0

Changes since [`v0.1.0`](https://github.com/soulsteads/soulsteads-model-router/releases/tag/v0.1.0).
The proposed `v0.2.0` was a draft, not a published release; this release range
supersedes that draft.

### Highlights

- Rename the shipped production executable from `slotline` to
  `soulsteads-router`, while retaining the `slotline` crate/package name,
  configuration directory and environment-variable namespace.
- Add a no-clobber `service install` command for user and system units. It
  renders literal-safe absolute executable and configuration paths, requires an
  explicit non-root `--run-as` account for system scope, and never enables or
  starts a service.
- Package and smoke-test the renamed release artifact, container entrypoint,
  documentation examples and fixture defaults.
- Add offline policy inspection and routing plans, explicit session continuation
  placement, bounded tool compatibility on managed Responses routes, updated
  routing/status guidance, Docker/Podman deployment, and hardened release asset
  and dependency-notice checks.

### Included commits

All commits on the public default branch from peeled `v0.1.0` through the
release base, plus the release-tree changes above. Unmerged branches are not
included.

- [`f6dd334`](https://github.com/soulsteads/soulsteads-model-router/commit/f6dd33458b06ceedf61d32341d34e9b94eafed4b) test(codex): reuse the CLI access token in the loopback check
- [`7351041`](https://github.com/soulsteads/soulsteads-model-router/commit/73510418935b42f42317c20f214a4d827f1a5ff0) fix(ci): verify package inputs and dependency notices
- [`8ca4a54`](https://github.com/soulsteads/soulsteads-model-router/commit/8ca4a54909477e3a03ebae621f5ed28d830c407c) fix(ci): validate the exact release asset set
- [`ce96b25`](https://github.com/soulsteads/soulsteads-model-router/commit/ce96b259b92903549c004b197027407054587bee) ci: build Model Router release assets
- [`6eb5003`](https://github.com/soulsteads/soulsteads-model-router/commit/6eb50030d67077b0372eb2dd056712e25790e093) feat: add Docker and Podman deployment
- [`9f27d84`](https://github.com/soulsteads/soulsteads-model-router/commit/9f27d848cce4407b27ea8a45fdfc0efd31d0fbb8) test(codex): make storage permission fixture independent of umask
- [`17113e2`](https://github.com/soulsteads/soulsteads-model-router/commit/17113e2d5c772ab554b9670af7a18d235802be81) chore(release): prepare v0.2.0
- [`8b2fc65`](https://github.com/soulsteads/soulsteads-model-router/commit/8b2fc65da586673bdcc6a3a639f60ba2d7738c03) docs: check session-pin loss before migrating traffic
- [`8621acd`](https://github.com/soulsteads/soulsteads-model-router/commit/8621acd55ad293c4140b1e58664f0fc4f5fa650d) Update Model Router policy planning and client compatibility
