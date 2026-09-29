use pbi_rs::{verify_probe_locations, EvidenceError};
use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Output};

const VERSION: &str = "0.1.0";
const DEFAULT_TIMEOUT: &str = "540";
const DEFAULT_MAX_RESULTS: usize = 8;

fn usage() {
    println!(
        "pbi-rs {VERSION} — Probe-backed source evidence\n\
         Usage: pbi-rs <question...>\n\
                pbi-rs search [--bm25] <query>\n\
                pbi-rs --message <question>\n\
                pbi-rs --debug-config\n\
         Default/search output is compact source-verified BM25 evidence; --bm25 relays raw Probe output."
    );
}

fn main() {
    let code = match run(env::args().skip(1).collect()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("pbi-rs: {}", error.message);
            error.code
        }
    };
    std::process::exit(code);
}

struct CliError {
    code: i32,
    message: String,
}

impl CliError {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            code: 2,
            message: message.into(),
        }
    }

    fn failed(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: message.into(),
        }
    }
}

fn run(arguments: Vec<String>) -> Result<i32, CliError> {
    if arguments.is_empty() {
        usage();
        return Ok(2);
    }
    if arguments
        .iter()
        .any(|argument| argument == "--help" || argument == "-h")
    {
        usage();
        return Ok(0);
    }
    if arguments[0] == "--version" {
        println!("pbi-rs {VERSION}");
        return Ok(0);
    }
    if arguments[0] == "--debug-config" {
        println!(
            "probe_binary={}",
            env::var("PBI_RS_PROBE").unwrap_or_else(|_| "probe".to_owned())
        );
        println!("search_default=compact_verified_bm25_no_chat");
        println!("search_bm25_opt_in=--bm25_raw_no_llm_probe");
        println!("model_path=not_configured_adk_workflow_kit_seam_pending");
        println!("api_key=[REDACTED]");
        return Ok(0);
    }

    let (raw, query, timeout, max_results) = if arguments[0] == "search" {
        parse_search(&arguments[1..])?
    } else if arguments[0] == "--message" {
        let query = arguments[1..].join(" ");
        if query.trim().is_empty() {
            return Err(CliError::usage(
                "question is required; interactive mode is disabled",
            ));
        }
        (
            false,
            query,
            DEFAULT_TIMEOUT.to_owned(),
            DEFAULT_MAX_RESULTS,
        )
    } else {
        let query = arguments.join(" ");
        if query.trim().is_empty() {
            return Err(CliError::usage(
                "question is required; interactive mode is disabled",
            ));
        }
        (
            false,
            query,
            DEFAULT_TIMEOUT.to_owned(),
            DEFAULT_MAX_RESULTS,
        )
    };

    let root =
        env::current_dir().map_err(|_| CliError::failed("cannot determine repository root"))?;
    let output = invoke_probe(&root, &query, &timeout, max_results, raw)?;
    if raw {
        io::stdout()
            .write_all(&output.stdout)
            .map_err(|_| CliError::failed("cannot write Probe output"))?;
        io::stderr()
            .write_all(&output.stderr)
            .map_err(|_| CliError::failed("cannot write Probe diagnostics"))?;
        return Ok(exit_status(&output));
    }
    if !output.status.success() {
        io::stderr()
            .write_all(&output.stderr)
            .map_err(|_| CliError::failed("cannot write Probe diagnostics"))?;
        return Ok(exit_status(&output));
    }
    let probe_stdout = String::from_utf8_lossy(&output.stdout);
    let locations = verify_probe_locations(&probe_stdout, &root, &query, max_results)
        .map_err(evidence_cli_error)?;
    for location in locations {
        println!(
            "{}",
            location
                .display_relative(&root)
                .map_err(evidence_cli_error)?
        );
    }
    Ok(0)
}

fn parse_search(arguments: &[String]) -> Result<(bool, String, String, usize), CliError> {
    let mut raw = false;
    let mut timeout = DEFAULT_TIMEOUT.to_owned();
    let mut max_results = DEFAULT_MAX_RESULTS;
    let mut query_parts = Vec::new();
    let mut after_separator = false;
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if after_separator {
            query_parts.push(argument.clone());
            index += 1;
            continue;
        }
        match argument.as_str() {
            "--" => {
                after_separator = true;
                index += 1;
            }
            "--bm25" => {
                raw = true;
                index += 1;
            }
            "--timeout" => {
                timeout = next_value(arguments, &mut index, "--timeout")?;
                validate_decimal(&timeout, "--timeout")?;
            }
            value if value.starts_with("--timeout=") => {
                timeout = value[10..].to_owned();
                validate_decimal(&timeout, "--timeout")?;
                index += 1;
            }
            "--max-results" => {
                let value = next_value(arguments, &mut index, "--max-results")?;
                max_results = value
                    .parse::<usize>()
                    .map_err(|_| CliError::usage("--max-results must be a positive integer"))?;
                if max_results == 0 {
                    return Err(CliError::usage("--max-results must be a positive integer"));
                }
            }
            value if value.starts_with("--max-results=") => {
                max_results = value[14..]
                    .parse::<usize>()
                    .map_err(|_| CliError::usage("--max-results must be a positive integer"))?;
                if max_results == 0 {
                    return Err(CliError::usage("--max-results must be a positive integer"));
                }
                index += 1;
            }
            value if value.starts_with('-') => {
                return Err(CliError::usage(format!(
                    "unsupported search option: {value}"
                )));
            }
            value => {
                query_parts.push(value.to_owned());
                index += 1;
            }
        }
    }
    let query = query_parts.join(" ");
    if query.trim().is_empty() {
        return Err(CliError::usage("search query is required"));
    }
    Ok((raw, query, timeout, max_results))
}

fn next_value(arguments: &[String], index: &mut usize, option: &str) -> Result<String, CliError> {
    *index += 1;
    let value = arguments
        .get(*index)
        .filter(|value| !value.starts_with('-'))
        .cloned()
        .ok_or_else(|| CliError::usage(format!("{option} requires a value")))?;
    *index += 1;
    Ok(value)
}

fn validate_decimal(value: &str, option: &str) -> Result<(), CliError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(CliError::usage(format!(
            "{option} must be a non-negative integer"
        )));
    }
    Ok(())
}

fn invoke_probe(
    root: &PathBuf,
    query: &str,
    timeout: &str,
    max_results: usize,
    raw: bool,
) -> Result<Output, CliError> {
    let probe = env::var_os("PBI_RS_PROBE").unwrap_or_else(|| "probe".into());
    let mut command = Command::new(probe);
    command.current_dir(root).args([
        "search",
        "--timeout",
        timeout,
        "--max-results",
        &max_results.to_string(),
        "--ignore",
        "drafts",
        "--reranker",
        "bm25",
    ]);
    if !raw {
        command.args(["--format", "plain", "--dry-run"]);
    }
    command.args(["--", query]);
    command.output().map_err(|_| CliError {
        code: 127,
        message: "probe is unavailable on PATH".to_owned(),
    })
}

fn exit_status(output: &Output) -> i32 {
    output.status.code().unwrap_or(1)
}

fn evidence_cli_error(error: EvidenceError) -> CliError {
    CliError::failed(error.to_string())
}
