//! All in Luna orchestration bridge.
//!
//! All in Luna (https://github.com/zenx0x/allinluna) is an external Python
//! runtime that compiles a goal into typed Tasks with contracts, dependencies,
//! ownership, done-when conditions and verification specs, then emits exact
//! `HostAction` opcodes for the host to relay.
//!
//! Snow App cannot execute `codex_app__*` opcodes directly, so this server plays
//! two roles:
//!
//! 1. **CLI relay** — every tool shells out to the `allinluna` runtime with one
//!    absolute Runtime DB, exactly as the upstream skill contract requires.
//! 2. **Host adapter** — `allinluna-actions` rewrites each emitted action into
//!    a concrete Snow App tool call (`snowApp` block), so the agent can relay it
//!    without guessing. The agent executes that Snow App tool and returns the
//!    observed receipt through `allinluna-receipt`.
//!
//! Truth boundary (upstream invariant #6): an action is not completion. Only an
//! ingested `host-receipt/v1` moves a Task forward, and `actual_tool` /
//! `actual_capability` / `action_contract_hash` are never inferred here — they
//! are copied from the receipt the host actually observed.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use napi::bindgen_prelude::*;
use serde_json::{json, Value};

use super::super::service::McpService;
use super::super::tools::McpTool;

const SERVER_ID: &str = "allinluna";

/// Pinned runtime version. Upstream ships release candidates only; pinning keeps
/// the CLI surface (subcommand names, receipt shapes) reproducible.
const PINNED_SPEC: &str = "allinluna==2.0.0rc7";

/// The upstream runtime exposes `--adapter {codex-app,deepseek-harness}`. Both
/// adapters emit exact host opcodes; `codex-app` is the one whose opcode family
/// (`codex_app__*`) this server knows how to translate into Snow App tools.
const DEFAULT_ADAPTER: &str = "codex-app";

const DEFAULT_TIMEOUT_SECS: u64 = 180;
const MAX_TIMEOUT_SECS: u64 = 1800;

/// How the runtime is invoked on this machine.
enum Launcher {
    /// A console script already on PATH (or an absolute path from env).
    Direct(String),
    /// `uvx --from <pinned spec> allinluna` — isolated, no global install.
    Uvx,
    /// `python -m allinluna_runtime` — fallback when uvx is unavailable.
    PythonModule,
}

fn launcher() -> &'static Launcher {
    static LAUNCHER: OnceLock<Launcher> = OnceLock::new();
    LAUNCHER.get_or_init(|| {
        if let Ok(custom) = std::env::var("SNOW_ALLINLUNA_BIN") {
            let custom = custom.trim().to_string();
            if !custom.is_empty() {
                return Launcher::Direct(custom);
            }
        }
        if probe_binary("allinluna") {
            return Launcher::Direct("allinluna".to_string());
        }
        if probe_binary("uvx") {
            return Launcher::Uvx;
        }
        Launcher::PythonModule
    })
}

fn probe_binary(binary: &str) -> bool {
    Command::new(binary)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Resolve the Runtime DB path. Upstream requires ONE absolute DB per run and
/// the skill forbids switching stores mid-run, so the path is derived the same
/// way for every subcommand.
fn resolve_db(args: &Value) -> napi::Result<PathBuf> {
    if let Some(path) = args.get("dbPath").and_then(Value::as_str) {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }
    let root = args
        .get("repositoryRoot")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            Error::new(
                Status::InvalidArg,
                "Provide `repositoryRoot` (absolute repo path) or `dbPath` (absolute runtime DB path). \
                 Both are absolute on purpose: the upstream runtime must reuse one exact Store for the whole run.",
            )
        })?;
    Ok(PathBuf::from(root).join(".allinluna").join("runtime.db"))
}

fn timeout_of(args: &Value) -> Duration {
    let secs = args
        .get("timeoutSeconds")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(5, MAX_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

fn required_str<'a>(args: &'a Value, key: &str) -> napi::Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            Error::new(
                Status::InvalidArg,
                format!("Missing or invalid parameter: `{key}` (a non-empty string)"),
            )
        })
}

struct CommandOutcome {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Spawn the runtime with a hard timeout. Output is drained on dedicated
/// threads so a chatty child can never deadlock on a full pipe.
fn run_with_timeout(mut command: Command, timeout: Duration) -> napi::Result<CommandOutcome> {
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let mut child = command.spawn().map_err(|error| {
        Error::new(
            Status::GenericFailure,
            format!("Failed to start the All in Luna runtime: {error}"),
        )
    })?;

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let stdout_handle = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(pipe) = stdout_pipe.as_mut() {
            let _ = pipe.read_to_end(&mut buffer);
        }
        buffer
    });
    let stderr_handle = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(pipe) = stderr_pipe.as_mut() {
            let _ = pipe.read_to_end(&mut buffer);
        }
        buffer
    });

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = stdout_handle.join().unwrap_or_default();
                let stderr = stderr_handle.join().unwrap_or_default();
                return Ok(CommandOutcome {
                    code: status.code().unwrap_or(-1),
                    stdout: decode(&stdout),
                    stderr: decode(&stderr),
                });
            }
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_handle.join();
                    let _ = stderr_handle.join();
                    return Err(Error::new(
                        Status::GenericFailure,
                        format!(
                            "All in Luna timed out after {}s. Increase `timeoutSeconds` or use a narrower goal.",
                            timeout.as_secs()
                        ),
                    ));
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(error) => {
                return Err(Error::new(
                    Status::GenericFailure,
                    format!("Failed while waiting for the All in Luna runtime: {error}"),
                ));
            }
        }
    }
}

fn decode(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim_end().to_string()
}

/// Build the runtime argv: `allinluna [--db <db>] --adapter <adapter> <tail...>`.
/// `--db` is optional at the CLI level (only `capabilities` omits it), so the
/// caller decides whether a Store is required.
fn build_command(db: Option<&Path>, adapter: &str, tail: &[String]) -> Command {
    let mut command = match launcher() {
        Launcher::Direct(binary) => {
            let mut command = Command::new(binary);
            if let Some(db) = db {
                command.arg("--db").arg(db);
            }
            command
        }
        Launcher::Uvx => {
            let mut command = Command::new("uvx");
            command.args(["--from", PINNED_SPEC, "allinluna"]);
            if let Some(db) = db {
                command.arg("--db").arg(db);
            }
            command
        }
        Launcher::PythonModule => {
            let mut command = Command::new("python");
            command.args(["-m", "allinluna_runtime"]);
            if let Some(db) = db {
                command.arg("--db").arg(db);
            }
            command
        }
    };
    command.arg("--adapter").arg(adapter);
    for item in tail {
        command.arg(item);
    }
    // The runtime prints JSON through Python's stdout; force UTF-8 so Windows
    // code pages cannot corrupt non-ASCII goals.
    command.env("PYTHONIOENCODING", "utf-8");
    command.env("PYTHONUTF8", "1");
    command
}

/// The persistent host helper is compiled into the binary, so an installed Snow
/// App never depends on a checkout of this repository.
const HELPER_SOURCE: &str = include_str!("../../../../scripts/allinluna_host.py");

