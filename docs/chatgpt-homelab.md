# Personal Obsidian MCP on a homelab

This branch adds an OAuth resource server, revision-checked edits, recovery
copies, and an optional ChatGPT plugin package to the Rust upstream. Your
existing Obsidian Headless Sync remains responsible for synchronization.

Connection: ChatGPT → public HTTPS Funnel → Rust MCP → existing synced folder.
The same server continues to support other MCP clients. A plugin is optional:
it packages the connection plus reusable workflow instructions, not another
vault service.

## Authentication prerequisites

Bring an **OIDC/OAuth identity provider** reachable over HTTPS. The Rust service
validates access tokens; it does not implement a new login server. Your provider
must offer:

- OIDC discovery at `<issuer>/.well-known/openid-configuration`, with an exact
  matching `issuer` and an HTTPS `jwks_uri`.
- JWT access tokens signed using RS256/384/512 or ES256/384. Opaque tokens and
  token introspection are not implemented.
- An audience for this MCP resource, ideally its full public HTTPS `/mcp` URL.
- `vault:read` and `vault:write` scopes. These can be a space-separated `scope`
  claim or an array-valued `scp` claim. Both scopes are needed for the full
  editing workflow. Read-only tokens cannot call mixed read/write tools such
  as `frontmatter` and `periodic`, because those routes are conservatively
  classified as writable.
- An authorization-code client with PKCE. The package script uses a **provided
  public client ID**, with token endpoint authentication `none`. Register the
  actual callback URL supplied by ChatGPT; do not guess it or use a wildcard.
- Support for MCP resource indicators/audience selection so the issued access
  token is intended for this server. Do not supply an ID token as an access token.

If your provider requires a confidential client, DCR or CIMD instead, configure
that supported client flow when adding the custom MCP connection and adjust the
plugin's `auth.client` metadata accordingly. The resource-server code is
independent of the OAuth client-registration mechanism.

This is a setup prerequisite, not a completed IdP integration: choose/configure
your provider and verify its issued token claims before connecting your vault.
No identity-provider account or credentials were created during implementation.

## Configure and start

From `deploy/remote/`:

```bash
cp .env.example .env
mkdir -p data
chmod 700 data
```

Edit `.env`:

| Variable | Meaning |
| --- | --- |
| `VAULT_HOST_PATH` | Absolute path to your existing Headless Sync vault folder. |
| `PUID`, `PGID` | UID/GID with write access to the vault and `data/`. Match your existing sync writer. |
| `OBSIDIAN_PUBLIC_URL` | Public HTTPS **origin**, without `/mcp` or another path. |
| `OBSIDIAN_OAUTH_ISSUER` | Exact issuer string from your provider's OIDC metadata. Trailing slash must match. |
| `OBSIDIAN_OAUTH_AUDIENCE` | Exact expected access-token `aud` value. |
| `OBSIDIAN_OAUTH_SUBJECTS` | Comma-separated allowed `sub` values for your own account(s). Not arbitrary tenant users. |
| `TZ` | Daily-note timezone; defaults to `America/Halifax` in Compose. |
| `OBSIDIAN_TOOLS` | Default `!open_in_obsidian`; retains graph and periodic notes. |

If `data/` belongs to another user, grant the configured UID access to it before
starting. Keep that directory private: it contains unencrypted index data and
recovery copies. The existing vault must also allow this UID to write the
upstream `.obsidian-mcp` configuration folder.

```bash
docker compose up -d --build
curl --fail http://127.0.0.1:37842/health
tailscale funnel status
# After checking that this node/port is not already serving another application:
tailscale funnel --bg http://127.0.0.1:37842
```

Use the exact Funnel origin in `OBSIDIAN_PUBLIC_URL`. If Funnel reports a
different origin, update `.env` and recreate the container. This assumes the
Tailscale daemon runs on the Docker host. Use a separate node/HTTPS port if the
current Funnel route belongs to another app. Do not expose the vault directory
itself. Docker binds only the host's loopback interface.

The container runs with a read-only root filesystem, a non-root UID, dropped
capabilities, and writable mounts only for the vault, index/recovery data and
temporary files. It builds without embedding-model support; start with BM25
search. Resource use is not benchmarked, and the 512 MiB memory cap may need
adjustment for large vaults. Docker deployment must be verified on your host.

## Connect ChatGPT

1. Verify `https://YOUR-HOST/.well-known/oauth-protected-resource/mcp` returns
   the exact resource URL, issuer, and `vault:read`/`vault:write` scopes.
