use chrono::Utc;
use serde_json::json;
use std::env;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const DEFAULT_MODES: &[&str] = &[
    "ramp",
    "mixed-scale",
    "bonding",
    "burst-verify",
    "hls-put",
    "bframe-rtmp",
];

#[derive(Debug)]
struct Config {
    run_id: String,
    work_root: Option<PathBuf>,
    modes: Vec<String>,
    run_flags: Vec<String>,
    extra_args: Vec<String>,
    continue_on_fail: bool,
    preflight_only: bool,
    restream_bin: Option<String>,
}

#[derive(Debug)]
struct ModeResult {
    mode: String,
    status: &'static str,
    preflight_status: &'static str,
    exit_code: i32,
    started_at: String,
    finished_at: String,
    mode_dir: PathBuf,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let config = parse_args(env::args().skip(1).collect())?;
    let root = env::current_dir().map_err(|err| err.to_string())?;
    let runner = root.join("test/run-integration.sh");
    let work_root = absolute_work_root(&root, &config);
    let results_jsonl = work_root.join("results.jsonl");
    let matrix_manifest = work_root.join("manifest.json");
    let started_at = now();

    fs::create_dir_all(&work_root).map_err(|err| err.to_string())?;
    File::create(&results_jsonl).map_err(|err| err.to_string())?;
    write_matrix_manifest(
        &matrix_manifest,
        "RUNNING",
        &started_at,
        None,
        &root,
        &work_root,
        &results_jsonl,
        &config,
    )?;

    let mut overall = 0;
    for mode in &config.modes {
        let mode = mode.trim();
        if mode.is_empty() {
            continue;
        }
        let passed = run_mode(mode, &runner, &work_root, &results_jsonl, &config)?;
        if !passed {
            overall = 1;
            if !config.continue_on_fail {
                break;
            }
        }
    }

    let finished_at = now();
    write_matrix_manifest(
        &matrix_manifest,
        if overall == 0 { "PASS" } else { "FAIL" },
        &started_at,
        Some(&finished_at),
        &root,
        &work_root,
        &results_jsonl,
        &config,
    )?;
    println!("[matrix] manifest={}", matrix_manifest.display());

    if overall == 0 {
        Ok(())
    } else {
        Err("protocol matrix failed".to_string())
    }
}

fn parse_args(args: Vec<String>) -> Result<Config, String> {
    let mut config = Config {
        run_id: env::var("RUN_ID")
            .unwrap_or_else(|_| Utc::now().format("%Y%m%dT%H%M%SZ").to_string()),
        work_root: env::var_os("WORK_ROOT").map(PathBuf::from),
        modes: DEFAULT_MODES
            .iter()
            .map(|mode| (*mode).to_string())
            .collect(),
        run_flags: Vec::new(),
        extra_args: Vec::new(),
        continue_on_fail: false,
        preflight_only: false,
        restream_bin: None,
    };
    let mut work_root_explicit = config.work_root.is_some();
    let mut iter = args.into_iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--run-id" => {
                config.run_id = next_value(&mut iter, "--run-id")?;
                if !work_root_explicit {
                    config.work_root = None;
                }
            }
            "--work-root" => {
                config.work_root = Some(PathBuf::from(next_value(&mut iter, "--work-root")?));
                work_root_explicit = true;
            }
            "--only-modes" => {
                config.modes = next_value(&mut iter, "--only-modes")?
                    .split(',')
                    .map(|mode| mode.trim().to_string())
                    .filter(|mode| !mode.is_empty())
                    .collect();
            }
            "--host" | "--fast" | "--skip-load" => config.run_flags.push(arg),
            "--continue-on-fail" => config.continue_on_fail = true,
            "--preflight-only" => config.preflight_only = true,
            "--restream-bin" => {
                config.restream_bin = Some(next_value(&mut iter, "--restream-bin")?)
            }
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            "--" => {
                config.extra_args.extend(iter);
                break;
            }
            _ => return Err(format!("Unknown option: {arg}\n\n{}", usage())),
        }
    }

    if config.modes.is_empty() {
        return Err("--only-modes did not include any modes".to_string());
    }
    Ok(config)
}

fn next_value(iter: &mut impl Iterator<Item = String>, option: &str) -> Result<String, String> {
    iter.next()
        .ok_or_else(|| format!("{option} requires a value"))
}

fn absolute_work_root(root: &Path, config: &Config) -> PathBuf {
    let work_root = config
        .work_root
        .clone()
        .unwrap_or_else(|| root.join("test/artifacts").join(&config.run_id));
    if work_root.is_absolute() {
        work_root
    } else {
        root.join(work_root)
    }
}

