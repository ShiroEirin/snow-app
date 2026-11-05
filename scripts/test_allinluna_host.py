"""Smoke-drive the persistent All in Luna host helper."""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DB = str(ROOT / ".allinluna" / "comfyui-analysis.db")
RUN = "run-Design-a-native-ComfyUI-provider-for-Snow-App-im"
TASK = RUN + ":task:deliver"


def main() -> int:
    process = subprocess.Popen(
        [
            sys.executable,
            str(ROOT / "scripts" / "allinluna_host.py"),
            "--db",
            DB,
            "--project-root",
            r"D:\github\snow-app",
            "--workspace",
            r"D:\github\snow-app\snow-app",
            "--project-id",
            r"local:D:\github\snow-app\snow-app",
            "--branch",
            "fork-main",
            "--model",
            "auto/deepseek-v4.1-flash",
        ],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        bufsize=1,
    )

    def call(method: str, params: dict, request_id: int = 1) -> dict:
        assert process.stdin is not None and process.stdout is not None
        process.stdin.write(json.dumps({"id": request_id, "method": method, "params": params}) + "\n")
        process.stdin.flush()
        line = process.stdout.readline()
        if not line:
            error = process.stderr.read() if process.stderr else ""
            raise RuntimeError(f"helper exited: {error}")
        return json.loads(line)

    result = call("dispatch", {"runId": RUN})
    print("=== dispatch ===")
    print(json.dumps(result, ensure_ascii=False, indent=2)[:4000])

    if result.get("ok"):
        actions = result["result"].get("actions") or []
        print("\n=== action summary ===")
        for action in actions:
            payload = action.get("payload") or {}
            print(
                {
                    "kind": action.get("kind"),
                    "tool": action.get("tool"),
                    "model": action.get("model"),
                    "executable": payload.get("executable"),
                    "reason": payload.get("reason"),
                }
            )
        receipts = result["result"].get("receipts") or []
        print("\n=== dispatch receipts ===")
        for receipt in receipts:
            print(
                {
                    "status": receipt.get("status"),
                    "reason": receipt.get("reason"),
                    "thread_id": receipt.get("receipt", {}).get("thread_id")
                    if isinstance(receipt.get("receipt"), dict)
                    else None,
                }
            )

    result = call("lane", {"runId": RUN, "taskId": TASK, "action": "start"}, 2)
    print("\n=== lane start ===")
    print(json.dumps(result, ensure_ascii=False, indent=2)[:2000])

    result = call("status", {"runId": RUN}, 3)
    print("\n=== status ===")
    print(json.dumps(result, ensure_ascii=False, indent=2)[:1500])

    call("shutdown", {}, 4)
    process.wait(timeout=10)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