2. Verify an unauthenticated `/mcp` request returns `401` with a Bearer
   `resource_metadata` challenge. It must not expose tools or notes.
3. Use MCP Inspector against `https://YOUR-HOST/mcp`, complete provider login,
   and inspect tools. Test a known note read, a new test note, an edit with its
   current revision, and a deliberately stale edit. Use a disposable test note.
4. Add the custom MCP server through ChatGPT Plugins. Select the OAuth client
   flow configured above and request both scopes for editing. Complete login
   at the identity provider. Never paste a token or Sync password into chat.
5. Start a new conversation and verify search, tags, links and daily notes.
   Explicitly test the web and iPhone surfaces; packaging cannot override
   account or surface availability restrictions.

You can stop here and use direct MCP. To add the bundled workflow skill after
the real endpoint and OAuth flow have been verified, run from the repository root:

```bash
python3 scripts/package-chatgpt-plugin.py \
  https://YOUR-HOST/mcp --client-id YOUR-ACTUAL-PUBLIC-CLIENT-ID
```

The generated `personal-obsidian-vault.zip` contains one portable private
plugin: manifest, workflow skill, and OAuth MCP configuration. It contains no
tokens or vault data and does not deploy or connect the server by itself.
No placeholder endpoint is included in an installable package in this branch.

## Changes to reads and edits

`note_read` preserves its existing first Markdown text block, adds a second
model-readable text block with the revision, and now returns:

```json
{"content":"# My note\n...", "revision":"<sha256 of these exact UTF-8 bytes>"}
```

in `structuredContent`. Each `note_read_many` item also includes `revision`.
Pass it as `expected_revision` to `note_write`, `note_insert`, `note_patch`,
`note_delete`, `note_move` (the source revision), and writable `frontmatter`
actions. A stale edit returns an MCP error with `kind: revision_conflict` and
instructions to reread. Do not blindly retry non-idempotent appends after a
timeout; first inspect the resulting note.

Authenticated HTTP **requires** this argument before routing any mutation of
an existing note. The vault layer checks it while holding the existing mutation
lock and again immediately before atomic replacement. Local legacy stdio and
unauthenticated loopback clients remain compatible: the argument is optional,
but checked whenever supplied. New-note creation already rejects existing paths.

Frontmatter changes and insert/patch writes use atomic replacement too. Other
existing field semantics are retained. Moves do not automatically rewrite all
backlinks. Notes are not a fully transactional database: Headless Sync and other
external writers do not share the mutex, and a small check-to-rename race remains.
Use one writable service instance and retain independent vault backups.

## Recovery copies and security behavior

OAuth HTTP mode requires `OBSIDIAN_BACKUP_DIR` outside the vault. Compose sets
it to `/data/backups`. Previous contents are saved before replacing, deleting,
or moving an existing note, organized as:

```text
backups/<sha256 of relative path>/path.txt
backups/<sha256 of relative path>/<sha256 of content>.md
```

Use `path.txt` to find the original destination. Restore manually after checking
the current note and sync state. Backups are content-addressed, unencrypted, and
have no automatic expiry. They are outside the MCP vault scope. This is not a
complete filesystem snapshot or a substitute for normal backup retention.

Access tokens are checked for signature, issuer, audience, expiry, optional nbf,
allowed subject and scope on **every** request. Only asymmetric signing
algorithms are accepted; request-supplied key URLs are never followed. JWKS keys
are cached for five minutes, with a serialized/rate-limited refresh for unknown
key IDs. Provider outages fail closed. Revocation of a JWT at the provider is
not detected until its expiry unless its signing key disappears after refresh;
use short-lived access tokens.

Read-only scope enforcement happens before MCP dispatch as well as in tool
discovery, so a read token cannot use a session initialized with write
permissions to mutate notes. Client tool-filter headers can narrow the
server's allowed tools, never enable disabled ones. `/health` exposes only
basic service status in OAuth mode. Resource metadata is deliberately public.

Non-loopback HTTP refuses to start without OAuth. Legacy unauthenticated
loopback HTTP is retained for local clients; do not put Funnel in front of that
mode. OAuth settings are all-or-nothing and validated at startup. TLS terminates
at Funnel; authorization remains in the Rust process.

Keep the vault and backup mounts owned by trusted local users. Path checks do
not protect against malicious local filesystem races. No JavaScript, desktop
workspace commands, Dataview or Templater execution is added by this branch.

## Updates and validation

