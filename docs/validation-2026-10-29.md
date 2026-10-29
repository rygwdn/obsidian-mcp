# Validation: authenticated homelab integration

Base: upstream `cdc2e60f8ebf73ded617d029803eebedb455fb6c` (3.2.1).
Branch: `feat/chatgpt-remote-access`. Rust 1.99.1, Linux authoring environment.

## Passed

- `cargo fmt --check` and `git diff --check`.
- `cargo clippy --locked -- -D warnings` (the project's normal CI lint command).
- `cargo check --locked --features embeddings-api`.
- 634 library tests, with the 17 Unix-socket tests below explicitly filtered
  after an initial unrestricted library run demonstrated environment failures.
- All 60 tests in `tests/integration_tests.rs`.
- 12 HTTP/stdio transport tests, with the process-stop test below filtered after
  an initial run demonstrated the PID/signalling environment failure.
- Two Python plugin-packaging tests via
  `python3 -m unittest discover -s tests/python`.
- All test targets compile using the default feature set.

The new Rust tests cover signed JWTs and rejected claims/signatures, public
resource discovery, unauthorized requests, scope checks on requests carrying
existing session IDs, required revisions, exact recovery bytes, stale external
edits, competing writers, and revision propagation through real MCP tool calls.

## Environment restrictions and outstanding checks

The first unrestricted library run passed 633 tests and failed 17 unchanged
Unix-socket tests with `Operation not permitted (os error 1)` on socket bind or
connect. One additional revision wire test was added afterwards, bringing the
filtered passing count to 634. The excluded tests were:

- `client::semantic_daemon::tests::call_retries_when_endpoint_is_temporarily_missing`
- `client::semantic_daemon::tests::ensure_vault_decodes_additive_progress_fields`
- `client::semantic_daemon::tests::ensure_vault_decodes_legacy_v1_response`
- `client::semantic_daemon::tests::health_call_decodes_success_response`
- `client::semantic_daemon::tests::response_missing_jsonrpc_is_rejected`
- `client::semantic_daemon::tests::response_with_mismatched_id_is_rejected`
- `client::semantic_daemon::tests::rpc_errors_preserve_semantic_status_data`
- `daemon::bootstrap::tests::cancelled_manifest_retry_retains_child_and_validates_pid_under_install_lock`
- `daemon::bootstrap::tests::ensure_daemon_reuses_healthy_manifest_endpoint`
- `daemon::bootstrap::tests::health_deadlines_bound_a_connected_silent_peer`
- `daemon::bootstrap::tests::new_daemon_health_timeout_retains_started_process_without_respawning`
- `daemon::server::tests::shutdown_response_is_flushed_before_server_exits`
- `daemon::server::tests::unix_socket_handles_parse_errors_and_health`
- `tools::search::tests::daemon_prefetch_preserves_configured_count`
- `tools::search::tests::daemon_semantic_sends_exclusion_scope_before_ranking`
- `tools::search::tests::daemon_semantic_sends_visible_paths_before_ranking`
- `tools::utility::tests::vault_info_uses_only_passive_daemon_status_and_preserves_provenance`

The initial transport run passed 12 tests and failed
`stop_uses_the_configured_ipv6_host_from_cli_and_environment`: the process-stop
utility discovered a PID that the environment could not signal (`No such
process`), leaving its socket occupied. This test should run on a normal CI host.

The stricter optional `cargo clippy --all-targets -- -D warnings` run flags
existing owned-path comparisons and an obfuscated conditional in upstream test
code under this newer toolchain. The project's normal Clippy invocation passes;
unrelated upstream test lint changes were not included.

Docker image build, real identity-provider discovery/login, Funnel routing,
ChatGPT installation and iPhone behavior have not been exercised. The all-features
local embedding/model-download matrix was not run. These are deployment/CI
follow-ups, not established successes. No personal vault content was accessed.

## Managed Headless Sync follow-up

Added an opt-in service-owned one-shot coordinator and eight regression tests:
seven unit tests covering download/index barriers, forced write passes and read
TTL, serialized cross-process transactions, pre-sync/configuration failure,
post-write failure/restart recovery, timeout, revision conflicts and cancellation;
one HTTP wire test exercises actual tool wrapping, managed annotations and
pending-upload recovery. OAuth tests also check sync_now needs write scope.

Validation after this addition: 641 library tests pass with the same 17 socket
tests filtered; all 60 integration tests and 13 transport tests pass with the
same process-stop test filtered. Formatting, normal Clippy, optional
embeddings-api compilation and both packaging tests pass. Tool schemas include
the two added sync tools. Tests use a fake executable rather than an Obsidian
account. The official CLI source/README (0.0.14) was inspected for command syntax,
configuration modes and locking; live Headless Sync, the optional Node container
image and homelab deployment remain untested. No personal credentials or vault
contents were accessed.

## Vault-owned access controls follow-up

Added `mcpignore.md` deny patterns and immutable `mcp_access` permissions with
filesystem-level checks, protected configuration paths, real-path checks for
symlinks, template checks and per-operation visible-index refresh. Denied notes
are omitted from indexes, file statistics and directory listings. The existing
ignore mechanism remains index-only. See `docs/vault-access.md` for syntax,
performance implications, concurrency and data-retention boundaries.

Seven added regressions cover read/write/navigation denial, read-only mutation
protection, access-field immutability, invalid policies, symlink bypasses,
restricted daily-note templates, policy download before a managed operation,
and HTTP-level reload/search/list/batch behavior. Final checks: 647 library,
60 integration, 14 transport and 2 packaging tests pass with the same 18
previously documented environment exclusions. Formatting, normal Clippy and
embeddings-api compilation pass. Live homelab behavior and the full local
embedding/model-download matrix remain untested.
