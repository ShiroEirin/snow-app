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

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
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
            "note": "Upstream forbids falling back to a sub-agent for a top-level task. Create a real conversation; the returned conversationId is the thread handle for later wait/read actions."
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
        let subcommand = if mode == "dispatch" {
            "dispatch"
        } else {
            "preview-actions"
        };
        let tail = vec![subcommand.to_string(), run_id.to_string()];
        let result = invoke(args, &tail)?;
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
        let mut tail = vec!["drive".to_string()];
        if let Some(cycles) = args.get("maxCycles").and_then(Value::as_u64) {
            tail.push("--max-cycles".to_string());
            tail.push(cycles.clamp(1, 64).to_string());
        }
        if !args
            .get("monitor")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            tail.push("--no-monitor".to_string());
        }
        tail.push(run_id.to_string());
        invoke(args, &tail)
    }

    fn receipt(&self, args: &Value) -> napi::Result<Value> {
        let run_id = required_str(args, "runId")?;
        let receipt = args.get("receipt").ok_or_else(|| {
            Error::new(
                Status::InvalidArg,
                "Missing `receipt`: pass the host-receipt/v1 object you actually observed.",
            )
        })?;
        let encoded = serde_json::to_string(receipt).map_err(|error| {
            Error::new(
                Status::GenericFailure,
                format!("Failed to serialize the receipt: {error}"),
            )
        })?;
        let tail = vec!["ingest-receipt".to_string(), run_id.to_string(), encoded];
        invoke(args, &tail)
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
                    "start" | "status" | "tick" | "drive" | "handoff" | "finalize" | "next-actions"
                )
            })
            .unwrap_or("status");
        let tail = vec![
            "lane".to_string(),
            action.to_string(),
            run_id.to_string(),
            task_id.to_string(),
        ];
        let result = invoke(args, &tail)?;
        let actions = result
            .get("result")
            .and_then(|value| value.get("actions"))
            .or_else(|| result.get("result").and_then(|value| value.get("next_actions")));
        let decorated = decorate_actions(actions);
        let mut object = result.as_object().cloned().unwrap_or_default();
        object.insert("laneAction".to_string(), json!(action));
        object.insert("relayPlan".to_string(), decorated);
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
            "description": "Absolute repository root used to derive the default runtime DB path."
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
                        "receipt": { "type": "object", "description": "host-receipt/v1 object as observed by the host." }
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
                            "enum": ["start", "status", "tick", "drive", "handoff", "finalize", "next-actions"],
                            "description": "Lane subcommand (default: status)."
                        }
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
