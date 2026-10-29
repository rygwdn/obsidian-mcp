//! Opt-in, serialized one-shot Obsidian Headless Sync around MCP operations.
//! No shell, login, credential management, continuous process, or lock-file removal.
use std::{
    fs::{File, OpenOptions},
    future::Future,
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Mutex,
    time::{Duration, Instant},
};

use rmcp::model::{CallToolResult, ContentBlock, ErrorData};
use schemars::JsonSchema;
use serde::Serialize;
use tokio::{io::AsyncReadExt, process::Command, sync::Mutex as AsyncMutex};

use crate::{
    error::{VaultError, VaultResult},
    vault::Vault,
};

#[derive(Clone)]
pub struct SyncConfig {
    pub executable: PathBuf,
    pub timeout: Duration,
    pub read_ttl: Duration,
}

impl SyncConfig {
    pub fn from_env() -> Result<Option<Self>, String> {
        let mode = match std::env::var("OBSIDIAN_SYNC_MODE") {
            Ok(mode) => mode,
            Err(std::env::VarError::NotPresent) => "disabled".into(),
            Err(_) => return Err("OBSIDIAN_SYNC_MODE is not valid Unicode".into()),
        };
        match mode.as_str() {
            "disabled" => return Ok(None),
            "managed" => {}
            _ => return Err("OBSIDIAN_SYNC_MODE must be disabled or managed".into()),
        }
        fn number(name: &str, default: u64, min: u64, max: u64) -> Result<u64, String> {
            match std::env::var(name) {
                Ok(raw) => raw
                    .parse::<u64>()
                    .ok()
                    .filter(|n| (min..=max).contains(n))
                    .ok_or_else(|| format!("{name} must be {min}–{max}")),
                Err(std::env::VarError::NotPresent) => Ok(default),
                Err(_) => Err(format!("{name} is not valid Unicode")),
            }
        }
        let executable = std::env::var_os("OBSIDIAN_HEADLESS_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| "ob".into());
        if executable.as_os_str().is_empty() {
            return Err("OBSIDIAN_HEADLESS_BIN must not be empty".into());
        }
        Ok(Some(Self {
            executable,
            timeout: Duration::from_secs(number("OBSIDIAN_SYNC_TIMEOUT_SECS", 90, 1, 3600)?),
            read_ttl: Duration::from_millis(number("OBSIDIAN_SYNC_READ_TTL_MS", 0, 0, 60000)?),
        }))
    }
}

#[derive(Default)]
struct Progress {
    running: bool,
    last_success: Option<String>,
    successful_at: Option<Instant>,
    last_error: Option<String>,
    last_duration_ms: Option<u64>,
}

#[derive(Serialize, JsonSchema)]
pub struct SyncStatus {
    pub enabled: bool,
    pub mode: String,
    pub running: bool,
    pub pending_upload: bool,
    pub last_success: Option<String>,
    pub last_error: Option<String>,
    pub last_duration_ms: Option<u64>,
    pub read_ttl_ms: u64,
}

impl SyncStatus {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            mode: "disabled".into(),
            running: false,
            pending_upload: false,
            last_success: None,
            last_error: None,
            last_duration_ms: None,
            read_ttl_ms: 0,
        }
    }
}

pub struct SyncCoordinator {
    config: SyncConfig,
    root: PathBuf,
    pending: PathBuf,
    lock_path: PathBuf,
    gate: AsyncMutex<()>,
    progress: Mutex<Progress>,
}

struct ProcessLock(File);
impl Drop for ProcessLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

struct Attempt<'a>(&'a SyncCoordinator);
impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        let mut progress = self.0.progress.lock().unwrap_or_else(|e| e.into_inner());
        if progress.running {
            progress.running = false;
            progress.successful_at = None;
            progress.last_error = Some(
                "Sync was cancelled; freshness is unknown. Retry sync before proceeding.".into(),
            );
        }
    }
}

fn failure(message: impl Into<String>) -> VaultError {
    VaultError::Other(message.into())
}

impl SyncCoordinator {
    pub fn new(config: SyncConfig, root: PathBuf, data: &Path, home: &Path) -> Self {
        Self {
            config,
            root,
            pending: data.join("headless-sync.pending"),
            lock_path: home.join("headless-coordinator.lock"),
            gate: AsyncMutex::new(()),
            progress: Mutex::new(Progress::default()),
        }
    }