/// One live helper process.
///
/// The runtime's `ActionBridge` keeps its dispatch queue and its
/// `CoordinatorEngine` in process memory, and a `LaneBootstrapEnvelope` is only
/// reachable through the `dispatch_outbox` row written by the scheduler. A
/// per-call CLI process therefore cannot link a receipt to its action; one
/// long-lived helper can.
struct HostSession {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    key: String,
    next_id: u64,
}

static HOST_SESSION: OnceLock<Mutex<Option<HostSession>>> = OnceLock::new();

fn write_helper_script() -> napi::Result<PathBuf> {
    let directory = crate::storage::paths::app_storage_dir()?.join("allinluna");
    std::fs::create_dir_all(&directory).map_err(|error| {
        Error::new(
            Status::GenericFailure,
            format!(
                "Failed to create the All in Luna helper directory {}: {error}",
                directory.display()
            ),
        )
    })?;
    let path = directory.join("host_helper.py");
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if current != HELPER_SOURCE {
        std::fs::write(&path, HELPER_SOURCE).map_err(|error| {
            Error::new(
                Status::GenericFailure,
                format!(
                    "Failed to write the All in Luna helper {}: {error}",
                    path.display()
                ),
            )
        })?;
    }
    Ok(path)
}

/// The runtime deliberately owns no concrete model name: it asks the host to map
/// a capability class to a route. The bridge must answer with the model this app
/// is actually configured to use, otherwise every top-level action degrades into
/// the non-executable `resolve-resource-route` placeholder.
fn active_profile_model() -> Option<String> {
    let storage = crate::storage::paths::app_storage_dir().ok()?;
    let database = crate::storage::paths::database_file_path(&storage);
    let configs = crate::storage::services::api_configs::list_api_configs(&database).ok()?;
    configs
        .into_iter()
        .find(|config| config.is_active)
        .map(|config| config.advanced_model.trim().to_string())
        .filter(|model| !model.is_empty())
}

fn resolve_model(args: &Value) -> String {
    if let Some(model) = args
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return model.to_string();
    }
    active_profile_model().unwrap_or_default()
}

fn text_arg(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string()
}

fn resolve_project_id(args: &Value, root: &str) -> String {
    let value = text_arg(args, "projectId");
    if value.is_empty() {
        format!("local:{root}")
    } else {
        value
    }
}

/// Repository root: the explicit argument first, otherwise recovered from a
/// `<root>/.allinluna/<db>` runtime DB path.
fn resolve_root(args: &Value) -> napi::Result<String> {
    let explicit = text_arg(args, "repositoryRoot");
    if !explicit.is_empty() {
        return Ok(explicit);
    }
    let db = text_arg(args, "dbPath");
    if !db.is_empty() {
        let path = PathBuf::from(db);
        let storage = path.parent().and_then(Path::parent);
        let inside_runtime_dir = path
            .parent()
            .and_then(Path::file_name)
            .map(|name| name == ".allinluna")
            .unwrap_or(false);
        if let (true, Some(storage)) = (inside_runtime_dir, storage) {
            return Ok(storage.to_string_lossy().to_string());
        }
    }
    Err(Error::new(
        Status::InvalidArg,
        "Provide `repositoryRoot` (absolute repo path) or a `dbPath` under `<root>/.allinluna/`, \
         so the All in Luna host session can bind one durable project identity.",
    ))
}

fn ensure_db_directory(path: &Path) -> napi::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|error| {
                Error::new(
                    Status::GenericFailure,
                    format!(
                        "Failed to create the runtime DB directory {}: {error}",
                        parent.display()
                    ),
                )
            })?;
        }
    }
    Ok(())
}

fn session_key(db: &Path, root: &str, project_id: &str, branch: &str, model: &str) -> String {
    format!("{}|{root}|{project_id}|{branch}|{model}", db.display())
}

fn timeout_error(timeout: Duration) -> Error {
    Error::new(
        Status::GenericFailure,
        format!(
            "All in Luna timed out after {}s. Increase `timeoutSeconds` or use a narrower goal.",
            timeout.as_secs()
        ),
    )
}

/// Fail before spawning when the interpreter cannot import the runtime: a bare
/// "helper exited unexpectedly" is impossible to diagnose from the tool result.
fn ensure_runtime_importable(python: &str) -> napi::Result<()> {
    let output = Command::new(python)
        .args(["-c", "import allinluna_runtime"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    match output {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(Error::new(
            Status::GenericFailure,
            format!(
                "`{python}` cannot import allinluna_runtime: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        )),
        Err(error) => Err(Error::new(
            Status::GenericFailure,
            format!(
                "Failed to run `{python}`: {error}. Set `SNOW_ALLINLUNA_PYTHON` to the interpreter \
                 that has the All in Luna runtime installed."
            ),
        )),
    }
}

fn spawn_session(
    db: &Path,
    root: &str,
    project_id: &str,
    branch: &str,
    model: &str,
    reasoning: &str,
) -> napi::Result<HostSession> {
    let script = write_helper_script()?;
    let python = std::env::var("SNOW_ALLINLUNA_PYTHON")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "python".to_string());
    ensure_runtime_importable(&python)?;
    let mut command = Command::new(&python);
    command
        // The helper rebuilds each LaneBootstrapEnvelope from the store, and in
        // projectless mode the runtime derives `workspace` from `Path.cwd()`
        // (lane_bootstrap.py). Without a stable cwd the workspace follows
        // whatever directory the app process happens to run in, so `file:`
        // artifacts resolve outside it and `complete-work-unit` rejects them.
        // `resolve_root` only checks for a non-empty string, so stay defensive:
        // an absent root keeps the previous behaviour instead of failing spawn.
        .arg(&script)
        .arg("--db")
        .arg(db)
        .arg("--project-root")
        .arg(root)
        .arg("--workspace")
        .arg(root)
        .arg("--project-id")
        .arg(project_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1");
    if Path::new(root).is_dir() {
        command.current_dir(root);
    }
    if !branch.is_empty() {
        command.arg("--branch").arg(branch);
    }
    if !model.is_empty() {
        command.arg("--model").arg(model);
    }
    if !reasoning.is_empty() {
        command.arg("--reasoning").arg(reasoning);
    }
    let mut child = command.spawn().map_err(|error| {
        Error::new(
            Status::GenericFailure,
            format!(
                "Failed to start the All in Luna host helper with `{python}`: {error}. \
                 Install the runtime (`pip install -e <allinluna>`) or set `SNOW_ALLINLUNA_PYTHON`."
            ),
        )
    })?;
    let stdin = child.stdin.take().ok_or_else(|| {
        Error::new(
            Status::GenericFailure,
            "The All in Luna host helper has no stdin pipe",
        )
    })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        Error::new(
            Status::GenericFailure,
            "The All in Luna host helper has no stdout pipe",
        )
    })?;
    let stderr_tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
    if let Some(stderr) = child.stderr.take() {
        // Drain stderr (a chatty helper must never block on a full pipe) and
        // keep the last lines so a crash stays diagnosable.
        let tail = Arc::clone(&stderr_tail);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if let Ok(mut buffer) = tail.lock() {
                            buffer.push_back(line.clone());
                            while buffer.len() > 40 {
                                buffer.pop_front();
                            }
                        }
                    }
                }
            }
        });
    }
    let (sender, lines) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) => {
                    if sender.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    Ok(HostSession {
        child,
        stdin,
        lines,
        stderr_tail,
        key: session_key(db, root, project_id, branch, model),
        next_id: 0,
    })
}

