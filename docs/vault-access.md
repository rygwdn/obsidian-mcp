# Vault-owned MCP access restrictions

Create `mcpignore.md` at the root of your vault. Edit it in Obsidian; it syncs
like a normal note. All paths are relative to the vault root, with `/` separators.

````markdown
# Notes unavailable to MCP

```mcpignore
Private/
Journal/
Finance/taxes.md
**/*.secret.md
```
````

Only fenced `mcpignore` blocks contain rules. Prose outside those blocks is
ignored. Multiple rule blocks combine. Blank lines and lines beginning with `#`
are ignored. A trailing `/` excludes a directory and its descendants. Globs are
case-sensitive and Unicode-normalized. Negation (`!`), absolute paths, `..`
components and backslashes are rejected. Rules only deny access: frontmatter
cannot override a folder/file denial. Invalid patterns, an unreadable policy,
a policy symlink, an unclosed block or a nonempty file with no rule block blocks
vault operations until corrected. An absent or empty file adds no deny patterns.

## Per-note frontmatter

For Markdown notes, add one of these YAML properties:

```yaml
---
mcp_access: deny
---
```

`deny` blocks reading and modification and omits the note from indexes and
listings. `read-only` permits reading/searching but blocks every mutation,
including appends, patches, moves and deletion. Omit the property for normal
read/write access. Explicit `read-write` is supported but does not override deny
patterns. Unknown values/types fail closed for that note. The frontmatter header
must fit within 64 KiB. These fields are reserved administrative configuration:
MCP cannot add, remove or change them, including through full-note replacement.
It can edit a `read-write` note while preserving that field.

## Enforcement and reload

Enforcement runs in Rust across direct note reads, batch reads, stat/inspection,
all content writes, creation, deletion, both ends of moves, templates, periodic
notes and listing. Existing symlink targets and symlink parents of new files are
checked against their real vault-relative paths as well as the requested path.
MCP directory moves are rejected; move individual notes. Policy and the internal
`.obsidian-mcp` directory are inaccessible through note tools; writes to
`.obsidian` configuration are blocked to prevent changing template/sync settings.
Ordinary notes and read-only notes remain available according to tool scopes.

The MCP entry point rebuilds the visible metadata and BM25 index before each
vault operation, after any managed pre-sync. No restart is required after editing
the policy or a note's access field in Obsidian. This also prevents a stale tag,
backlink or search index from returning a newly denied note. Watcher and initial
index construction apply the same policy. Denied files do not contribute to
visible aggregate file statistics. Explicit batch requests containing denied
paths fail rather than silently treating a protected note as readable.

Index refresh currently scans the vault on each operation, including when sync
is disabled; batch reads reduce this overhead. Embedding updates are asynchronous,
and daemon search receives the visible-path allowlist. Previously generated
embedding caches are not securely erased by changing the policy.

These controls govern MCP access, not Obsidian Sync membership, filesystem access,
or another application's MCP implementation. Headless Sync still synchronizes
the vault. Notes already returned to a conversation cannot be retracted. Text
and wikilinks that you put in an otherwise readable note remain readable, even
if they mention the name or contents of a denied note. Access rules do not redact
such references or sanitize user-authored prose. Local editors and other Sync
clients can change policies; concurrent external edits are not a distributed
transaction. Do not give other untrusted tools unrestricted access to this vault.

The older `.obsidian-mcp/ignore` and `OBSIDIAN_EXCLUDE_PATHS` settings remain index-only
filters and are not access controls. Use `mcpignore.md`/`mcp_access` for restrictions.
