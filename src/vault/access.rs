//! Vault-owned MCP access controls. Policy is read from disk, never from agent instructions.
use super::{frontmatter, path};
use crate::error::{VaultError, VaultResult};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::{
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

pub const POLICY: &str = "mcpignore.md";
fn denied() -> VaultError {
    VaultError::Other("MCP access denied by vault policy".into())
}
fn invalid() -> VaultError {
    VaultError::Other(
        "Invalid MCP access policy; fix mcpignore.md or mcp_access in Obsidian".into(),
    )
}

pub struct Policy {
    denies: GlobSet,
}
impl Policy {
    pub fn load(root: &Path) -> VaultResult<Self> {
        // Never follow a policy symlink, including when it targets outside the vault.
        if fs::symlink_metadata(root.join(POLICY)).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(invalid());
        }
        let source = match fs::read_to_string(root.join(POLICY)) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(_) => return Err(invalid()),
        };
        let mut builder = GlobSetBuilder::new();
        let mut inside = false;
        let mut found = false;
        for line in source.lines() {
            let line = line.trim();
            if line == "```mcpignore" {
                if inside {
                    return Err(invalid());
                }
                inside = true;
                found = true;
                continue;
            }
            if inside && line == "```" {
                inside = false;
                continue;
            }
            if !inside || line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('!')
                || line.starts_with('/')
                || line.contains('\\')
                || line.split('/').any(|p| p == "..")
            {
                return Err(invalid());
            }
            let pattern = if let Some(dir) = line.strip_suffix('/') {
                format!("{dir}/**")
            } else {
                line.into()
            };
            builder.add(Glob::new(&path::canonical_unicode_key(&pattern)).map_err(|_| invalid())?);
        }
        if inside || (!source.trim().is_empty() && !found) {
            return Err(invalid());
        }
        Ok(Self {
            denies: builder.build().map_err(|_| invalid())?,
        })
    }

    fn blocked(&self, relative: &Path) -> bool {
        let mut current = Some(relative);
        while let Some(p) = current {
            let key = path::canonical_unicode_key(&p.to_string_lossy());
            if self.denies.is_match(&key) || self.denies.is_match(format!("{key}/")) {
                return true;
            }
            current = p.parent().filter(|p| !p.as_os_str().is_empty());
        }
        false
    }

    pub fn check(&self, root: &Path, requested: &Path, write: bool) -> VaultResult<PathBuf> {
        let resolved = path::resolve_for_write(root, requested)?;
        let canonical_root = root.canonicalize()?;
        let canonical = match resolved.absolute.canonicalize() {
            Ok(p) => p
                .strip_prefix(&canonical_root)
                .map_err(|_| denied())?
                .to_path_buf(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Resolve an existing symlink parent for new files, too.
                let mut parent = resolved.absolute.as_path();
                let mut tail = Vec::new();
                while !parent.exists() {
                    tail.push(parent.file_name().ok_or_else(denied)?.to_os_string());
                    parent = parent.parent().ok_or_else(denied)?;
                }
                let parent = parent.canonicalize()?;
                let mut target = parent
                    .strip_prefix(&canonical_root)
                    .map_err(|_| denied())?
                    .to_path_buf();
                for part in tail.into_iter().rev() {
                    target.push(part);
                }
                target
            }
            Err(_) => return Err(denied()),
        };
        for p in [&resolved.relative, &canonical] {
            let first = p
                .components()
                .next()
                .map(|c| c.as_os_str().to_string_lossy().to_string());
            if p == Path::new(POLICY)
                || first.as_deref() == Some(".obsidian-mcp")
                || (write && first.as_deref() == Some(".obsidian"))
                || self.blocked(p)
            {
                return Err(denied());
            }
        }
        if resolved.absolute.is_file()
            && resolved
                .absolute
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("md"))
        {
            match disk_access(&resolved.absolute)?
                .as_ref()
                .and_then(|v| v.as_str())
            {
                Some("deny") => return Err(denied()),
                Some("read-only") if write => return Err(denied()),
                _ => {}
            }
        }
        Ok(resolved.relative)
    }
}

