use super::*;
use std::sync::Mutex;
use std::time::{Duration, Instant};

static ENV_LOCK: Mutex<()> = Mutex::new(());

const LOW: &str = "abliterated-qwen-latest-27b-low";
const NONE: &str = "abliterated-qwen-latest-27b-none";
const LOCAL: &str = "http://localhost:18317/v1";
const GB10: &str = "http://gb10:18009/v1";

struct EnvGuard {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl EnvGuard {
    fn set(pairs: &[(&'static str, Option<&str>)]) -> Self {
        let lock = ENV_LOCK.lock().expect("env lock");
        let saved = pairs
            .iter()
            .map(|(name, _)| (*name, env::var_os(name)))
            .collect();
        for (name, value) in pairs {
            match value {
                Some(value) => env::set_var(name, value),
                None => env::remove_var(name),
            }
        }
        Self { saved, _lock: lock }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            match value {
                Some(value) => env::set_var(name, value),
                None => env::remove_var(name),
            }
        }
    }
}

fn enabled(extra: &[(&'static str, Option<&str>)]) -> EnvGuard {
    let mut pairs = vec![
        ("PBI_RS_ADK_ENABLE", Some("1")),
        ("PBI_RS_CREDENTIAL_HANDLE", Some("CLIPROXY_API_KEY")),
        ("CLIPROXY_BASE_URL", None),
        ("LOCAL_ROUTER_BASEURL", None),
        ("LOCAL_MODEL", None),
        ("LLM_MODEL", None),
        ("PBI_CONFIG_FILE", None),
    ];
    pairs.extend(extra.iter().copied());
    EnvGuard::set(&pairs)
}

fn route() -> (String, String) {
    let routes = explicit_admitted_routes_from_environment()
        .expect("route")
        .expect("admitted");
    let selected = routes.first().expect("one route");
    (selected.base_url().to_owned(), selected.model().to_owned())
}

fn matching_config(dir: &Path, primary: &str, second_base: &str) -> std::path::PathBuf {
    let config = dir.join("config.toml");
    fs::write(
        &config,
        format!(
            "primary_model = \"{primary}\"\nmodel = \"{NONE}\"\n\n\
             [[endpoints]]\nmodel = \"{NONE}\"\nbase_url = \"{LOCAL}\"\n\n\
             [[endpoints]]\nmodel = \"{primary}\"\nbase_url = \"{second_base}\"\n"
        ),
    )
    .expect("config");
    config
}

#[test]
fn fifo_without_writer_is_bounded_invalid_config() {
    let dir = env::temp_dir().join(format!("pbi-rs-fifo-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).expect("dir");
    let fifo = dir.join("pipe.toml");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo");
    assert!(status.success());
    let _env = enabled(&[("PBI_CONFIG_FILE", Some(fifo.to_str().expect("utf8")))]);
    let started = Instant::now();
    let error = explicit_admitted_routes_from_environment().expect_err("fifo");
    assert_eq!(error, SemanticRouteError::InvalidConfig);
    assert!(started.elapsed() < Duration::from_secs(2));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn oversized_file_is_rejected_before_parse() {
    let dir = env::temp_dir().join(format!("pbi-rs-grow-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).expect("dir");
    let path = dir.join("config.toml");
    let mut bytes = format!(
        "primary_model = \"{LOW}\"\n[[endpoints]]\nmodel = \"{LOW}\"\nbase_url = \"{GB10}\"\n# "
    )
    .into_bytes();
    bytes.resize((MAX_CONFIG_BYTES as usize) + 1, b' ');
    fs::write(&path, &bytes).expect("limit");
    let _env = enabled(&[("PBI_CONFIG_FILE", Some(path.to_str().expect("utf8")))]);
    let error = explicit_admitted_routes_from_environment().expect_err("oversized");
    assert_eq!(error, SemanticRouteError::InvalidConfig);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn partial_env_keeps_matching_toml_field() {
    let dir = env::temp_dir().join(format!("pbi-rs-partial-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).expect("dir");
    let config = matching_config(&dir, LOW, GB10);
    let path = config.to_str().expect("utf8").to_owned();
    {
        let _env = enabled(&[("PBI_CONFIG_FILE", Some(&path)), ("LOCAL_MODEL", Some(LOW))]);
        assert_eq!(route(), (GB10.to_owned(), LOW.to_owned()));
    }
    {
        let _env = enabled(&[
            ("PBI_CONFIG_FILE", Some(&path)),
            ("CLIPROXY_BASE_URL", Some(GB10)),
        ]);
        assert_eq!(route(), (GB10.to_owned(), LOW.to_owned()));
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn explicit_invalid_config_does_not_fall_back_to_builtin() {
    let dir = env::temp_dir().join(format!("pbi-rs-invalid-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).expect("dir");
    let config = dir.join("config.toml");
    fs::write(&config, "primary_model = [").expect("config");
    let _env = enabled(&[("PBI_CONFIG_FILE", Some(config.to_str().expect("utf8")))]);
    assert_eq!(
        explicit_admitted_routes_from_environment().expect_err("invalid config"),
        SemanticRouteError::InvalidConfig
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn symlink_to_regular_config_is_admitted() {
    let dir = env::temp_dir().join(format!("pbi-rs-link-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).expect("dir");
    let config = matching_config(&dir, NONE, LOCAL);
    let link = dir.join("link.toml");
    std::os::unix::fs::symlink(&config, &link).expect("symlink");
    let _env = enabled(&[("PBI_CONFIG_FILE", Some(link.to_str().expect("utf8")))]);
    assert_eq!(route(), (LOCAL.to_owned(), NONE.to_owned()));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn exactly_limit_bytes_still_parse() {
    let dir = env::temp_dir().join(format!("pbi-rs-limit-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).expect("dir");
    let path = dir.join("config.toml");
    let body = format!(
        "primary_model = \"{LOW}\"\n[[endpoints]]\nmodel = \"{LOW}\"\nbase_url = \"{GB10}\"\n"
    );
    let mut bytes = body.into_bytes();
    bytes.resize(MAX_CONFIG_BYTES as usize, b' ');
    fs::write(&path, bytes).expect("limit");
    let _env = enabled(&[("PBI_CONFIG_FILE", Some(path.to_str().expect("utf8")))]);
    assert_eq!(route(), (GB10.to_owned(), LOW.to_owned()));
    let _ = fs::remove_dir_all(&dir);
}
