use super::*;

const NONE: &str = "abliterated-qwen-latest-27b-none";
const LOW: &str = "abliterated-qwen-latest-27b-low";
const LOCAL: &str = "http://localhost:18317/v1";
const GB10: &str = "http://gb10:18009/v1";

fn fixture(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pbi-rs-debug-{label}-{}",
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).expect("fixture");
    let probe = dir.join("probe");
    fs::write(
        &probe,
        "#!/bin/sh\nprintf invoked > \"$PWD/probe.called\"\nexit 97\n",
    )
    .expect("forbidden probe fixture");
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(probe, fs::Permissions::from_mode(0o700)).expect("probe mode");
    dir
}

fn route_env(
    root: &std::path::Path,
    extra: &[(&'static str, Option<String>)],
) -> RouteConfigEnvGuard {
    let mut entries = vec![
        (
            "PBI_RS_PROBE",
            Some(root.join("probe").to_string_lossy().into_owned()),
        ),
        ("PBI_RS_ADK_ENABLE", None),
        ("PBI_RS_CREDENTIAL_HANDLE", None),
        ("CLIPROXY_API_KEY", None),
        ("OPENAI_API_KEY", None),
        ("LOCAL_ROUTER_API_KEY", None),
        ("CLIPROXY_BASE_URL", None),
        ("LOCAL_ROUTER_BASEURL", None),
        ("LOCAL_MODEL", None),
        ("LLM_MODEL", None),
        ("PBI_CONFIG_FILE", None),
    ];
    for (key, value) in extra {
        entries.retain(|(existing, _)| existing != key);
        entries.push((*key, value.clone()));
    }
    RouteConfigEnvGuard::new(root, &entries)
}

fn config(dir: &std::path::Path, body: &str) -> String {
    let path = dir.join("config.toml");
    fs::write(&path, body).expect("config");
    path.to_string_lossy().into_owned()
}

fn debug(arguments: &[&str]) -> Result<String, CliError> {
    let mut output = Vec::new();
    let code = run(
        arguments.iter().copied().map(str::to_owned).collect(),
        Some(TestRouteInjection::Factory {
            build: &|_| panic!("debug config must not build a publisher or model"),
            deadline: Duration::from_secs(1),
        }),
        &mut output,
    )?;
    assert_eq!(code, 0);
    assert!(
        !Path::new("probe.called").exists(),
        "debug must not invoke Probe"
    );
    Ok(String::from_utf8(output).expect("debug stdout"))
}

fn assert_selected(stdout: &str, base: &str, model: &str) {
    assert!(
        stdout.contains(&format!("primary_model={model}\n")),
        "{stdout}"
    );
    assert!(stdout.contains(&format!("base_url={base}\n")), "{stdout}");
    assert!(stdout.contains("api_key=[REDACTED]\n"), "{stdout}");
    assert!(!stdout.contains("secret-value"), "{stdout}");
    assert!(!stdout.contains("fixture-secret"), "{stdout}");
}

#[test]
fn debug_selects_second_gb10_none_without_key_or_adk() {
    let dir = fixture("gb10");
    let path = config(
        &dir,
        &format!(
            "primary_model = \"{NONE}\"\napi_key = \"fixture-secret\"\n\
             [[endpoints]]\nmodel = \"{LOW}\"\nbase_url = \"{LOCAL}\"\n\
             api_key = \"decoy-secret\"\n\
             [[endpoints]]\nmodel = \"{NONE}\"\nbase_url = \"{GB10}\"\n\
             api_key = \"selected-secret\"\n"
        ),
    );
    let _env = route_env(&dir, &[("PBI_CONFIG_FILE", Some(path))]);
    let stdout = debug(&["--debug-config"]).unwrap_or_else(|error| panic!("{}", error.message));
    assert_selected(&stdout, GB10, NONE);
    assert!(
        !stdout.contains(&format!("base_url={LOCAL}\n")),
        "decoy localhost must not be the selected base: {stdout}"
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn debug_cloud_host_is_rc78_with_empty_stdout() {
    let dir = fixture("cloud");
    let path = config(
        &dir,
        &format!(
            "primary_model = \"{NONE}\"\n[[endpoints]]\nmodel = \"{NONE}\"\n\
             base_url = \"https://api.openai.com/v1\"\n"
        ),
    );
    let _env = route_env(&dir, &[("PBI_CONFIG_FILE", Some(path))]);
    let mut output = Vec::new();
    let error = run(
        vec!["--debug-config".to_owned()],
        Some(TestRouteInjection::Factory {
            build: &|_| panic!("rejected route must not build a publisher"),
            deadline: Duration::from_secs(1),
        }),
        &mut output,
    )
    .expect_err("cloud host");
    assert_eq!(error.code, 78);
    assert!(output.is_empty(), "{output:?}");
    assert!(!error.message.contains("api.openai.com"));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn cli_localhost_low_overrides_invalid_config() {
    let dir = fixture("cli");
    let path = config(&dir, "primary_model = [");
    let _env = route_env(&dir, &[("PBI_CONFIG_FILE", Some(path))]);
    let stdout = debug(&[
        "--model-route",
        LOCAL,
        LOW,
        "CLIPROXY_API_KEY",
        "--debug-config",
    ])
    .unwrap_or_else(|error| panic!("{}", error.message));
    assert_selected(&stdout, LOCAL, LOW);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn complete_env_bypasses_malformed_config() {
    let dir = fixture("env");
    let path = config(&dir, "primary_model = [");
    let _env = route_env(
        &dir,
        &[
            ("PBI_CONFIG_FILE", Some(path)),
            ("CLIPROXY_BASE_URL", Some(GB10.to_owned())),
            ("LOCAL_MODEL", Some(NONE.to_owned())),
        ],
    );
    let stdout = debug(&["--debug-config"]).unwrap_or_else(|error| panic!("{}", error.message));
    assert_selected(&stdout, GB10, NONE);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn partial_env_uses_matching_toml_endpoint() {
    let dir = fixture("partial");
    let path = config(
        &dir,
        &format!(
            "primary_model = \"{LOW}\"\n[[endpoints]]\nmodel = \"{LOW}\"\n\
             base_url = \"{LOCAL}\"\n[[endpoints]]\nmodel = \"{NONE}\"\n\
             base_url = \"{GB10}\"\n"
        ),
    );
    let _env = route_env(
        &dir,
        &[
            ("PBI_CONFIG_FILE", Some(path)),
            ("LLM_MODEL", Some(NONE.to_owned())),
        ],
    );
    let stdout = debug(&["--debug-config"]).unwrap_or_else(|error| panic!("{}", error.message));
    assert_selected(&stdout, GB10, NONE);
    env::remove_var("LLM_MODEL");
    env::set_var("LOCAL_ROUTER_BASEURL", GB10);
    let stdout = debug(&["--debug-config"]).unwrap_or_else(|error| panic!("{}", error.message));
    assert_selected(&stdout, GB10, LOW);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn inherited_controls_and_default_path_stay_unchanged() {
    let dir = fixture("default");
    let _env = route_env(&dir, &[]);
    let stdout = debug(&["--debug-config"]).unwrap_or_else(|error| panic!("{}", error.message));
    assert_selected(&stdout, LOCAL, NONE);
    assert!(stdout.contains("search_default=compact_verified_bm25_no_chat\n"));
    assert!(stdout.contains("search_bm25_opt_in=--bm25_raw_no_llm_probe\n"));
    assert!(stdout.contains(&format!(
        "search_outer_deadline_seconds={PROBE_OUTER_DEADLINE_SECONDS}\n"
    )));
    assert!(stdout.contains(&format!(
        "search_scoped_target_limit={MAX_SCOPED_PROBE_TARGETS}\n"
    )));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn debug_rejects_secret_bearing_urls_before_echo() {
    let dir = fixture("secret-url");
    let _env = route_env(&dir, &[]);
    for base in [
        "http://user:fixture-secret@localhost:18317/v1",
        "http://localhost:18317/v1?api_key=fixture-secret",
        "http://localhost:18317/v1#fixture-secret",
        "not-a-url-fixture-secret",
    ] {
        let mut output = Vec::new();
        let error = run(
            [
                "--model-route",
                base,
                NONE,
                "CLIPROXY_API_KEY",
                "--debug-config",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            None,
            &mut output,
        )
        .expect_err(base);
        assert_eq!(error.code, 78, "{base}");
        assert!(output.is_empty(), "{base}: {output:?}");
        assert!(!error.message.contains("fixture-secret"), "{base}");
    }
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn search_raw_and_off_do_not_read_debug_config() {
    let dir = fixture("off");
    let probe = dir.join("probe");
    fs::write(
        &probe,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$PWD/probe.args\"\nprintf 'File: %s/item.rs, Lines: 1-1\\n' \"$PWD\"\n",
    )
    .expect("probe");
    fs::write(dir.join("item.rs"), "fn debug_config_marker() {}\n").expect("source");
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o700)).expect("mode");
    let pipe = dir.join("pipe.toml");
    assert!(std::process::Command::new("mkfifo")
        .arg(&pipe)
        .status()
        .expect("mkfifo")
        .success());
    let _env = route_env(
        &dir,
        &[
            ("PBI_RS_PROBE", Some(probe.to_string_lossy().into_owned())),
            ("PBI_CONFIG_FILE", Some(pipe.to_string_lossy().into_owned())),
        ],
    );
    for args in [
        vec!["search", "--bm25", "debug_config_marker"],
        vec!["search", "debug_config_marker"],
        vec!["debug_config_marker"],
    ] {
        let started = Instant::now();
        let mut output = Vec::new();
        let code = run(
            args.into_iter().map(str::to_owned).collect(),
            None,
            &mut output,
        )
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(code, 0);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!String::from_utf8_lossy(&output).contains("primary_model="));
    }
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn invalid_fifo_and_oversize_keep_static_rc78() {
    let dir = fixture("bad");
    let fifo = dir.join("pipe.toml");
    assert!(std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo")
        .success());
    let oversize = dir.join("oversize.toml");
    let mut bytes = format!(
        "primary_model = \"{NONE}\"\n[[endpoints]]\nmodel = \"{NONE}\"\nbase_url = \"{GB10}\"\n# "
    )
    .into_bytes();
    bytes.resize((64 * 1024) + 1, b' ');
    fs::write(&oversize, bytes).expect("oversize");
    let bad = config(&dir, "primary_model = [");
    for path in [
        fifo.to_string_lossy().into_owned(),
        oversize.to_string_lossy().into_owned(),
        bad,
    ] {
        let _env = route_env(&dir, &[("PBI_CONFIG_FILE", Some(path.clone()))]);
        let mut output = Vec::new();
        let started = Instant::now();
        let error = run(vec!["--debug-config".to_owned()], None, &mut output).expect_err("bad");
        assert!(started.elapsed() < Duration::from_secs(2), "{path}");
        assert_eq!(error.code, 78, "{path}");
        assert!(output.is_empty(), "{path}");
        assert_eq!(
            error.message,
            "semantic route denied: semantic route configuration file is invalid"
        );
    }
    let _ = fs::remove_dir_all(dir);
}