    pub fn status(&self) -> SyncStatus {
        let p = self.progress.lock().unwrap_or_else(|e| e.into_inner());
        SyncStatus {
            enabled: true,
            mode: "managed".into(),
            running: p.running,
            pending_upload: self.pending.exists(),
            last_success: p.last_success.clone(),
            last_error: p.last_error.clone(),
            last_duration_ms: p.last_duration_ms,
            read_ttl_ms: self.config.read_ttl.as_millis() as u64,
        }
    }

    async fn process_lock(&self) -> VaultResult<ProcessLock> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.lock_path)?;
        let deadline = Instant::now() + self.config.timeout;
        loop {
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(ProcessLock(file)),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(25)).await
                }
                Err(_) => {
                    return Err(failure(
                        "Could not acquire the managed-sync operation lock; another service may be busy.",
                    ));
                }
            }
        }
    }

    async fn command(&self, subcommand: &str, json: bool) -> VaultResult<Vec<u8>> {
        let mut command = Command::new(&self.config.executable);
        command
            .arg(subcommand)
            .arg("--path")
            .arg(&self.root)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if json {
            command.arg("--json").stdout(Stdio::piped());
        } else {
            command.stdout(Stdio::null());
        }
        let mut child = command.spawn().map_err(|_| failure("Could not start Obsidian Headless. Check OBSIDIAN_HEADLESS_BIN and the installed Node runtime."))?;
        let result = tokio::time::timeout(self.config.timeout, async {
            let mut bytes = Vec::new();
            if let Some(stdout) = child.stdout.take() {
                stdout.take(65537).read_to_end(&mut bytes).await.map_err(|_| failure("Could not read Headless configuration status."))?;
                if bytes.len() > 65536 { return Err(failure("Headless configuration response exceeded the size limit.")); }
            }
            let status = child.wait().await.map_err(|_| failure("Could not wait for Obsidian Headless."))?;
            if !status.success() {
                return Err(failure(format!("Obsidian Headless {subcommand} failed (exit {:?}). Check CLI login/setup and ensure no continuous sync owns the vault. CLI output is suppressed to protect credentials and note paths.", status.code())));
            }
            Ok(bytes)
        }).await;
        match result {
            Ok(Ok(bytes)) => Ok(bytes),
            other => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                match other {
                    Ok(Err(error)) => Err(error),
                    _ => Err(failure(
                        "Obsidian Headless timed out. The child was stopped; no lock files were removed.",
                    )),
                }
            }
        }
    }

    fn mark_pending(&self) -> VaultResult<()> {
        let parent = self
            .pending
            .parent()
            .ok_or_else(|| failure("Invalid sync state directory"))?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(b"A local mutation may need uploading. Do not replay it blindly.\n")?;
        file.as_file().sync_all()?;
        file.persist(&self.pending).map_err(|e| e.error)?;
        self.progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .successful_at = None;
        Ok(())
    }

    async fn pass(&self, vault: &Vault, allow_cached: bool) -> VaultResult<()> {
        {
            let mut p = self.progress.lock().unwrap_or_else(|e| e.into_inner());
            if allow_cached
                && !self.pending.exists()
                && p.successful_at
                    .is_some_and(|at| at.elapsed() < self.config.read_ttl)
            {
                return Ok(());
            }
            p.running = true;
            p.successful_at = None;
        }
        let _attempt = Attempt(self);
        let started = Instant::now();
        let result = async {
            let status = self.command("sync-status", true).await?;
            let status: serde_json::Value = serde_json::from_slice(&status).map_err(|_| failure("Invalid Headless sync-status JSON."))?;
            if status.get("syncMode").and_then(|v| v.as_str()) != Some("bidirectional") {
                return Err(failure("Managed sync requires bidirectional mode. Pull-only cannot upload writes, and mirror-remote may discard local edits."));
            }
            self.command("sync", false).await?;
            vault.refresh_after_sync().await?;
            match std::fs::remove_file(&self.pending) {
                Ok(()) => {}, Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}, Err(e) => return Err(e.into()),
            }
            Ok(())
        }.await;
        let mut p = self.progress.lock().unwrap_or_else(|e| e.into_inner());
        p.running = false;
        p.last_duration_ms = Some(started.elapsed().as_millis() as u64);
        match &result {
            Ok(()) => {
                p.last_success = Some(chrono::Utc::now().to_rfc3339());
                p.successful_at = Some(Instant::now());
                p.last_error = None;
            }
            Err(error) => {
                p.successful_at = None;
                p.last_error = Some(error.to_string());
            }
        }
        result
    }

    pub async fn sync_now(&self, vault: &Vault) -> VaultResult<SyncStatus> {
        let _gate = self.gate.lock().await;
        let _process = self.process_lock().await?;
        self.pass(vault, false).await?;
        Ok(self.status())
    }

    pub async fn execute<F>(
        &self,
        vault: &Vault,
        mutation: bool,
        operation: F,
    ) -> Result<CallToolResult, ErrorData>
    where
        F: Future<Output = Result<CallToolResult, ErrorData>>,
    {
        let _gate = self.gate.lock().await;
        let _process = self.process_lock().await.map_err(ErrorData::from)?;
        self.pass(vault, !mutation).await.map_err(|error| {
            let mut error = ErrorData::from(error);
            error.data =
                Some(serde_json::json!({"kind":"pre_sync_failed", "operation_started":false}));
            error
        })?;
        if !mutation {
            return operation.await;
        }
        // Persist before the local mutation so cancellation/restarts cannot lose the pending signal.
        self.mark_pending().map_err(ErrorData::from)?;
        let result = operation.await;
        let synced = self.pass(vault, false).await;
        match (result, synced) {
            (Ok(mut result), Err(_)) => {
                // A tool error could cause an agent to repeat an already-applied append.
                result.content.push(ContentBlock::text("WARNING: The local write succeeded, but the follow-up sync failed. Upload is pending. Do NOT repeat the write. Use sync_status/sync_now to retry synchronization only."));
                Ok(result)
            }
            (Err(mut error), Err(_)) => {
                error.data = Some(
                    serde_json::json!({"kind":"operation_and_post_sync_failed", "mutation_may_have_applied":true, "sync_pending":true, "retry_operation":false}),
                );
                Err(error)
            }
            (result, Ok(())) => result,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::test_helpers::{create_test_vault, tantivy_config};
    use std::{
        os::unix::fs::PermissionsExt,
        sync::atomic::{AtomicBool, Ordering},
    };

    async fn fixture(
        script: &str,
        ttl: u64,
        timeout_ms: u64,
    ) -> (tempfile::TempDir, Vault, SyncCoordinator) {
        let dir = tempfile::tempdir().unwrap();
        create_test_vault(dir.path());
        let executable = dir.path().join("fake-ob");
        std::fs::write(&executable, format!("#!/bin/sh\nset -eu\n{script}\n")).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let vault = Vault::open(&tantivy_config(dir.path())).await.unwrap();
        let sync = SyncCoordinator::new(
            SyncConfig {
                executable,
                timeout: Duration::from_millis(timeout_ms),
                read_ttl: Duration::from_millis(ttl),
            },
            vault.root().into(),
            vault.mcp_data(),
            vault.mcp_home(),
        );
        (dir, vault, sync)
    }

    const BASIC: &str = r#"
[ "$2" = '--path' ]
[ "$3" = "$PWD" ]
if [ "$1" = sync-status ]; then
  [ "$4" = '--json' ]
  printf '{"syncMode":"bidirectional"}'
  exit
fi
[ "$1" = sync ]
[ "$#" = 3 ]
n=0; [ ! -f count ] || n=$(cat count)
n=$((n+1)); printf '%s' "$n" > count
if [ -f fail ]; then echo 'SECRET sentinel' >&2; exit 7; fi
if [ -f download ]; then cp download remote.md; fi
"#;

    #[tokio::test]
    async fn download_refreshes_metadata_and_read_ttl_never_skips_writes() {
        let (dir, vault, sync) = fixture(BASIC, 60000, 2000).await;
        std::fs::write(
            dir.path().join("download"),
            "# Downloaded\n#fresh [[target]]",
        )
        .unwrap();
        sync.execute(&vault, false, async {
            assert!(
                vault
                    .read_note(Path::new("remote.md"))
                    .unwrap()
                    .contains("Downloaded")
            );
            assert_eq!(vault.search_by_tag("fresh").unwrap().len(), 1);
            assert_eq!(vault.search_text("Downloaded", 100).unwrap().len(), 1);
            Ok(CallToolResult::success(vec![]))
        })
        .await
        .unwrap();
        sync.execute(&vault, false, async { Ok(CallToolResult::success(vec![])) })
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("count")).unwrap(),
            "1"
        );
        sync.execute(&vault, true, async { Ok(CallToolResult::success(vec![])) })
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("count")).unwrap(),
            "3"
        );
        assert!(!sync.status().pending_upload);
    }

    #[tokio::test]
    async fn post_write_failure_survives_restart_and_retries_only_sync() {
        let (dir, vault, sync) = fixture(BASIC, 0, 2000).await;
        let result = sync
            .execute(&vault, true, async {
                std::fs::write(dir.path().join("saved.md"), "one append").unwrap();
                std::fs::write(dir.path().join("fail"), "").unwrap();
                Ok(CallToolResult::success(vec![]))
            })
            .await
            .unwrap();
        let text = &result.content[0].as_text().unwrap().text;
        assert!(text.contains("Do NOT repeat"));
        assert!(sync.status().pending_upload);
        assert!(!sync.status().last_error.unwrap().contains("SECRET"));
        let restarted = SyncCoordinator::new(
            sync.config.clone(),
            vault.root().into(),
            vault.mcp_data(),
            vault.mcp_home(),
        );
        assert!(restarted.status().pending_upload);
        std::fs::remove_file(dir.path().join("fail")).unwrap();
        restarted.sync_now(&vault).await.unwrap();
        assert!(!restarted.status().pending_upload);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("saved.md")).unwrap(),
            "one append"
        );
    }

    #[tokio::test]
    async fn pre_sync_failure_and_unsafe_modes_block_operations() {
        for script in [
            BASIC.to_string(),
            "printf '{\"syncMode\":\"mirror-remote\"}'".into(),
            "printf '{\"syncMode\":\"pull-only\"}'".into(),
            "printf 'invalid json'".into(),
        ] {
            let (dir, vault, sync) = fixture(&script, 0, 2000).await;
            std::fs::write(dir.path().join("fail"), "").unwrap();
            let started = AtomicBool::new(false);
            let error = sync
                .execute(&vault, true, async {
                    started.store(true, Ordering::SeqCst);
                    Ok(CallToolResult::success(vec![]))
                })
                .await
                .unwrap_err();
            assert!(!started.load(Ordering::SeqCst));
            assert_eq!(error.data.unwrap()["operation_started"], false);
            assert!(!sync.status().pending_upload);
        }
    }

    #[tokio::test]
    async fn timeout_blocks_reads() {
        let (_dir, vault, sync) = fixture("exec sleep 5", 0, 30).await;
        let started = AtomicBool::new(false);
        let before = Instant::now();
        assert!(
            sync.execute(&vault, false, async {
                started.store(true, Ordering::SeqCst);
                Ok(CallToolResult::success(vec![]))
            })
            .await
            .is_err()
        );
        assert!(!started.load(Ordering::SeqCst));
        assert!(before.elapsed() < Duration::from_secs(2));
        assert!(!sync.status().running);
    }

    #[tokio::test]
    async fn independent_coordinators_serialize_entire_write_transactions() {
        let (dir, vault, sync) = fixture(BASIC, 0, 2000).await;
        let second = SyncCoordinator::new(
            sync.config.clone(),
            vault.root().into(),
            vault.mcp_data(),
            vault.mcp_home(),
        );
        let first = sync.execute(&vault, true, async {
            tokio::time::sleep(Duration::from_millis(80)).await;
            assert_eq!(
                std::fs::read_to_string(dir.path().join("count")).unwrap(),
                "1"
            );
            Ok(CallToolResult::success(vec![]))
        });
        let next = second.execute(&vault, true, async {
            assert_eq!(
                std::fs::read_to_string(dir.path().join("count")).unwrap(),
                "3"
            );
            Ok(CallToolResult::success(vec![]))
        });
        let (a, b) = tokio::join!(first, next);
        a.unwrap();
        b.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("count")).unwrap(),
            "4"
        );
    }
    #[tokio::test]
    async fn downloaded_edit_invalidates_revision_before_write() {
        let (dir, vault, sync) = fixture(BASIC, 0, 2000).await;
        std::fs::write(dir.path().join("remote.md"), "old").unwrap();
        let revision = crate::vault::fs::revision("old");
        std::fs::write(dir.path().join("download"), "external edit").unwrap();
        let result = sync
            .execute(&vault, true, async {
                vault.write_note_checked(Path::new("remote.md"), "overwrite", Some(&revision))?;
                Ok(CallToolResult::success(vec![]))
            })
            .await;
        assert!(result.is_err());
        assert_eq!(
            vault.read_note(Path::new("remote.md")).unwrap(),
            "external edit"
        );
    }

    #[tokio::test]
    async fn cancellation_during_mutation_keeps_pending_marker_and_unlocks() {
        let (_dir, vault, sync) = fixture(BASIC, 0, 2000).await;
        let result = tokio::time::timeout(
            Duration::from_millis(150),
            sync.execute(&vault, true, async {
                std::future::pending::<()>().await;
                Ok(CallToolResult::success(vec![]))
            }),
        )
        .await;
        assert!(result.is_err());
        assert!(sync.status().pending_upload);
        sync.sync_now(&vault).await.unwrap();
        assert!(!sync.status().pending_upload);
    }
}