// Only inspect a bounded YAML header. Batch reads must not load a large note's
// body merely to decide whether it is accessible.
fn disk_access(file: &Path) -> VaultResult<Option<serde_json::Value>> {
    let mut input = fs::File::open(file).map_err(|_| denied())?;
    let mut prefix = [0u8; 4];
    match input.read_exact(&mut prefix) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(_) => return Err(denied()),
    }
    if &prefix != b"---\n" && &prefix != b"---\r" {
        return Ok(None);
    }
    let mut text = prefix.to_vec();
    if &prefix == b"---\r" {
        let mut byte = [0u8; 1];
        if input.read_exact(&mut byte).is_err() || byte[0] != b'\n' {
            return Ok(None);
        }
        text.push(byte[0]);
    }
    let mut reader = BufReader::new(input.take(65537));
    loop {
        let mut line = Vec::new();
        let read = reader.read_until(b'\n', &mut line).map_err(|_| denied())?;
        if text.len() + line.len() > 65536 {
            return Err(invalid());
        }
        let end = line == b"---\n" || line == b"---\r\n" || line == b"---";
        text.extend_from_slice(&line);
        if end || read == 0 {
            break;
        }
    }
    access_field(std::str::from_utf8(&text).map_err(|_| invalid())?)
}

fn access_field(text: &str) -> VaultResult<Option<serde_json::Value>> {
    // Preserve upstream handling of unrelated malformed YAML. A malformed access
    // field must never be interpreted as an unrestricted note.
    let yaml = frontmatter::extract_raw_frontmatter(text).map(|(raw, _)| raw);
    if yaml.is_none() && text.starts_with("---") && text.contains("mcp_access") {
        return Err(invalid());
    }
    let value = match frontmatter::parse_frontmatter(text) {
        Ok(v) => v.and_then(|v| v.get("mcp_access").cloned()),
        Err(_) if yaml.is_some_and(|raw| raw.contains("mcp_access")) => return Err(invalid()),
        Err(_) => None,
    };
    if let Some(value) = &value
        && !matches!(value.as_str(), Some("deny" | "read-only" | "read-write"))
    {
        return Err(invalid());
    }
    Ok(value)
}

pub fn check(root: &Path, relative: &Path, write: bool) -> VaultResult<PathBuf> {
    Policy::load(root)?.check(root, relative, write)
}
pub fn readable(root: &Path, relative: &Path) -> bool {
    check(root, relative, false).is_ok()
}