impl HostSession {
    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> napi::Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let mut line = serde_json::to_string(&json!({ "id": id, "method": method, "params": params }))
            .map_err(|error| {
                Error::new(
                    Status::GenericFailure,
                    format!("Failed to encode the All in Luna helper request: {error}"),
                )
            })?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.flush())
            .map_err(|error| {
                Error::new(
                    Status::GenericFailure,
                    format!("Failed to send the All in Luna helper request: {error}"),
                )
            })?;
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(timeout_error(timeout));
            }
            match self.lines.recv_timeout(remaining) {
                Ok(text) => {
                    let value: Value = serde_json::from_str(&text).map_err(|error| {
                        Error::new(
                            Status::GenericFailure,
                            format!("The All in Luna host helper returned invalid JSON: {error}"),
                        )
                    })?;
                    if value.get("id").and_then(Value::as_u64) != Some(id) {
                        continue;
                    }
                    if value.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                        return Ok(value.get("result").cloned().unwrap_or(Value::Null));
                    }
                    return Ok(json!({
                        "success": false,
                        "error": value.get("error").cloned().unwrap_or(Value::Null),
                        "traceback": value.get("traceback").cloned().unwrap_or(Value::Null),
                    }));
                }
                Err(RecvTimeoutError::Timeout) => return Err(timeout_error(timeout)),
                Err(RecvTimeoutError::Disconnected) => {
                    let detail = self
                        .stderr_tail
                        .lock()
                        .map(|lines| lines.iter().cloned().collect::<String>().trim().to_string())
                        .unwrap_or_default();
                    let suffix = if detail.is_empty() {
                        String::new()
                    } else {
                        format!(" Last stderr: {detail}")
                    };
                    return Err(Error::new(
                        Status::GenericFailure,
                        format!(
                            "The All in Luna host helper exited unexpectedly.{suffix} Re-run the tool \
                             once; if it repeats, verify that `python -c \"import allinluna_runtime\"` \
                             succeeds or set `SNOW_ALLINLUNA_PYTHON`."
                        ),
                    ));
                }
            }
        }
    }
}

/// Run one request against the single long-lived host session, respawning it
/// only when the bound Store/identity/model changed or the process died.
fn with_session<T>(
    args: &Value,
    action: impl FnOnce(&mut HostSession) -> napi::Result<T>,
) -> napi::Result<T> {
    let db = resolve_db(args)?;
    ensure_db_directory(&db)?;
    let root = resolve_root(args)?;
    let project_id = resolve_project_id(args, &root);
    let branch = text_arg(args, "branch");
    let model = resolve_model(args);
    let reasoning = text_arg(args, "reasoning");
    let key = session_key(&db, &root, &project_id, &branch, &model);

    let slot = HOST_SESSION.get_or_init(|| Mutex::new(None));
    let mut guard = slot.lock().map_err(|_| {
        Error::new(
            Status::GenericFailure,
            "The All in Luna host session lock is poisoned",
        )
    })?;
    let healthy = match guard.as_mut() {
        Some(session) => session.key == key && matches!(session.child.try_wait(), Ok(None)),
        None => false,
    };
    if !healthy {
        if let Some(mut stale) = guard.take() {
            let _ = stale.child.kill();
            let _ = stale.child.wait();
        }
        *guard = Some(spawn_session(
            &db,
            &root,
            &project_id,
            &branch,
            &model,
            &reasoning,
        )?);
    }
    let session = guard.as_mut().ok_or_else(|| {
        Error::new(
            Status::GenericFailure,
            "The All in Luna host session is unavailable",
        )
    })?;
    action(session)
}

/// Send one helper method and wrap its result in the same envelope shape the CLI
/// path returns, so every `allinluna-*` tool answers consistently.
fn session_call(args: &Value, method: &str, params: Value) -> napi::Result<Value> {
    let timeout = timeout_of(args);
    let result = with_session(args, |session| session.request(method, params, timeout))?;
    let runtime_db = resolve_db(args)
        .ok()
        .map(|path| path.to_string_lossy().to_string());
    if result.get("success").and_then(Value::as_bool) == Some(false) {
        return Ok(json!({
            "success": false,
            "runtimeDb": runtime_db,
            "method": method,
            "error": result.get("error").cloned().unwrap_or(Value::Null),
            "traceback": result.get("traceback").cloned().unwrap_or(Value::Null),
            "hint": "This is the runtime's own error. Read `error`/`traceback`; do not retry blindly with different arguments."
        }));
    }
    Ok(json!({
        "success": true,
        "runtimeDb": runtime_db,
        "method": method,
        "result": result
    }))
}

/// Invoke the runtime and parse its JSON. A non-zero exit is reported with the
/// captured stderr, never silently swallowed.
///
/// `require_db` is false only for `capabilities`, which is a pure capability
/// query and must not demand a Runtime DB the caller may not have created yet.
fn invoke(args: &Value, tail: &[String]) -> napi::Result<Value> {
    // Every Store-bearing subcommand reuses the same absolute Runtime DB.
    invoke_with_db(args, tail, true)
}

/// Capability queries are stateless: they must not demand a Runtime DB that the
/// caller may not have created yet.
fn invoke_stateless(tail: &[String]) -> napi::Result<Value> {
    invoke_with_db(&json!({}), tail, false)
}

fn invoke_with_db(args: &Value, tail: &[String], require_db: bool) -> napi::Result<Value> {
    let db = if require_db {
        let path = resolve_db(args)?;
        ensure_db_directory(&path)?;
        Some(path)
    } else {
        None
    };
    let adapter = args
        .get("adapter")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_ADAPTER);

    let command = build_command(db.as_deref(), adapter, tail);
    let outcome = run_with_timeout(command, timeout_of(args))?;

    let runtime_db = db.as_ref().map(|path| path.to_string_lossy().to_string());
    let parsed: Option<Value> = serde_json::from_str(&outcome.stdout).ok();
    if outcome.code != 0 {
        return Ok(json!({
            "success": false,
            "exitCode": outcome.code,
            "runtimeDb": runtime_db,
            "stdout": outcome.stdout,
            "stderr": outcome.stderr,
            "hint": "Read `stderr` for the runtime's own validation message. Do not retry blindly with different arguments.",
        }));
    }

    match parsed {
        Some(value) => Ok(json!({
            "success": true,
            "runtimeDb": runtime_db,
            "result": value,
        })),
        None => Ok(json!({
            "success": true,
            "runtimeDb": runtime_db,
            "stdout": outcome.stdout,
            "stderr": outcome.stderr,
            "note": "The runtime returned non-JSON output for this subcommand.",
        })),
    }
}

