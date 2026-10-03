//! Private, bounded pagination state for explicitly requested raw searches.
//!
//! Each call recomputes results. The durable file contains only a source
//! fingerprint and cursor; no query, source text, snippet, or credential.

use serde_json::json;
use std::env;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_STATE_BYTES: u64 = 512;
const MAX_CURSOR: usize = 4096;
const MAX_STATE_FILES: usize = 512;

pub struct RawSession {
    path: PathBuf,
    _lock: File,
}

impl RawSession {
    /// Open one scope's state with exclusive ownership until this value drops.
    pub fn open(id: &str, scope: u64, deadline: Instant) -> Result<Self, String> {
        let base = if let Some(path) = env::var_os("XDG_STATE_HOME") {
            PathBuf::from(path)
        } else {
            PathBuf::from(env::var_os("HOME").ok_or("HOME is required for --session")?)
                .join(".local/state")
        };
        if !base.is_absolute() || base.components().any(|part| part == Component::ParentDir) {
            return Err("session state root must be an absolute normalized path".into());
        }
        let dir = base.join("pbi-rs/search-sessions");
        ensure_state_dir(&dir)?;
        let name = format!("{id}-{scope:016x}");
        let lock_path = dir.join(format!("{name}.lock"));
        let state_path = dir.join(format!("{name}.json"));
        let mut count = 0usize;
        for entry in fs::read_dir(&dir).map_err(|e| e.to_string())? {
            entry.map_err(|e| e.to_string())?;
            count += 1;
            if count > MAX_STATE_FILES {
                return Err("session state file count exceeds its cap".into());
            }
        }
        let additional = usize::from(fs::symlink_metadata(&lock_path).is_err())
            + usize::from(fs::symlink_metadata(&state_path).is_err());
        if count + additional > MAX_STATE_FILES {
            return Err("session state file count exceeds its cap".into());
        }
        let lock = private_file(&lock_path, true)?;
        loop {
            // The lock covers search, rendering, and cursor publication.
            let result = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(format!("session lock failed: {error}"));
            }
            if Instant::now() >= deadline {
                return Err("session lock deadline exceeded".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(Self {
            path: state_path,
            _lock: lock,
        })
    }

    /// Return the next offset, restarting if any admitted source changed.
    pub fn cursor(&self, freshness: u64) -> Result<usize, String> {
        let file = match private_file(&self.path, false) {
            Ok(file) => file,
            Err(error) if error == "session state is absent" => return Ok(0),
            Err(error) => return Err(error),
        };
        if file.metadata().map_err(|e| e.to_string())?.len() > MAX_STATE_BYTES {
            return Err("session state exceeds its byte cap".into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_STATE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err("session state exceeds its byte cap".into());
        }
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| "session state is invalid".to_owned())?;
        if value["version"].as_u64() != Some(1) {
            return Err("session state version is unsupported".into());
        }
        if value["freshness"].as_u64() != Some(freshness) {
            return Ok(0);
        }
        let cursor = value["cursor"]
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value <= MAX_CURSOR)
            .ok_or("session cursor is invalid")?;
        Ok(cursor)
    }

    /// Atomically publish the next offset while the exclusive lock is held.
    pub fn advance(&self, freshness: u64, cursor: usize) -> Result<(), String> {
        if cursor > MAX_CURSOR {
            return Err("session cursor exceeds its result cap".into());
        }
        // Refuse a hostile existing target before replacing it. Rename itself
        // replaces a symlink rather than following it, but the state is private.
        match fs::symlink_metadata(&self.path) {
            Ok(_) => {
                private_file(&self.path, false)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos();
        let temporary = self
            .path
            .with_extension(format!("{}.{}.tmp", std::process::id(), nanos));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&temporary)
                .map_err(|e| e.to_string())?;
            let bytes = json!({"version": 1, "freshness": freshness, "cursor": cursor}).to_string();
            file.write_all(bytes.as_bytes())
                .map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
            fs::rename(&temporary, &self.path).map_err(|e| e.to_string())?;
            Ok::<(), String>(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

use std::os::fd::AsRawFd;

fn private_file(path: &Path, create: bool) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(create)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    if create {
        options.create(true).mode(0o600);
    }
    let file = options.open(path).map_err(|error| {
        if !create && error.kind() == std::io::ErrorKind::NotFound {
            "session state is absent".to_owned()
        } else {
            format!("session file cannot be opened safely: {error}")
        }
    })?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o777 != 0o600
    {
        return Err("session file is not private and regular".into());
    }
    Ok(file)
}

fn ensure_state_dir(dir: &Path) -> Result<(), String> {
    let mut current = PathBuf::new();
    for component in dir.components() {
        current.push(component.as_os_str());
        if current == Path::new("/") {
            continue;
        }
        let private = current == dir || current == dir.parent().ok_or("invalid state path")?;
        match fs::symlink_metadata(&current) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match DirBuilder::new().mode(0o700).create(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.to_string()),
                }
            }
            Err(error) => return Err(error.to_string()),
        }
        let metadata = fs::symlink_metadata(&current).map_err(|e| e.to_string())?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err("session state path contains a symlink or non-directory".into());
        }
        if private
            && (metadata.uid() != unsafe { libc::geteuid() }
                || metadata.permissions().mode() & 0o077 != 0)
        {
            return Err("session state directory is not private".into());
        }
    }
    Ok(())
}
