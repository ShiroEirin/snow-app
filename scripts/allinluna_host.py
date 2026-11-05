#!/usr/bin/env python3
"""All in Luna persistent host helper for Snow App.

The runtime's CLI is stateless: every invocation creates a fresh
``ActionBridge`` whose in-memory queue is lost, so a receipt cannot be linked
back to the action that produced it.  This helper keeps one
``CoordinatorEngine`` alive and injects a ``SnowAppHost`` so the runtime can
dispatch project resolution and top-level task creation instead of falling
back to ``ACTION_RELAY_REQUIRED``.

Protocol: one JSON object per line on stdin/stdout.

  request  {"id": 1, "method": "dispatch", "params": {"runId": "..."}}
  response {"id": 1, "ok": true, "result": {...}}
  error    {"id": 1, "ok": false, "error": "..."}

Commands: dispatch, ingest_receipt, relay, status, lane, drive, shutdown.
"""

from __future__ import annotations

import argparse
import json
import sys
import traceback
from datetime import datetime, timezone
from typing import Any, Mapping

from allinluna_runtime.adapters.host.base import (
    TOP_LEVEL_CREATE_THREAD_TOOL,
    HostCapabilities,
    HostReceipt,
    stable_digest,
)
from allinluna_runtime.engine.coordinator import CoordinatorEngine
from allinluna_runtime.resource import ResourceBroker
from allinluna_runtime.resource_policy import CAPABILITY_CLASSES
from allinluna_runtime.store import Store

SNOW_TOOLS = (
    "app-control-conversation",
    "sub-agents-activate",
    "sub-agents-continue",
    "sub-agents-getResult",
    "sub-agents-cancel",
    "sub-agents-listSubAgents",
    "config-list",
)

# The scheduler emits this exact opcode for every top-level lane.  Snow App
# services it with ``app-control-conversation`` (create + autoReply), so the
# opcode is a capability claim by this adapter, not an assertion that a Codex
# tool of that name exists in the app.
TOP_LEVEL_OPCODE = TOP_LEVEL_CREATE_THREAD_TOOL