/// Translate one emitted `HostAction.tool` into the Snow App tool the agent must
/// actually call. Returning `None` means "no known mapping" — the agent then has
/// to inspect the raw action, and the runtime's `on_unavailable` policy applies.
fn snow_app_mapping(tool: &str, payload: &Value) -> Option<Value> {
    match tool {
        "codex_app__list_projects" => Some(json!({
            "tool": "config-list",
            "arguments": { "scope": "workspace", "key": "directories" },
            "receiptFields": {
                "actual_tool": "config-list",
                "actual_capability": "workspace.directories.list"
            },
            "note": "Map payload.project_root to the directory whose `path` matches it, then report that directoryId as the resolved project identity."
        })),
        "codex_app__create_thread" => Some(json!({
            "tool": "app-control-conversation",
            "arguments": {
                "action": "create",
                "autoReply": true
            },
            "argumentsFrom": {
                "title": "action.payload.title (or the Task outcome)",
                "text": "the verbatim lane-bootstrap/v1 payload plus the child prompt"
            },
            "receiptFields": {
                "actual_tool": "app-control-conversation",
                "actual_capability": "conversation.create"
            },
            "note": "Upstream forbids falling back to a sub-agent for a top-level task. Create a real conversation with the lane-bootstrap prompt, then call `allinluna-relay` with { runId, threadId: <conversationId> } so the runtime records that this lane is actually running. A created conversation is not completion."
        })),
        "sub-agents-activate" => Some(json!({
            "tool": "sub-agents-activate",
            "argumentsFrom": {
                "agentId": "envelope.agent_id (optional; omit to use the general agent)",
                "summary": "envelope.objective (short label)",
                "prompt": "arguments.envelope (the full work-unit envelope, verbatim)"
            },
            "receiptFields": {
                "actual_tool": "sub-agents-activate",
                "actual_capability": "native_subagent",
                "action_contract_hash": "action.action_contract_hash"
            },
            "note": "Create the sub-agent with the work-unit envelope, then report the observed sub-agent conversation id with allinluna-subagent { runId, taskId, workUnitId, threadId }."
        })),
        "codex_app__wait_thread" | "codex_app__read_thread" => Some(json!({
            "tool": "app-control-conversation",
            "arguments": { "action": "activate" },
            "note": "Read the child conversation's messages, then build a receipt with the observed final state. A pending child is NOT completion: report it as unresolved."
        })),
        "codex_app__list_threads" => Some(json!({
            "tool": "app-control-conversation",
            "note": "Snow App exposes no thread listing; enumerate through the run's persisted actions instead."
        })),
        _ => {
            if tool.starts_with("codex_app__") {
                Some(json!({
                    "tool": null,
                    "note": "Unmapped codex_app opcode. Inspect payload, choose the closest Snow App capability, and record actual_tool/actual_capability from what you actually ran. If nothing fits, return ACTION_RELAY_REQUIRED semantics by reporting the action as unresolved.",
                    "payloadHint": payload
                }))
            } else {
                None
            }
        }
    }
}

/// Decorate every emitted action with its Snow App relay plan.
fn decorate_actions(actions: Option<&Value>) -> Value {
    let Some(Value::Array(items)) = actions else {
        return json!([]);
    };
    let decorated: Vec<Value> = items
        .iter()
        .map(|action| {
            let mut clone = action.clone();
            let tool = clone
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let payload = clone.get("payload").cloned().unwrap_or(Value::Null);
            if let Some(mapping) = snow_app_mapping(&tool, &payload) {
                if let Some(object) = clone.as_object_mut() {
                    object.insert("snowApp".to_string(), mapping);
                }
            }
            clone
        })
        .collect();
    json!(decorated)
}

pub struct AllInLunaService;

impl AllInLunaService {
    pub fn new() -> Self {
        AllInLunaService
    }