Refresh ChatGPT's connection metadata after changing tool schemas or server
instructions. Existing clients must reconnect to learn the revision field.
For this fork, deploy rebuilt versions of your branch. The upstream
`obsidian-mcp upgrade` command installs from upstream package sources; do not
use it to update a customized fork deployment.

Run `cargo fmt --check`, `cargo clippy -- -D warnings`, and
`cargo test`. Optional embedding features keep their upstream configuration.
The new tests cover JWT rejection, scope enforcement, metadata, revision
requirements, competing writers, external edits and exact recovery bytes.

Official integration references:

- https://developers.openai.com/plugins/build/auth
- https://developers.openai.com/plugins/deploy/connect-chatgpt
- https://tailscale.com/docs/reference/tailscale-cli/funnel


## Optional service-managed Headless Sync

Set `OBSIDIAN_SYNC_MODE=managed` to run the official CLI before every vault read,
search, graph, metadata or periodic-note operation, and before **and after** each
mutation. The CLI has no lightweight remote-change probe; each barrier runs
`ob sync-status --path <vault> --json` to validate bidirectional configuration,
then a one-shot `ob sync --path <vault>`. It never uses `--continuous`.
`sync_status` reports health without contacting Sync; `sync_now` forces a pass.
Disabled mode (default) leaves sync ownership with your existing service.

Stop your existing continuous-sync service **before enabling managed mode**.
The MCP service serializes its own complete sync/operation/sync transactions,
including across processes sharing the same vault path. Obsidian's CLI lock
protects against other sync processes; a collision fails the operation. It
never kills your other service or deletes its lock. Other Obsidian clients may
still edit concurrently, so normal Sync conflict handling and revision checks
remain necessary; this is not a distributed transaction.

For the supplied container:

1. Set `OBSIDIAN_IMAGE_TARGET=managed-sync` and `OBSIDIAN_SYNC_MODE=managed` in
   `.env`. This optional image installs Node 22 and `obsidian-headless@0.0.14`.
2. Stop the old sync process, then run `docker compose build` from `deploy/remote`.
3. Authenticate interactively with
   `docker compose run --rm --entrypoint ob obsidian-mcp login`.
4. Run `docker compose run --rm --entrypoint ob obsidian-mcp sync-list-remote`,
   then `docker compose run --rm --entrypoint ob obsidian-mcp sync-setup --vault <REMOTE_ID> --path /vault`.
   Follow CLI prompts on your own terminal. Never paste credentials into chat.
   Run setup inside this container: host-path configuration does not identify
   the container's `/vault` path. Confirm the chosen remote and existing local
   vault match before setup.
5. Verify `docker compose run --rm --entrypoint ob obsidian-mcp sync-status --path /vault --json`
   shows `bidirectional`, and run one manual `sync --path /vault` before
   `docker compose up -d`.

CLI authentication/configuration persists under `/data/config/obsidian-headless`.
Protect this directory as a secret and keep it out of the vault and Git. The
existing data bind mount must be writable by PUID/PGID; credentials are not
included in the image. Do not run a separate scheduler against this same vault.
For a native installation, install Node >=22 and the official CLI, authenticate
and set up the exact vault path as the service OS user. Set
`OBSIDIAN_HEADLESS_BIN` if `ob` is not on that user's PATH.

The default `OBSIDIAN_SYNC_READ_TTL_MS=0` checks every read. Optionally set up to
60000 ms to reuse a recent successful barrier for reads; writes always force
both passes. Batch reads reduce sync overhead. `OBSIDIAN_SYNC_TIMEOUT_SECS`
defaults to 90 (range 1–3600), applied per CLI command and coordinator lock wait.
Subprocess input is closed and output suppressed to avoid leaking credentials
or note paths. Login/setup are separate interactive administrative operations.

A failed pre-sync blocks the operation. A failed post-sync returns the local
write result with an explicit **upload pending; do not repeat the write**
warning. A persistent marker survives restarts; use `sync_status` and retry
`sync_now` to upload it. Cancellation during a write also leaves the marker.
This marker conservatively means a mutation may need uploading, not a precise
remote queue count. A successful sync clears it. Disk content, tags, backlinks
and BM25 search are refreshed before the operation proceeds; semantic embedding
updates remain asynchronous. Each real pass scans/reindexes the vault, so large
vaults may benefit from batch reads or the optional read TTL.

Managed read calls can upload pre-existing local changes because sync is
bidirectional. Tool annotations reflect this external write behavior. OAuth
`vault:read` still limits requested note operations; it does not make underlying
Sync housekeeping download-only. `sync_now` requires `vault:write` remotely.