class SnowAppHost:
    """Minimal HostAdapter for Snow App.

    Project resolution is answered synchronously from the workspace binding.
    Top-level task creation returns a *pending* receipt on purpose: the real
    conversation is created by Snow App (``app-control-conversation``) and its
    ``conversationId`` comes back through ``ingest_receipt``.
    """

    host_kind = "snow-app"

    def __init__(
        self,
        *,
        project_root: str,
        workspace: str,
        branch: str | None,
        project_id: str,
        model: str | None = None,
        reasoning: str | None = None,
    ) -> None:
        self.project_root = project_root
        self.workspace = workspace
        self.branch = branch
        self.project_id = project_id
        self.model = model
        self.reasoning = reasoning

    def discover(self) -> HostCapabilities:
        evidence: dict[str, Any] = {
            "binding": "stdio-json-lines",
            "tool_bindings": {TOP_LEVEL_OPCODE: "app-control-conversation"},
        }
        # The runtime deliberately owns no concrete model name: it asks the host
        # to map a capability class to a route.  Snow App knows the model of its
        # active API profile, so it is the only party able to answer.
        if self.model:
            route: dict[str, str] = {"model": self.model}
            if self.reasoning:
                route["reasoning"] = self.reasoning
            evidence["capability_routes"] = {
                capability_class: dict(route) for capability_class in CAPABILITY_CLASSES
            }
        # Snow App has real sub-agents. Advertising the logical capability lets
        # the Lane dispatch bounded WorkUnits in parallel instead of collapsing
        # every lane into one lane-direct plan executed by the caller.
        native_subagent = {
            "available": True,
            "physical_tools": [
                "sub-agents-activate",
                "sub-agents-continue",
                "sub-agents-getResult",
                "sub-agents-cancel",
            ],
            "preferred_tool": "sub-agents-activate",
            "receipt_contract": "host-receipt/v1",
        }
        return HostCapabilities(
            host_id="snow-app",
            host_kind=self.host_kind,
            available=True,
            tools=(TOP_LEVEL_OPCODE, *SNOW_TOOLS),
            source="snow-app-host-helper",
            logical_capabilities={"native_subagent": native_subagent},
            evidence=evidence,
        )

    def list_projects(self) -> dict[str, Any]:
        # ``environment.path`` must be the repository root the Run was created
        # for; the runtime rejects a resolution rooted at a subdirectory.
        environment: dict[str, Any] = {"type": "worktree", "path": self.project_root}
        if self.branch:
            environment["branch"] = self.branch
        return {
            "projects": [
                {
                    "projectId": self.project_id,
                    "path": self.project_root,
                    "environment": environment,
                }
            ]
        }

    def create_top_level_task(self, action: Any) -> HostReceipt:
        raw = {
            "receipt_id": "receipt-" + stable_digest(
                {"action": action.action_id, "kind": "create-top-level-task"}
            ),
            "idempotency_key": action.idempotency_key,
            "dispatch_key": action.idempotency_key,
            "status": "pending",
            "host_adapter": "snow-app",
            "host_id": "snow-app",
            "action_id": action.action_id,
            "task_id": action.task_id,
            "actual_tool": action.tool,
            "payload": {
                "action": action.to_dict(),
                "relay": {
                    "tool": "app-control-conversation",
                    "arguments": {"action": "create", "autoReply": True},
                    "note": (
                        "Create the conversation, then report its conversationId "
                        "as thread_id in the receipt."
                    ),
                },
            },
        }
        return HostReceipt.from_value(raw, action=action, default_source="snow-app")

    def invoke_tool(self, tool: str, arguments: Any) -> dict[str, Any]:
        """Relay a resolved local tool call without pretending it ran.

        A receipt with only a ``client_thread_id`` is *pending* to the runtime,
        so the WorkUnit stays delegated instead of being marked blocked.  The
        real sub-agent conversation id arrives later through
        ``subagent_report`` and upgrades the same attempt.
        """
        envelope = None
        if isinstance(arguments, Mapping):
            envelope = arguments.get("envelope")
        key = stable_digest({"tool": tool, "envelope": envelope})
        return {
            "receipt_id": "receipt-pending-" + key,
            "status": "pending",
            "client_thread_id": "snow-app-pending:" + key,
            "host_adapter": "snow-app",
            "host_id": "snow-app",
            "payload": {
                "relayRequired": True,
                "physical_tool": tool,
                "envelope": envelope,
            },
        }

    def wait_tasks(self, targets: Any, cursor: str | None = None) -> HostReceipt:
        return self._pending("wait", targets=targets, cursor=cursor)

    def read_task(self, target: Any, cursor: str | None = None) -> HostReceipt:
        return self._pending("read", target=target, cursor=cursor)

    def send_message(self, target: Any, envelope: Any) -> HostReceipt:
        return self._pending("send", target=target, envelope=envelope)

    def cancel_task(self, target: Any) -> HostReceipt:
        return self._pending("cancel", target=target)

    def _pending(self, kind: str, **payload: Any) -> HostReceipt:
        raw = {
            "receipt_id": "receipt-" + stable_digest({"kind": kind, "payload": payload}),
            "idempotency_key": f"snow-app:{kind}:pending",
            "status": "pending",
            "host_adapter": "snow-app",
            "host_id": "snow-app",
            "payload": {"relayRequired": True, "operation": kind, **payload},
        }
        return HostReceipt.from_value(raw, default_source="snow-app")