    fn start(&self, args: &Value) -> napi::Result<Value> {
        let goal = required_str(args, "goal")?;
        let repository_root = required_str(args, "repositoryRoot")?;

        let mut tail: Vec<String> = vec!["start".to_string(), "--goal".to_string(), goal.to_string()];
        if let Some(done_when) = args
            .get("doneWhen")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            tail.push("--done-when".to_string());
            tail.push(done_when.to_string());
        }
        if let Some(pack) = args
            .get("pack")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            tail.push("--pack".to_string());
            tail.push(pack.to_string());
        }
        if let Some(policy) = args
            .get("externalActionPolicy")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| matches!(*value, "ask" | "deny" | "allow"))
        {
            tail.push("--external-action-policy".to_string());
            tail.push(policy.to_string());
        }
        // A plain local checkout has no worktree environment, and the runtime's
        // project resolution only accepts a worktree. `--projectless` is the CLI's
        // supported escape hatch; without it every local run stalls on
        // `resolve-project` forever.
        if args
            .get("projectless")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            tail.push("--projectless".to_string());
        }
        if let Some(task_graph) = args.get("taskGraph") {
            let encoded = if let Some(text) = task_graph.as_str() {
                text.to_string()
            } else {
                serde_json::to_string(task_graph).map_err(|error| {
                    Error::new(
                        Status::InvalidArg,
                        format!("`taskGraph` must be JSON-serializable: {error}"),
                    )
                })?
            };
            tail.push("--task-graph".to_string());
            tail.push(encoded);
        }
        let intent_id = text_arg(args, "intentId");
        if !intent_id.is_empty() {
            tail.push("--intent-id".to_string());
            tail.push(intent_id);
        }
        let supersedes = text_arg(args, "supersedesRun");
        if !supersedes.is_empty() {
            tail.push("--supersedes-run".to_string());
            tail.push(supersedes);
        }
        // A top-level lane is only executable once the run carries a concrete
        // model. The runtime refuses to invent one, so the bridge supplies the
        // model this app is configured with (or the explicit override).
        let model = resolve_model(args);
        if !model.is_empty() {
            tail.push("--model".to_string());
            tail.push(model);
        }
        let reasoning = text_arg(args, "reasoning");
        if !reasoning.is_empty() {
            tail.push("--reasoning".to_string());
            tail.push(reasoning);
        }
        let dispatch = args
            .get("dispatch")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !dispatch {
            tail.push("--no-dispatch".to_string());
        }
        // `repositoryRoot` also decides the default DB path, so it must be an
        // explicit input rather than an inferred working directory.
        let root = PathBuf::from(repository_root);
        if !root.is_absolute() {
            return Err(Error::new(
                Status::InvalidArg,
                "`repositoryRoot` must be an absolute path (e.g. H:/github/snow-app).",
            ));
        }
        tail.push("--repository-root".to_string());
        tail.push(repository_root.to_string());

        let result = invoke(args, &tail)?;
        let run_ref = result
            .get("result")
            .and_then(|value| value.get("run_ref"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let actions = result.get("result").and_then(|value| value.get("actions"));
        let compiled = decorate_actions(actions);
        let mut object = result.as_object().cloned().unwrap_or_default();
        object.insert("dispatched".to_string(), json!(dispatch));
        object.insert("runRef".to_string(), json!(run_ref));
        object.insert("nextActions".to_string(), compiled);
        if !dispatch {
            object.insert(
                "guidance".to_string(),
                json!("Compiled only (--no-dispatch). Inspect `nextActions`, relay each one through its `snowApp` plan, then call allinluna-receipt with the observed receipt before dispatching the lane."),
            );
        }
        Ok(Value::Object(object))
    }

    fn status(&self, args: &Value) -> napi::Result<Value> {
        let run_id = required_str(args, "runId")?;
        let mut tail = vec!["status".to_string()];
        if args
            .get("invariants")
            .and_then(Value::as_bool)
            .unwrap_or(true)
        {
            tail.push("--invariants".to_string());
        }
        tail.push(run_id.to_string());
        invoke(args, &tail)
    }

    fn actions(&self, args: &Value) -> napi::Result<Value> {
        let run_id = required_str(args, "runId")?;
        let mode = args
            .get("mode")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| matches!(*value, "preview" | "dispatch"))
            .unwrap_or("preview");
        // Dispatch must run inside the persistent host session: the scheduler
        // writes the durable outbox intent that a Lane later reopens, and the
        // bridge keeps the in-flight queue in the same process.
        let result = if mode == "dispatch" {
            session_call(args, "dispatch", json!({ "runId": run_id, "commit": true }))?
        } else {
            invoke(args, &["preview-actions".to_string(), run_id.to_string()])?
        };
        let actions = result.get("result").and_then(|value| value.get("actions"));
        let decorated = decorate_actions(actions);
        let mut object = result.as_object().cloned().unwrap_or_default();
        object.insert("mode".to_string(), json!(mode));
        object.insert("relayPlan".to_string(), decorated);
        object.insert(
            "contract".to_string(),
            json!("A returned action is not completion. Execute the mapped Snow App tool, then ingest the observed receipt with allinluna-receipt."),
        );
        Ok(Value::Object(object))
    }

    fn drive(&self, args: &Value) -> napi::Result<Value> {
        let run_id = required_str(args, "runId")?;
        let max_cycles = args
            .get("maxCycles")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .clamp(1, 64);
        let monitor = args
            .get("monitor")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        session_call(
            args,
            "drive",
            json!({ "runId": run_id, "maxCycles": max_cycles, "monitor": monitor }),
        )
    }

    fn receipt(&self, args: &Value) -> napi::Result<Value> {
        let run_id = required_str(args, "runId")?;
        let mut receipt = args.get("receipt").cloned().ok_or_else(|| {
            Error::new(
                Status::InvalidArg,
                "Missing `receipt`: pass the host-receipt/v1 object you actually observed.",
            )
        })?;
        // The conversation id observed from `app-control-conversation` is the
        // thread handle; it may be supplied on its own for convenience.
        if let Some(thread_id) = args
            .get("threadId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if let Some(object) = receipt.as_object_mut() {
                object
                    .entry("thread_id")
                    .or_insert_with(|| json!(thread_id));
            }
        }
        session_call(
            args,
            "ingest_receipt",
            json!({ "runId": run_id, "receipt": receipt }),
        )
    }

    /// Report the conversation that actually serviced a dispatched top-level
    /// action. The runtime opcode stays the contract identity; the physical
    /// Snow App call is recorded in the receipt payload.
    fn relay(&self, args: &Value) -> napi::Result<Value> {
        let run_id = required_str(args, "runId")?;
        let thread_id = args
            .get("threadId")
            .or_else(|| args.get("conversationId"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                Error::new(
                    Status::InvalidArg,
                    "Missing `threadId`: pass the conversationId returned by `app-control-conversation`.",
                )
            })?;
        session_call(
            args,
            "relay",
            json!({
                "runId": run_id,
                "threadId": thread_id,
                "taskId": text_arg(args, "taskId"),
                "physicalTool": text_arg(args, "physicalTool")
            }),
        )
    }

    /// Read-only compilation: `compile` returns the full compiled graph, `plan`
    /// returns only the ready tasks. Neither persists a Run or touches a Store.
    fn plan(&self, args: &Value) -> napi::Result<Value> {
        let goal = required_str(args, "goal")?;
        let mode = args
            .get("mode")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| matches!(*value, "compile" | "plan"))
            .unwrap_or("plan");
        let mut tail = vec![mode.to_string(), "--goal".to_string(), goal.to_string()];
        if let Some(items) = args.get("doneWhen").and_then(Value::as_array) {
            for item in items {
                if let Some(text) = item.as_str().map(str::trim).filter(|value| !value.is_empty()) {
                    tail.push("--done-when".to_string());
                    tail.push(text.to_string());
                }
            }
        }
        let pack = text_arg(args, "pack");
        if !pack.is_empty() {
            tail.push("--pack".to_string());
            tail.push(pack);
        }
        let model = resolve_model(args);
        if !model.is_empty() {
            tail.push("--model".to_string());
            tail.push(model);
        }
        let reasoning = text_arg(args, "reasoning");
        if !reasoning.is_empty() {
            tail.push("--reasoning".to_string());
            tail.push(reasoning);
        }
        let policy = text_arg(args, "externalActionPolicy");
        if matches!(policy.as_str(), "ask" | "deny" | "allow") {
            tail.push("--external-action-policy".to_string());
            tail.push(policy);
        }
        let root = text_arg(args, "repositoryRoot");
        if !root.is_empty() {
            tail.push("--repository-root".to_string());
            tail.push(root);
        }
        invoke_with_db(args, &tail, false)
    }

    /// Run-level lifecycle commands: pause / resume / reconcile / retry / cancel /
    /// next-actions / inspect.
    fn run_admin(&self, args: &Value) -> napi::Result<Value> {
        let run_id = required_str(args, "runId")?;
        let operation = text_arg(args, "operation");
        let mut params = json!({
            "runId": run_id,
            "operation": if operation.is_empty() { "inspect".to_string() } else { operation }
        });
        if let Some(object) = params.as_object_mut() {
            for key in ["taskId", "kind", "identity", "contractDelta"] {
                if let Some(value) = args.get(key) {
                    object.insert(key.to_string(), value.clone());
                }
            }
        }
        session_call(args, "run_admin", params)
    }

    fn policy(&self, args: &Value) -> napi::Result<Value> {
        let run_id = required_str(args, "runId")?;
        let policy = args.get("policy").cloned().ok_or_else(|| {
            Error::new(
                Status::InvalidArg,
                "Missing `policy`: pass the policy object to write.",
            )
        })?;
        let mut params = json!({
            "runId": run_id,
            "operation": text_arg(args, "operation"),
            "policy": policy
        });
        if let Some(object) = params.as_object_mut() {
            if let Some(value) = args.get("taskId") {
                object.insert("taskId".to_string(), value.clone());
            }
        }
        session_call(args, "policy", params)
    }

    fn permission(&self, args: &Value) -> napi::Result<Value> {
        let run_id = required_str(args, "runId")?;
        let mut params = json!({ "runId": run_id, "operation": text_arg(args, "operation") });
        if let Some(object) = params.as_object_mut() {
            for key in [
                "scopeType",
                "scopeId",
                "action",
                "permissionId",
                "allow",
                "rationale",
            ] {
                if let Some(value) = args.get(key) {
                    object.insert(key.to_string(), value.clone());
                }
            }
        }
        session_call(args, "permission", params)
    }

    /// Report the sub-agent conversation that actually serviced one spawned
    /// WorkUnit. The runtime keeps the unit delegated until this arrives.
    fn subagent(&self, args: &Value) -> napi::Result<Value> {
        let run_id = required_str(args, "runId")?;
        let task_id = required_str(args, "taskId")?;
        let work_unit_id = required_str(args, "workUnitId")?;
        let thread_id = args
            .get("threadId")
            .or_else(|| args.get("subagentId"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                Error::new(
                    Status::InvalidArg,
                    "Missing `threadId`: pass the sub-agent conversation id you observed.",
                )
            })?;
        let mut params = json!({
            "runId": run_id,
            "taskId": task_id,
            "workUnitId": work_unit_id,
            "threadId": thread_id
        });
        if let Some(object) = params.as_object_mut() {
            for key in ["status", "summary", "physicalTool", "handoff"] {
                if let Some(value) = args.get(key) {
                    object.insert(key.to_string(), value.clone());
                }
            }
        }
        session_call(args, "subagent_report", params)
    }

    fn lane(&self, args: &Value) -> napi::Result<Value> {
        let run_id = required_str(args, "runId")?;
        let task_id = required_str(args, "taskId")?;
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| {
                matches!(
                    *value,
                    "start"
                        | "status"
                        | "tick"
                        | "drive"
                        | "handoff"
                        | "finalize"
                        | "next-actions"
                        | "ingest-receipt"
                        | "ingest-direct-result"
                        | "ingest-handoff"
                        | "complete-work-unit"
                        | "finalize-lane"
                )
            })
            .unwrap_or("status");
        let monitor = args
            .get("monitor")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let max_cycles = args
            .get("maxCycles")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .clamp(1, 64);
        let mut params = json!({
            "runId": run_id,
            "taskId": task_id,
            "action": action,
            "monitor": monitor,
            "maxCycles": max_cycles
        });
        if let Some(object) = params.as_object_mut() {
            for key in ["receipt", "result", "handoff"] {
                if let Some(value) = args.get(key) {
                    object.insert(key.to_string(), value.clone());
                }
            }
        }
        let result = session_call(args, "lane", params)?;
        let actions = result
            .get("result")
            .and_then(|value| value.get("actions"))
            .or_else(|| result.get("result").and_then(|value| value.get("next_actions")));
        let decorated = decorate_actions(actions);
        let direct_plans = result
            .get("result")
            .and_then(|value| value.get("boundary"))
            .and_then(|value| value.get("plans"))
            .cloned()
            .unwrap_or_else(|| json!([]));
        let mut object = result.as_object().cloned().unwrap_or_default();
        object.insert("laneAction".to_string(), json!(action));
        object.insert("relayPlan".to_string(), decorated);
        object.insert("directPlans".to_string(), direct_plans);
        Ok(Value::Object(object))
    }

    fn capabilities(&self) -> napi::Result<Value> {
        invoke_stateless(&["capabilities".to_string()])
    }

    fn doctor(&self, args: &Value) -> napi::Result<Value> {
        let run_id = required_str(args, "runId")?;
        let tail = vec!["doctor".to_string(), run_id.to_string()];
        invoke(args, &tail)
    }
}