pub fn check_content(root: &Path, relative: &Path, proposed: &str) -> VaultResult<()> {
    check(root, relative, true)?;
    let resolved = path::resolve_for_write(root, relative)?;
    let old = match fs::read_to_string(&resolved.absolute) {
        Ok(text) => access_field(&text)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err(denied()),
    };
    if old != access_field(proposed)? {
        return Err(denied());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_helpers::tantivy_config, vault::Vault};
    fn policy(root: &Path, patterns: &str) {
        fs::write(
            root.join(POLICY),
            format!("# Access\n```mcpignore\n{patterns}\n```\n"),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn denies_direct_reads_writes_and_navigation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("Private")).unwrap();
        fs::write(
            root.join("Private/secret.md"),
            "classified #hidden [[public]]",
        )
        .unwrap();
        fs::write(root.join("public.md"), "public").unwrap();
        policy(root, "Private/");
        let vault = Vault::open(&tantivy_config(root)).await.unwrap();
        for name in [
            "Private/secret.md",
            "Private/../Private/secret.md",
            "mcpignore.md",
        ] {
            assert!(vault.read_note(Path::new(name)).is_err());
            assert!(vault.write_note(Path::new(name), "replace").is_err());
            assert!(vault.delete_note(Path::new(name)).is_err());
            assert!(
                vault
                    .move_note(Path::new(name), Path::new("stolen.md"))
                    .is_err()
            );
        }
        assert!(
            vault
                .create_note(Path::new("Private/new.md"), "new", None)
                .is_err()
        );
        assert!(
            vault
                .move_note(Path::new("public.md"), Path::new("Private/public.md"))
                .is_err()
        );
        assert!(vault.list_files(Path::new("Private"), true, None).is_err());
        let listing = vault.list_files(Path::new(""), true, None).unwrap();
        assert_eq!(listing, vec![PathBuf::from("public.md")]);
        assert!(vault.search_text("classified", 100).unwrap().is_empty());
        assert!(vault.search_by_tag("hidden").unwrap().is_empty());
        assert!(vault.backlinks(Path::new("public.md")).unwrap().is_empty());
        assert_eq!(vault.vault_stats().unwrap().total_files, 1);
        assert_eq!(
            fs::read_to_string(root.join("Private/secret.md")).unwrap(),
            "classified #hidden [[public]]"
        );
    }

    #[tokio::test]
    async fn read_only_and_access_fields_cannot_be_changed_by_mutations() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("ro.md"),
            "---\nmcp_access: read-only\n---\nreference",
        )
        .unwrap();
        fs::write(root.join("deny.md"), "---\nmcp_access: deny\n---\nsecret").unwrap();
        fs::write(
            root.join("rw.md"),
            "---\nmcp_access: read-write\n---\neditable",
        )
        .unwrap();
        let vault = Vault::open(&tantivy_config(root)).await.unwrap();
        assert!(vault.read_note(Path::new("ro.md")).is_ok());
        assert!(vault.read_note(Path::new("deny.md")).is_err());
        for name in ["ro.md", "deny.md"] {
            assert!(vault.append_note(Path::new(name), "append").is_err());
            assert!(vault.prepend_note(Path::new(name), "prepend").is_err());
            assert!(vault.write_note(Path::new(name), "overwrite").is_err());
            assert!(vault.delete_note(Path::new(name)).is_err());
            assert!(
                vault
                    .move_note(Path::new(name), Path::new("moved.md"))
                    .is_err()
            );
            assert!(
                vault
                    .remove_frontmatter_field(Path::new(name), "mcp_access")
                    .is_err()
            );
        }
        assert!(
            vault
                .write_note(Path::new("rw.md"), "no permission field")
                .is_err()
        );
        assert!(
            vault
                .set_frontmatter_field(Path::new("rw.md"), "mcp_access", serde_json::json!("deny"))
                .is_err()
        );
        vault.append_note(Path::new("rw.md"), "more").unwrap();
        assert!(
            vault
                .create_note(
                    Path::new("new.md"),
                    "---\nmcp_access: read-write\n---",
                    None
                )
                .is_err()
        );
        assert!(
            vault
                .create_note(Path::new(".obsidian-mcp/ignore"), "", None)
                .is_err()
        );
        assert!(
            vault
                .write_note(Path::new(".obsidian/daily-notes.json"), "{}")
                .is_err()
        );
    }

    #[tokio::test]
    async fn changes_on_disk_refresh_metadata_bm25_and_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("note.md"), "searchtoken #tag").unwrap();
        let vault = Vault::open(&tantivy_config(root)).await.unwrap();
        assert_eq!(vault.search_text("searchtoken", 100).unwrap().len(), 1);
        fs::write(
            root.join("note.md"),
            "---\nmcp_access: deny\n---\nsearchtoken #tag",
        )
        .unwrap();
        vault.refresh_after_sync().await.unwrap();
        assert!(vault.search_text("searchtoken", 100).unwrap().is_empty());
        assert!(vault.search_by_tag("tag").unwrap().is_empty());
        fs::write(root.join("note.md"), "searchtoken #tag").unwrap();
        vault.refresh_after_sync().await.unwrap();
        assert_eq!(vault.search_text("searchtoken", 100).unwrap().len(), 1);
        for content in [
            "```mcpignore\n[invalid\n```",
            "```mcpignore\n!allow\n```",
            "```mcpignore\nPrivate/",
            "Forgot the fence",
        ] {
            fs::write(root.join(POLICY), content).unwrap();
            assert!(vault.read_note(Path::new("note.md")).is_err());
            assert!(vault.refresh_after_sync().await.is_err());
        }
        fs::remove_file(root.join(POLICY)).unwrap();
        fs::write(root.join("note.md"), "---\nmcp_access: false\n---\nsecret").unwrap();
        assert!(vault.read_note(Path::new("note.md")).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_cannot_bypass_paths_or_modify_policy() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("Private")).unwrap();
        fs::write(root.join("Private/secret.md"), "secret").unwrap();
        policy(root, "Private/");
        symlink("Private/secret.md", root.join("alias.md")).unwrap();
        symlink("Private", root.join("alias-dir")).unwrap();
        symlink(POLICY, root.join("policy-alias.md")).unwrap();
        let vault = Vault::open(&tantivy_config(root)).await.unwrap();
        assert!(vault.read_note(Path::new("alias.md")).is_err());
        assert!(
            vault
                .create_note(Path::new("alias-dir/new.md"), "new", None)
                .is_err()
        );
        assert!(
            vault
                .write_note(Path::new("policy-alias.md"), "replace")
                .is_err()
        );
    }

    #[tokio::test]
    async fn periodic_creation_cannot_copy_a_restricted_template() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".obsidian")).unwrap();
        fs::write(
            root.join(".obsidian/daily-notes.json"),
            r#"{"template":"secret"}"#,
        )
        .unwrap();
        fs::write(root.join("secret.md"), "---\nmcp_access: deny\n---\nsecret").unwrap();
        let vault = Vault::open(&tantivy_config(root)).await.unwrap();
        assert!(
            vault
                .create_periodic_note(&crate::models::NotePeriod::Daily, None, None)
                .is_err()
        );
    }
}