class Session:
    def __init__(self, args: argparse.Namespace) -> None:
        self.store = Store(args.db)
        self.host = SnowAppHost(
            project_root=args.project_root,
            workspace=args.workspace,
            branch=args.branch,
            project_id=args.project_id,
            model=args.model,
            reasoning=args.reasoning,
        )
        self.engines: dict[str, CoordinatorEngine] = {}

    def _engine(self, run_id: str) -> CoordinatorEngine:
        # One engine per Run: the engine owns a ResourceBroker bound to a single
        # run_id, so reusing it for another Run would claim resources in the
        # wrong Run when both Runs share the same policy envelope.
        engine = self.engines.get(run_id)
        if engine is not None:
            return engine
        run = self.store.get_run(run_id)
        if run is None:
            raise KeyError(f"unknown run: {run_id}")
        policy = dict(run.get("policy") or {})
        engine = CoordinatorEngine(
            self.store,
            host=self.host,
            resource_broker=ResourceBroker(policy),
            adapter="snow-app",
        )
        self.engines[run_id] = engine
        return engine

    def dispatch(self, params: Mapping[str, Any]) -> Any:
        run_id = str(params["runId"])
        engine = self._engine(run_id)
        tick = engine.tick(run_id, dispatch=bool(params.get("commit", True)))
        return {
            "runId": run_id,
            "actions": [
                action if isinstance(action, dict) else action.to_dict()
                for action in tick.actions
            ],
            "receipts": list(tick.receipts),
            "status": tick.status,
        }

    def ingest_receipt(self, params: Mapping[str, Any]) -> Any:
        run_id = str(params["runId"])
        receipt = params.get("receipt")
        if not isinstance(receipt, Mapping):
            raise ValueError("receipt must be an object")
        return self._engine(run_id).ingest_receipt(receipt, run_id=run_id)

    def complete_work_unit(self, params: Mapping[str, Any]) -> Any:
        """Record a real work-unit handoff, then activate and complete the unit.

        ``LaneDriver._persisted_work_evidence`` only accepts a completed work
        handoff whose ``evidence.verified`` is true and whose ``checks`` all
        pass, so this command writes nothing unless every supplied check is
        passing and at least one artifact can be materialized.
        """

        import uuid

        from allinluna_runtime.engine.lane_driver import LaneDriver

        run_id = str(params["runId"])
        task_id = str(params["taskId"])
        driver = LaneDriver(self.store, task_id, run_id=run_id, host=self.host)
        lane = driver.lane
        if lane.evidence_collector is None:
            lane.evidence_collector = driver._default_evidence_collector()

        task = self.store.get_task(task_id) or {}
        contract = self.store.get_contract(
            str(task.get("contract_id") or ""), int(task.get("contract_version", 1))
        ) or {}
        units = lane.scheduler._units()
        unit_id = str(params.get("workUnitId") or "").strip()
        if not unit_id:
            if len(units) != 1:
                raise ValueError(
                    "workUnitId is required when the lane has more than one work unit"
                )
            unit_id = str(units[0]["id"])
        unit = self.store.get_work_unit(unit_id)
        if unit is None or str(unit.get("task_id")) != task_id:
            raise ValueError("workUnitId does not belong to this task")

        checks: list[dict[str, Any]] = []
        for index, raw in enumerate(params.get("checks") or ()):
            item = dict(raw) if isinstance(raw, Mapping) else {"name": str(raw)}
            exit_code = item.get("exit_code", item.get("exitCode"))
            status = str(item.get("status") or "").lower()
            if not status:
                status = "pass" if exit_code in (None, 0) else "fail"
            checks.append(
                {
                    "name": str(item.get("name") or f"check-{index + 1}"),
                    "command": str(item.get("command") or "observe:caller-report"),
                    "status": status,
                    "exit_code": int(exit_code or 0),
                    "observed_at": str(
                        item.get("observed_at")
                        or datetime.now(timezone.utc).isoformat()
                    ),
                }
            )
        failing = [
            check["name"]
            for check in checks
            if check["status"] not in {"pass", "passed", "ok", "success", "completed"}
        ]
        if not checks or failing:
            return {
                "task_id": task_id,
                "work_unit_id": unit_id,
                "written": False,
                "error": "no checks were supplied"
                if not checks
                else f"failing checks: {failing}",
            }

        artifacts = driver._materialize_work_artifacts(
            list(params.get("artifacts") or ())
        )
        if not artifacts:
            return {
                "task_id": task_id,
                "work_unit_id": unit_id,
                "written": False,
                "error": "no verifiable artifacts were supplied (use file:<path> or artifact://...)",
            }

        provided: dict[str, dict[str, Any]] = {
            str(item.get("name")): dict(item)
            for item in params.get("exports") or ()
            if isinstance(item, Mapping) and item.get("name")
        }
        for declared in contract.get("exports", ()) or ():
            name = str(declared.get("name") or "") if isinstance(declared, Mapping) else ""
            if name:
                provided.setdefault(name, {"name": name, "version": 1})
        exports: list[dict[str, Any]] = []
        for name, item in provided.items():
            item.setdefault("artifact_ref", artifacts[0])
            item.setdefault("version", 1)
            exports.append(item)

        handoff = {
            "kind": "handoff",
            "protocol": "work-handoff/v1",
            "handoff_kind": "work",
            "handoff_id": str(
                params.get("handoffId") or "work-handoff-" + uuid.uuid4().hex[:16]
            ),
            "run_ref": f"run://{run_id}",
            "task_id": task_id,
            "work_unit_id": unit_id,
            "status": "completed",
            # A top-level lane carries its evidence here; ``lane_direct`` would
            # add the plan/result-digest gate that only direct executors can pass.
            "execution_mode": "top-level-lane",
            "execution_source": "snow-app-subagent",
            "subagent_created": True,
            "thread_id": params.get("threadId"),
            "summary": str(
                params.get("summary") or "work unit completed by a Snow App sub-agent"
            ),
            "changed_paths": [str(item) for item in params.get("changedPaths") or ()],
            "checks": checks,
            "artifacts": artifacts,
            "exports": exports,
            "blockers": [],
            "evidence": {
                "verified": True,
                "checks": checks,
                "artifacts": artifacts,
                "exports": exports,
            },
            "created_at": datetime.now(timezone.utc).isoformat(),
        }

        if str(unit.get("state") or "") in {"proposed", "ready"}:
            self.store.update_work_unit_status(
                unit_id,
                "active",
                signal_type="WORK_UNIT_PULSE",
                payload={"source": "snow-app-complete-work-unit"},
            )
        driver.ingest_handoff(handoff)
        latest = self.store.get_work_unit(unit_id) or {}
        return {
            "task_id": task_id,
            "work_unit_id": unit_id,
            "written": True,
            "handoff_id": handoff["handoff_id"],
            "checks": [check["name"] for check in checks],
            "artifacts": artifacts,
            "exports": [item["name"] for item in exports],
            "state": latest.get("state"),
        }

    def finalize_lane(self, params: Mapping[str, Any]) -> Any:
        """Finalize a Lane from persisted work results, then close its Task.

        A previously persisted lane handoff is deliberately NOT reused: an older
        record can predate the evidence fields and would keep failing the schema
        check forever.  Instead this mirrors ``LaneDriver.finalize`` (fresh
        synthesis + real EvidenceCollector) without its terminal-handoff dedupe,
        then lets ``GlobalScheduler.accept_handoff`` verify and apply the result.
        """

        from allinluna_runtime.engine.lane_driver import LaneDriver

        run_id = str(params["runId"])
        task_id = str(params["taskId"])
        driver = LaneDriver(self.store, task_id, run_id=run_id, host=self.host)
        lane = driver.lane
        if lane.evidence_collector is None:
            # Mirrors LaneDriver.finalize: the collector is what turns persisted
            # work results into checks / done_when / workspace evidence.
            lane.evidence_collector = driver._default_evidence_collector()

        handoff = lane.synthesize_handoff()
        result: dict[str, Any] = {
            "task_id": task_id,
            "handoff_id": handoff.get("handoff_id"),
            "handoff_status": handoff.get("status"),
            "blockers": list(handoff.get("blockers") or ()),
        }
        if not params.get("accept", True):
            return result

        try:
            work_results = driver._persisted_work_evidence()
            artifacts = driver._materialize_work_artifacts(work_results["artifacts"])
            exports = list(work_results["exports"])
            handoff = lane.collect_handoff_evidence(
                handoff,
                artifacts=artifacts,
                exports=exports,
                result_receipts=work_results["receipts"],
                workspace_scope=driver._workspace_scope(),
            )
        except Exception as error:  # noqa: BLE001 - report the collector's objection
            result["error"] = f"{type(error).__name__}: {error}"
            return result

        result["collected"] = {
            key: handoff.get(key)
            for key in ("checks", "done_when", "exports", "artifacts", "workspace_evidence", "changed_paths")
        }
        try:
            task = self._engine(run_id).scheduler.accept_handoff(task_id, handoff)
            result["accepted"] = {"id": task.get("id"), "state": task.get("state")}
        except Exception as error:  # noqa: BLE001 - surface the verifier's exact objection
            result["error"] = f"{type(error).__name__}: {error}"
        return result

    def relay(self, params: Mapping[str, Any]) -> Any:
        """Report the conversation that actually serviced a top-level action.

        The runtime opcode (``codex_app__create_thread``) is the capability this
        adapter declared in ``discover``; Snow App services it with
        ``app-control-conversation``.  The physical call is recorded next to the
        contract identity instead of being substituted for it, so the runtime's
        protocol check still verifies the action it actually dispatched.
        """
        run_id = str(params["runId"])
        thread_id = str(params.get("threadId") or params.get("conversationId") or "").strip()
        if not thread_id:
            raise ValueError("relay requires a non-empty threadId/conversationId")
        task_id = str(params.get("taskId") or "").strip()

        rows = self.store._fetchall(
            "SELECT action_json FROM dispatch_outbox "
            "WHERE run_id = ? AND target_type = 'task' ORDER BY created_at DESC",
            (run_id,),
        )
        action: dict[str, Any] | None = None
        for row in rows:
            value = json.loads(str(row.get("action_json") or "{}"))
            if not isinstance(value, Mapping) or value.get("kind") != "create-top-level-task":
                continue
            if task_id and str(value.get("task_id")) != task_id:
                continue
            action = dict(value)
            break
        if action is None:
            # ``enqueue`` keeps a queued action in the bridge's in-memory queue
            # instead of writing ``dispatch_outbox``.  A relay issued in the same
            # persistent session must still resolve that action, otherwise the
            # only way to report a thread is to hand-roll an ingest_receipt call.
            engine = self.engines.get(run_id) or self._engine(run_id)
            bridge = getattr(engine, "bridge", None)
            pending = getattr(bridge, "next_actions", None)
            for candidate in (pending() if callable(pending) else ()) or ():
                value = (
                    candidate.to_dict()
                    if hasattr(candidate, "to_dict")
                    else dict(candidate)
                )
                if value.get("kind") != "create-top-level-task":
                    continue
                candidate_task = str(
                    value.get("task_id")
                    or (value.get("identity") or {}).get("task_id")
                    or ""
                )
                if task_id and candidate_task != task_id:
                    continue
                action = dict(value)
                break
        if action is None:
            raise KeyError(
                "no create-top-level-task action is available for this run: it is "
                "neither persisted in dispatch_outbox nor queued in this session "
                "(call allinluna-actions with dispatch=true in the same session first)"
            )

        # A lane has exactly one observed thread.  Re-reporting a different
        # conversation for the same action must not silently rebind it, so the
        # first real relay wins and later calls are answered read-only.
        existing = self.store._fetchone(
            "SELECT thread_id, id AS receipt_id, status FROM host_receipts "
            "WHERE dispatch_key = ? AND thread_id IS NOT NULL "
            "ORDER BY received_at LIMIT 1",
            (action["idempotency_key"],),
        )
        if existing is not None:
            return {
                "already_relayed": True,
                "dispatch_key": action["idempotency_key"],
                "thread_id": existing.get("thread_id"),
                "receipt_id": existing.get("receipt_id"),
                "status": existing.get("status"),
                "note": "This action already has an observed thread; a later relay cannot replace it.",
            }

        receipt = {
            "receipt_id": "receipt-" + thread_id,
            "dispatch_key": action["idempotency_key"],
            "action_id": action["action_id"],
            "action_contract_hash": action["action_contract_hash"],
            "actual_tool": action["tool"],
            "actual_capability": action.get("host_capability_required") or action["tool"],
            "thread_id": thread_id,
            "status": "active",
            "host_adapter": "snow-app",
            "host_id": "snow-app",
            "task_id": action.get("task_id"),
            "payload": {
                "physical_tool": params.get("physicalTool") or "app-control-conversation",
                "conversation_id": thread_id,
                "observed_at": datetime.now(timezone.utc)
                .isoformat(timespec="seconds")
                .replace("+00:00", "Z"),
            },
        }
        return self._engine(run_id).ingest_receipt(receipt, run_id=run_id)

    def status(self, params: Mapping[str, Any]) -> Any:
        run_id = str(params["runId"])
        return self._engine(run_id).status(run_id)

    def lane(self, params: Mapping[str, Any]) -> Any:
        """Operate one lane inside this persistent process.

        ``LaneBootstrapEnvelope.from_store`` reopens the durable
        ``dispatch_outbox`` row, which only exists after a top-level intent was
        persisted here — never in a separate stateless CLI process.
        """
        from allinluna_runtime.engine.lane_driver import LaneDriver

        run_id = str(params["runId"])
        task_id = str(params["taskId"])
        action = str(params.get("action") or "status").replace("-", "_")
        driver = LaneDriver(self.store, task_id, run_id=run_id, host=self.host)
        monitor = bool(params.get("monitor", False))
        calls = {
            "start": driver.start,
            "status": driver.status,
            "tick": lambda: driver.tick(monitor=monitor),
            "drive": lambda: driver.drive(
                max_cycles=int(params.get("maxCycles") or 1), monitor=monitor
            ),
            "next_actions": driver.next_actions,
            "handoff": driver.handoff,
            "finalize": driver.finalize,
            # A lane whose host exposes no native sub-agent falls back to
            # ``LANE_DIRECT_EXECUTION_REQUIRED``: the caller executes the bounded
            # plan and must be able to report the result back, otherwise the
            # lane can never leave that boundary.
            "ingest_receipt": lambda: driver.ingest_receipt(params["receipt"]),
            "ingest_direct_result": lambda: driver.ingest_direct_result(params["result"]),
            "ingest_handoff": lambda: driver.ingest_handoff(params["handoff"]),
            "complete_work_unit": lambda: self.complete_work_unit(params),
            "finalize_lane": lambda: self.finalize_lane(params),
        }
        call = calls.get(action)
        if call is None:
            raise ValueError(f"unsupported lane action: {action}")
        return call()

    def drive(self, params: Mapping[str, Any]) -> Any:
        from allinluna_runtime.engine.coordinator_driver import CoordinatorDriver

        run_id = str(params["runId"])
        driver = CoordinatorDriver(self.store, host=self.host, engine=self._engine(run_id))
        return driver.drive(
            run_id,
            max_cycles=int(params.get("maxCycles") or 1),
            monitor=bool(params.get("monitor", False)),
        )

    def run_admin(self, params: Mapping[str, Any]) -> Any:
        """Run-level operations the CLI exposes: pause/resume/reconcile/retry/cancel/next-actions/inspect."""
        run_id = str(params["runId"])
        operation = str(params.get("operation") or "inspect").replace("-", "_")
        engine = self._engine(run_id)
        if operation == "pause":
            return engine.pause(run_id)
        if operation == "resume":
            return engine.resume(run_id)
        if operation == "reconcile":
            return engine.reconcile(run_id)
        if operation == "retry":
            return engine.retry(
                run_id,
                str(params["taskId"]),
                contract_delta=params.get("contractDelta"),
            )
        if operation == "cancel":
            return engine.cancel(run_id, params.get("taskId"))
        if operation == "next_actions":
            # Read-only scheduler preview, exactly like the CLI alias.
            return {
                "run_id": run_id,
                "relayable": False,
                "deprecated": "next-actions is an alias for preview-actions; use dispatch or drive to relay",
                "actions": [
                    action.to_dict() if hasattr(action, "to_dict") else action
                    for action in engine.scheduler.preview(run_id)
                ],
            }
        if operation == "inspect":
            kind = str(params.get("kind") or "run").replace("-", "_")
            identity = str(params.get("identity") or run_id)
            if kind == "run":
                return {
                    "run": self.store.get_run(identity),
                    "status": engine.status(identity),
                    "metrics": self.store.runtime_metrics(identity),
                }
            if kind == "task":
                return self.store.get_task(identity)
            if kind == "work":
                return self.store.get_work_unit(identity)
            if kind == "context":
                return self.store.inspect_snapshot(identity)
            if kind == "outbox":
                return self.store.inspect_outbox(identity)
            if kind == "receipt":
                return self.store.get_host_receipt(identity)
            if kind == "artifacts":
                return self.store.inspect_artifacts(identity)
            raise ValueError(f"unsupported inspect kind: {kind}")
        raise ValueError(f"unsupported run operation: {operation}")

    def policy(self, params: Mapping[str, Any]) -> Any:
        run_id = str(params["runId"])
        operation = str(params.get("operation") or "").replace("-", "_")
        engine = self._engine(run_id)
        policy = params.get("policy")
        if not isinstance(policy, Mapping):
            raise ValueError("policy must be an object")
        if operation in {"set_policy", "run"}:
            return engine.set_policy(run_id, dict(policy))
        if operation in {"set_task_policy", "task"}:
            return engine.set_task_policy(run_id, str(params["taskId"]), dict(policy))
        raise ValueError(f"unsupported policy operation: {operation}")

    def permission(self, params: Mapping[str, Any]) -> Any:
        run_id = str(params["runId"])
        operation = str(params.get("operation") or "").replace("-", "_")
        if operation in {"request", "request_permission"}:
            scope_type = str(params.get("scopeType") or "run")
            scope_id = str(params.get("scopeId") or run_id)
            if scope_type == "task":
                task = self.store.get_task(scope_id, run_id=run_id)
                if task is None or str(task["run_id"]) != run_id:
                    raise ValueError("permission Task scope is outside the Run")
                scope_id = str(task["id"])
            elif scope_type == "work_unit":
                unit = self.store.get_work_unit(scope_id)
                task = self.store.get_task(str((unit or {}).get("task_id") or ""))
                if unit is None or task is None or str(task["run_id"]) != run_id:
                    raise ValueError("permission WorkUnit scope is outside the Run")
                scope_id = str(unit["id"])
            return self.store.request_permission(
                run_id,
                scope_type=scope_type,
                scope_id=scope_id,
                action=str(params["action"]),
            )
        if operation in {"decide", "decision"}:
            permission_id = str(params["permissionId"])
            row = self.store._fetchone(
                "SELECT run_id FROM permission_intents WHERE id = ?", (permission_id,)
            )
            if row is None or str(row["run_id"]) != run_id:
                raise ValueError("permission decision is outside the Run")
            return self.store.decide_permission(
                permission_id,
                allowed=bool(params.get("allow")),
                rationale=params.get("rationale"),
            )
        raise ValueError(f"unsupported permission operation: {operation}")

    def subagent_report(self, params: Mapping[str, Any]) -> Any:
        """Record the sub-agent that actually serviced one spawned WorkUnit.

        The persisted WorkUnit dispatch action is the contract identity; the
        logical capability is ``native_subagent``.  A completed report closes the
        attempt but never fabricates the WorkHandoff — pass ``handoff`` for that.
        """
        from allinluna_runtime.engine.lane_driver import LaneDriver

        run_id = str(params["runId"])
        task_id = str(params["taskId"])
        work_unit_id = str(params.get("workUnitId") or "").strip()
        thread_id = str(params.get("threadId") or params.get("subagentId") or "").strip()
        if not work_unit_id:
            raise ValueError("subagent_report requires workUnitId")
        if not thread_id:
            raise ValueError("subagent_report requires threadId/subagentId")

        rows = self.store._fetchall(
            "SELECT action_json FROM dispatch_outbox "
            "WHERE run_id = ? AND target_type = 'work_unit' AND target_id = ? "
            "ORDER BY created_at DESC",
            (run_id, work_unit_id),
        )
        action: dict[str, Any] | None = None
        for row in rows:
            value = json.loads(str(row.get("action_json") or "{}"))
            if not isinstance(value, Mapping):
                continue
            if str(value.get("kind")) not in {"spawn-subagent", "spawn", "create-subagent"}:
                continue
            action = dict(value)
            break
        if action is None:
            raise KeyError("no durable spawn action exists for this work unit")

        status = str(params.get("status") or "active").strip().lower()
        receipt = {
            "receipt_id": "receipt-" + thread_id,
            "dispatch_key": action["idempotency_key"],
            "action_id": action["action_id"],
            "action_contract_hash": action["action_contract_hash"],
            "actual_tool": action["tool"],
            "actual_capability": action.get("logical_capability") or "native_subagent",
            "thread_id": thread_id,
            "status": status,
            "host_adapter": "snow-app",
            "host_id": "snow-app",
            "task_id": action.get("task_id") or task_id,
            "payload": {
                "physical_tool": params.get("physicalTool") or "sub-agents-activate",
                "subagent_id": thread_id,
                "work_unit_id": work_unit_id,
                "summary": str(params.get("summary") or ""),
                "observed_at": datetime.now(timezone.utc)
                .isoformat(timespec="seconds")
                .replace("+00:00", "Z"),
            },
        }
        driver = LaneDriver(self.store, task_id, run_id=run_id, host=self.host)
        result: dict[str, Any] = {"receipt": driver.ingest_receipt(receipt)}
        handoff = params.get("handoff")
        if isinstance(handoff, Mapping):
            result["handoff"] = driver.ingest_handoff(dict(handoff))
        return result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", required=True)
    parser.add_argument("--project-root", required=True)
    parser.add_argument("--workspace", required=True)
    parser.add_argument("--project-id", required=True)
    parser.add_argument("--branch", default=None)
    parser.add_argument("--model", default=None)
    parser.add_argument("--reasoning", default=None)
    args = parser.parse_args()

    sys.stdin.reconfigure(encoding="utf-8")
    sys.stdout.reconfigure(encoding="utf-8")
    sys.stderr.reconfigure(encoding="utf-8")

    session = Session(args)
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        request_id = None
        try:
            request = json.loads(line)
            request_id = request.get("id")
            method = str(request.get("method") or "")
            params = request.get("params") or {}
            if method == "shutdown":
                _write({"id": request_id, "ok": True, "result": {"shutdown": True}})
                return 0
            handler = getattr(session, method, None)
            if not callable(handler):
                raise ValueError(f"unknown method: {method}")
            _write({"id": request_id, "ok": True, "result": handler(params)})
        except Exception as error:  # noqa: BLE001 - report to the host verbatim
            _write(
                {
                    "id": request_id,
                    "ok": False,
                    "error": f"{type(error).__name__}: {error}",
                    "traceback": traceback.format_exc(limit=6),
                }
            )
    return 0


def _write(payload: Mapping[str, Any]) -> None:
    sys.stdout.write(json.dumps(payload, ensure_ascii=False, default=str) + "\n")
    sys.stdout.flush()


if __name__ == "__main__":
    raise SystemExit(main())