fn db_hint_schema() -> Value {
    json!({
        "dbPath": {
            "type": "string",
            "description": "Absolute Runtime DB path. Omit to use <repositoryRoot>/.allinluna/runtime.db."
        },
        "repositoryRoot": {
            "type": "string",
            "description": "Absolute repository root used to derive the default runtime DB path and bind the host session's project identity."
        },
        "projectId": {
            "type": "string",
            "description": "Optional stable project id reported by the host session (defaults to `local:<repositoryRoot>`). Only used when the runtime has not persisted a project resolution yet."
        },
        "branch": {
            "type": "string",
            "description": "Optional branch reported with the project environment (defaults to none)."
        },
        "model": {
            "type": "string",
            "description": "Concrete model for this run/lane. Omit to use the model of the active API profile; the runtime never invents one, so an unresolved model blocks every top-level action."
        },
        "reasoning": {
            "type": "string",
            "description": "Optional reasoning level recorded with the route (omitted by default)."
        }
    })
}

fn merge_schema(base: Value, extra: Value) -> Value {
    let mut properties = base.as_object().cloned().unwrap_or_default();
    if let Some(map) = extra.as_object() {
        for (key, value) in map {
            properties.insert(key.clone(), value.clone());
        }
    }
    json!({ "properties": properties })
}

impl McpService for AllInLunaService {
    fn id(&self) -> &str {
        SERVER_ID
    }