fn run_mode(
    mode: &str,
    runner: &Path,
    work_root: &Path,
    results_jsonl: &Path,
    config: &Config,
) -> Result<bool, String> {
    let mode_dir = work_root.join(mode);
    fs::create_dir_all(&mode_dir).map_err(|err| err.to_string())?;
    let started_at = now();
    let mut status = "PASS";
    let mut preflight_status = "PASS";
    let mut exit_code = 0;

    println!("[matrix] preflight {mode}");
    let preflight = run_integration(
        runner,
        mode,
        &mode_dir,
        &mode_dir.join("preflight.log"),
        &mode_dir.join("preflight.jsonl"),
        true,
        config,
    )?;
    if !preflight.success {
        preflight_status = "FAIL";
        status = "FAIL";
        exit_code = preflight.code;
    }

    if status == "PASS" && config.preflight_only {
        println!("[matrix] skip run {mode} (preflight-only)");
    } else if status == "PASS" {
        println!("[matrix] run {mode}");
        let run = run_integration(
            runner,
            mode,
            &mode_dir,
            &mode_dir.join("run.log"),
            &mode_dir.join("assertions.jsonl"),
            false,
            config,
        )?;
        if !run.success {
            status = "FAIL";
            exit_code = run.code;
        }
    }

    let finished_at = now();
    let result = ModeResult {
        mode: mode.to_string(),
        status,
        preflight_status,
        exit_code,
        started_at,
        finished_at,
        mode_dir,
    };
    append_result(results_jsonl, &result)?;
    println!("[matrix] {mode}: {status}");
    Ok(status == "PASS")
}

struct CommandStatus {
    success: bool,
    code: i32,
}

fn run_integration(
    runner: &Path,
    mode: &str,
    mode_dir: &Path,
    log_path: &Path,
    json_path: &Path,
    preflight: bool,
    config: &Config,
) -> Result<CommandStatus, String> {
    let log = File::create(log_path).map_err(|err| err.to_string())?;
    let err_log = log.try_clone().map_err(|err| err.to_string())?;
    let mut command = Command::new(runner);
    command
        .env("WORK_DIR", mode_dir)
        .env(
            "RESTREAM_BIN",
            config
                .restream_bin
                .clone()
                .or_else(|| env::var("RESTREAM_BIN").ok())
                .unwrap_or_default(),
        )
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err_log));
    command.args(&config.run_flags);
    if preflight {
        command.arg("--preflight");
    }
    command.arg("--json").arg(json_path);
    command.args(&config.extra_args);
    command.arg(mode);

    let status = command.status().map_err(|err| err.to_string())?;
    Ok(CommandStatus {
        success: status.success(),
        code: status.code().unwrap_or(1),
    })
}

fn write_matrix_manifest(
    manifest_path: &Path,
    status: &str,
    started_at: &str,
    finished_at: Option<&str>,
    root: &Path,
    work_root: &Path,
    results_jsonl: &Path,
    config: &Config,
) -> Result<(), String> {
    let git_head = git_head(root);
    let manifest = json!({
        "kind": "protocol-matrix",
        "status": status,
        "runId": config.run_id,
        "startedAt": started_at,
        "finishedAt": finished_at,
        "gitHead": git_head,
        "workRoot": work_root,
        "preflightOnly": config.preflight_only,
        "modes": config.modes,
        "resultsJsonl": results_jsonl,
    });
    write_json(manifest_path, &manifest)
}

fn append_result(path: &Path, result: &ModeResult) -> Result<(), String> {
    use std::io::Write;

    let line = json!({
        "mode": result.mode,
        "status": result.status,
        "preflightStatus": result.preflight_status,
        "exitCode": result.exit_code,
        "startedAt": result.started_at,
        "finishedAt": result.finished_at,
        "workDir": result.mode_dir,
        "manifest": result.mode_dir.join("manifest.json"),
        "assertions": result.mode_dir.join("assertions.jsonl"),
        "preflight": result.mode_dir.join("preflight.jsonl"),
        "log": result.mode_dir.join("run.log"),
    });
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|err| err.to_string())?;
    writeln!(
        file,
        "{}",
        serde_json::to_string(&line).map_err(|err| err.to_string())?
    )
    .map_err(|err| err.to_string())
}

fn write_json(path: &Path, value: &serde_json::Value) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|err| err.to_string())?;
    fs::write(path, bytes).map_err(|err| err.to_string())
}

fn git_head(root: &Path) -> String {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("rev-parse")
        .arg("--short")
        .arg("HEAD")
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
            } else {
                None
            }
        })
        .filter(|head| !head.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn now() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn print_usage() {
    println!("{}", usage());
}

fn usage() -> String {
    format!(
        "Usage: protocol_matrix [options] [-- extra run-integration args]\n\n\
         Options:\n\
           --run-id <id>           Artifact run id (default: UTC timestamp)\n\
           --work-root <path>      Aggregate artifact root (default: test/artifacts/<run-id>)\n\
           --only-modes <list>     Comma-separated mode list\n\
           --host                  Pass --host to run-integration.sh\n\
           --fast                  Pass --fast to run-integration.sh\n\
           --skip-load             Pass --skip-load to run-integration.sh\n\
           --continue-on-fail      Run remaining modes after a failure\n\
           --preflight-only        Run aggregate preflight for all selected modes only\n\
           --restream-bin <path>   RESTREAM_BIN for non-bonding modes\n\
           -h, --help              Show this help\n\n\
         Default modes: {}",
        DEFAULT_MODES.join(" ")
    )
}