    fn tools(&self) -> Vec<McpTool> {
        let common = db_hint_schema();
        let repository_required = json!({
            "goal": {
                "type": "string",
                "description": "One-sentence goal. All in Luna compiles it into typed Tasks with contracts, dependencies, ownership, done-when conditions and verification specs."
            },
            "repositoryRoot": {
                "type": "string",
                "description": "Absolute repository root the run is bound to, e.g. H:/github/snow-app."
            },
            "doneWhen": {
                "type": "string",
                "description": "Optional explicit completion condition appended to the contract."
            },
            "pack": {
                "type": "string",
                "description": "Workflow pack: delivery (default) | gsd | research-routes-bridge."
            },
            "externalActionPolicy": {
                "type": "string",
                "enum": ["ask", "deny", "allow"],
                "description": "Just-in-time permission policy for push/deploy/publish (default: ask)."
            },
            "dispatch": {
                "type": "boolean",
                "description": "false (default) compiles and persists only (--no-dispatch). true also releases the first Lane."
            },
            "projectless": {
                "type": "boolean",
                "description": "Bind the run to a plain local checkout instead of a worktree (CLI --projectless). Required for ordinary repositories: project resolution otherwise only accepts a worktree environment and the run stalls on resolve-project."
            },
            "taskGraph": {
                "type": ["object", "string"],
                "description": "Optional explicit task graph. Accepts a canonical TaskGraph object or a compact outcome-task list ({tasks:[{id,outcome,ownership:{paths},done_when,checks,dependencies,exports,priority}]}). When supplied the runtime uses it verbatim instead of auto-decomposing the goal, letting you assign per-Lane ownership.paths so parallel sub-agents never overwrite each other. Pass as an object or a JSON string."
            },
            "model": {
                "type": "string",
                "description": "Concrete model persisted on the run. Omit to use the model of the active API profile. Without it the runtime refuses to emit an executable top-level action."
            },
            "reasoning": {
                "type": "string",
                "description": "Optional reasoning level persisted on the run."
            },
            "timeoutSeconds": {
                "type": "number",
                "description": "Hard timeout for the runtime call (default 180, max 1800)."
            }
        });

        vec![
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "allinluna-start".to_string(),
                description: "Compile a goal, existing plan or active run into a typed TaskGraph with contracts, dependencies, ownership, done-when conditions and verification specs, then persist it in one absolute Runtime DB. Defaults to compile-only (--no-dispatch) so nothing executes until you relay the emitted actions. Returns `nextActions`, each already decorated with a `snowApp` relay plan telling you which Snow App tool to call. Use this for large multi-part work (frontend + backend + tests + docs); for a one-line edit use the normal agent tools instead.".to_string(),
                input_schema: {
                    let merged = merge_schema(json!({ "properties": {} }), repository_required);
                    let mut properties = merged.get("properties").cloned().unwrap_or_else(|| json!({}));
                    if let (Some(map), Some(extra)) = (properties.as_object_mut(), common.as_object()) {
                        if let Some(entry) = extra.get("dbPath") {
                            map.insert("dbPath".to_string(), entry.clone());
                        }
                    }
                    json!({
                        "type": "object",
                        "properties": properties,
                        "required": ["goal", "repositoryRoot"]
                    })
                },
            },
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "allinluna-status".to_string(),
                description: "Read one Run's persisted state (tasks, blockers, metrics, resource policy) plus read-only completion invariant diagnostics. This is Store truth, not a UI summary.".to_string(),
                input_schema: {
                    let merged = merge_schema(common.clone(), json!({
                        "runId": { "type": "string", "description": "Run id returned by allinluna-start (e.g. run-my-goal...)." },
                        "invariants": { "type": "boolean", "description": "Include completion-invariant diagnostics (default true)." }
                    }));
                    json!({
                        "type": "object",
                        "properties": merged.get("properties").cloned().unwrap_or_else(|| json!({})),
                        "required": ["runId"]
                    })
                },
            },
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "allinluna-actions".to_string(),
                description: "Return the run's dependency-ready actions. mode=preview is a non-relayable summary; mode=dispatch returns persisted Coordinator actions (the default is preview because dispatch mutates state). Every action is decorated with a `snowApp` block naming the exact Snow App tool, its arguments and the receipt fields you must report back. Never relay an action that has no snowApp mapping.".to_string(),
                input_schema: {
                    let merged = merge_schema(common.clone(), json!({
                        "runId": { "type": "string" },
                        "mode": { "type": "string", "enum": ["preview", "dispatch"], "description": "preview (default, read-only) or dispatch (persists actions)." }
                    }));
                    json!({
                        "type": "object",
                        "properties": merged.get("properties").cloned().unwrap_or_else(|| json!({})),
                        "required": ["runId"]
                    })
                },
            },
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "allinluna-receipt".to_string(),
                description: "Ingest the host receipt you actually observed for one relayed action. Pass the receipt object verbatim: `actual_tool`, `actual_capability` and `action_contract_hash` must reflect what really ran and are never inferred. A pending or self-authored result is not completion — omit it or report it as unresolved instead of fabricating success.".to_string(),
                input_schema: {
                    let merged = merge_schema(common.clone(), json!({
                        "runId": { "type": "string" },
                        "receipt": { "type": "object", "description": "host-receipt/v1 object as observed by the host." },
                        "threadId": { "type": "string", "description": "Optional conversation/thread id observed from `app-control-conversation`; merged into the receipt as `thread_id` when the receipt does not already carry one." }
                    }));
                    json!({
                        "type": "object",
                        "properties": merged.get("properties").cloned().unwrap_or_else(|| json!({})),
                        "required": ["runId", "receipt"]
                    })
                },
            },
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "allinluna-relay".to_string(),
                description: "Report the Snow App conversation that actually serviced a dispatched top-level action. Call this after relaying `codex_app__create_thread` through `app-control-conversation`: pass the returned conversationId as `threadId`. The bridge records the runtime opcode as the contract identity and the physical tool in the receipt payload; it never invents an execution result.".to_string(),
                input_schema: {
                    let merged = merge_schema(common.clone(), json!({
                        "runId": { "type": "string" },
                        "threadId": { "type": "string", "description": "conversationId returned by `app-control-conversation` (action=create)." },
                        "conversationId": { "type": "string", "description": "Alias of `threadId`." },
                        "taskId": { "type": "string", "description": "Optional task id when the Run has more than one dispatched lane." },
                        "physicalTool": { "type": "string", "description": "Physical tool that created the conversation (default: app-control-conversation)." }
                    }));
                    json!({
                        "type": "object",
                        "properties": merged.get("properties").cloned().unwrap_or_else(|| json!({})),
                        "required": ["runId", "threadId"]
                    })
                },
            },
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "allinluna-plan".to_string(),
                description: "Read-only compilation: `compile` returns the full typed task graph, `plan` returns only the ready tasks. Neither persists a Run or touches the Runtime DB, so use allinluna-start when you want the graph bound and dispatchable.".to_string(),
                input_schema: {
                    let merged = merge_schema(common.clone(), json!({
                        "goal": { "type": "string", "description": "One-sentence goal to decompose into outcome domains." },
                        "doneWhen": { "type": "array", "items": { "type": "string" }, "description": "Completion conditions appended to the contract." },
                        "pack": { "type": "string", "description": "Workflow pack: delivery (default) | gsd | research-routes-bridge." },
                        "mode": { "type": "string", "enum": ["compile", "plan"], "description": "compile = full graph; plan = ready tasks only (default)." },
                        "externalActionPolicy": { "type": "string", "enum": ["ask", "deny", "allow"] }
                    }));
                    json!({
                        "type": "object",
                        "properties": merged.get("properties").cloned().unwrap_or_else(|| json!({})),
                        "required": ["goal"]
                    })
                },
            },
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "allinluna-run".to_string(),
                description: "Run-level lifecycle and inspection: pause / resume / reconcile / retry / cancel / next-actions / inspect (run, task, work, context, outbox, receipt, artifacts). This is the CLI surface, executed inside the persistent host session so it sees the same runtime state.".to_string(),
                input_schema: {
                    let merged = merge_schema(common.clone(), json!({
                        "runId": { "type": "string" },
                        "operation": { "type": "string", "enum": ["pause", "resume", "reconcile", "retry", "cancel", "next-actions", "inspect"], "description": "Lifecycle operation (default: inspect)." },
                        "taskId": { "type": "string", "description": "Target task for retry/cancel." },
                        "contractDelta": { "type": "object", "description": "For retry: explicit forward-only contract correction." },
                        "kind": { "type": "string", "enum": ["run", "task", "work", "context", "outbox", "receipt", "artifacts"], "description": "For inspect: what to read." },
                        "identity": { "type": "string", "description": "For inspect: the id to read (defaults to runId)." }
                    }));
                    json!({
                        "type": "object",
                        "properties": merged.get("properties").cloned().unwrap_or_else(|| json!({})),
                        "required": ["runId"]
                    })
                },
            },
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "allinluna-policy".to_string(),
                description: "Write the Run policy or one Task policy (resource envelope, route assurance, external action policy). Omitted fields keep their stored values.".to_string(),
                input_schema: {
                    let merged = merge_schema(common.clone(), json!({
                        "runId": { "type": "string" },
                        "operation": { "type": "string", "enum": ["set", "set-task"], "description": "set = run policy; set-task = task policy (requires taskId)." },
                        "taskId": { "type": "string", "description": "Target task for set-task." },
                        "policy": { "type": "object", "description": "Policy object to write." }
                    }));
                    json!({
                        "type": "object",
                        "properties": merged.get("properties").cloned().unwrap_or_else(|| json!({})),
                        "required": ["runId", "operation", "policy"]
                    })
                },
            },
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "allinluna-permission".to_string(),
                description: "Request or decide a just-in-time permission for an external action (push/deploy/publish). Decisions are scoped to the Run and never fabricated.".to_string(),
                input_schema: {
                    let merged = merge_schema(common.clone(), json!({
                        "runId": { "type": "string" },
                        "operation": { "type": "string", "enum": ["request", "decide"] },
                        "scopeType": { "type": "string", "enum": ["run", "task", "work_unit"] },
                        "scopeId": { "type": "string" },
                        "action": { "type": "string", "description": "For request: the external action name." },
                        "permissionId": { "type": "string", "description": "For decide: the permission intent id." },
                        "allow": { "type": "boolean", "description": "For decide: true to allow." },
                        "rationale": { "type": "string" }
                    }));
                    json!({
                        "type": "object",
                        "properties": merged.get("properties").cloned().unwrap_or_else(|| json!({})),
                        "required": ["runId", "operation"]
                    })
                },
            },
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "allinluna-subagent".to_string(),
                description: "Report the sub-agent conversation that actually serviced one spawned WorkUnit (from a `spawn-subagent` action). The runtime keeps the unit delegated until this receipt arrives; pass `status` completed only with real evidence, and pass `handoff` for the WorkHandoff.".to_string(),
                input_schema: {
                    let merged = merge_schema(common.clone(), json!({
                        "runId": { "type": "string" },
                        "taskId": { "type": "string" },
                        "workUnitId": { "type": "string", "description": "From the spawn action payload." },
                        "threadId": { "type": "string", "description": "Sub-agent conversation id you observed." },
                        "subagentId": { "type": "string", "description": "Alias of threadId." },
                        "status": { "type": "string", "enum": ["active", "completed", "failed", "handoff_ready"], "description": "Observed state (default active)." },
                        "summary": { "type": "string" },
                        "handoff": { "type": "object", "description": "Optional work-handoff/v1 document." }
                    }));
                    json!({
                        "type": "object",
                        "properties": merged.get("properties").cloned().unwrap_or_else(|| json!({})),
                        "required": ["runId", "taskId", "workUnitId", "threadId"]
                    })
                },
            },
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "allinluna-drive".to_string(),
                description: "Run the persistent Coordinator loop for one cycle (or up to maxCycles): schedule ready actions, ingest receipts, reconcile, and release newly-ready tasks. Returns the next relay plan.".to_string(),
                input_schema: {
                    let merged = merge_schema(common.clone(), json!({
                        "runId": { "type": "string" },
                        "maxCycles": { "type": "number", "description": "Cycle budget 1-64." },
                        "monitor": { "type": "boolean", "description": "Wait on child threads (default false = --no-monitor)." }
                    }));
                    json!({
                        "type": "object",
                        "properties": merged.get("properties").cloned().unwrap_or_else(|| json!({})),
                        "required": ["runId"]
                    })
                },
            },
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "allinluna-lane".to_string(),
                description: "Operate one Task Lane: start/status/tick/drive/handoff/finalize/next-actions. A Lane owns only its local WorkGraph, context slice and handoff — it never changes another lane's contract or the global graph. Use finalize to synthesize the canonical handoff from persisted work-unit results.".to_string(),
                input_schema: {
                    let merged = merge_schema(common.clone(), json!({
                        "runId": { "type": "string" },
                        "taskId": { "type": "string", "description": "Task id, e.g. <runId>:task:deliver." },
                        "action": {
                            "type": "string",
                            "enum": ["start", "status", "tick", "drive", "handoff", "finalize", "next-actions", "ingest-receipt", "ingest-direct-result", "ingest-handoff", "complete-work-unit", "finalize-lane"],
                            "description": "Lane subcommand (default: status). When a drive/tick returns LANE_DIRECT_EXECUTION_REQUIRED, execute the bounded plan in `directPlans` and report it back with ingest-direct-result."
                        },
                        "receipt": { "type": "object", "description": "For ingest-receipt: the lane/work receipt you actually observed." },
                        "result": { "type": "object", "description": "For ingest-direct-result: a direct-work-result/v1 object. Copy work_unit_id, attempt_id, idempotency_key and plan_digest verbatim from the plan in `directPlans`, then add the observed status/summary/changed_paths/raw_outputs. A result without check evidence becomes `blocked`, never completed." },
                        "handoff": { "type": "object", "description": "For ingest-handoff: a work-handoff/v1 or lane-handoff/v1 document." }
                    }));
                    json!({
                        "type": "object",
                        "properties": merged.get("properties").cloned().unwrap_or_else(|| json!({})),
                        "required": ["runId", "taskId"]
                    })
                },
            },
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "allinluna-doctor".to_string(),
                description: "Read-only diagnostic for one Run: surfaces completion-state contradictions (for example a task claiming done without a verified receipt). Use it before reporting a run as finished.".to_string(),
                input_schema: {
                    let merged = merge_schema(common.clone(), json!({
                        "runId": { "type": "string" }
                    }));
                    json!({
                        "type": "object",
                        "properties": merged.get("properties").cloned().unwrap_or_else(|| json!({})),
                        "required": ["runId"]
                    })
                },
            },
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "allinluna-capabilities".to_string(),
                description: "Show the runtime's valid capability classes, route-assurance modes, external-action policies and installed Workflow Packs.".to_string(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
        ]
    }

    fn execute(&self, tool_name: &str, args: &Value) -> napi::Result<Value> {
        match tool_name {
            "allinluna-start" => self.start(args),
            "allinluna-status" => self.status(args),
            "allinluna-actions" => self.actions(args),
            "allinluna-drive" => self.drive(args),
            "allinluna-receipt" => self.receipt(args),
            "allinluna-relay" => self.relay(args),
            "allinluna-run" => self.run_admin(args),
            "allinluna-plan" => self.plan(args),
            "allinluna-policy" => self.policy(args),
            "allinluna-permission" => self.permission(args),
            "allinluna-subagent" => self.subagent(args),
            "allinluna-lane" => self.lane(args),
            "allinluna-doctor" => self.doctor(args),
            "allinluna-capabilities" => self.capabilities(),
            other => Err(Error::new(
                Status::InvalidArg,
                format!("Unknown tool: \"{other}\" for server \"{SERVER_ID}\""),
            )),
        }
    }
}
