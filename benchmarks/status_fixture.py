"""Synthetic recorded-state fixtures; these paths are not provisioned worktrees."""
import hashlib
import json


STAMP = "2026-01-01T00:00:00.123Z"
UUID = "11111111-2222-3333-4444-555555555555"


def write_record(path, data):
    path.parent.mkdir(parents=True, mode=0o700, exist_ok=True)
    path.write_text(json.dumps(data, ensure_ascii=False))
    path.chmod(0o600)
    return path


def receipt_path(state, receipt):
    key = json.dumps([receipt["socket"], receipt["path"]], separators=(",", ":"), ensure_ascii=False)
    return state / "setup" / (hashlib.sha256(key.encode()).hexdigest() + ".json")


def plan(kind, status="armed"):
    return {
        "runId": UUID, "attempt": 0, "status": status,
        "hooks": [{"name": "synthetic-hook", "command": ["sh", "-c", "printf synthetic-private-argument"], "timeoutMs": 1000}],
        "context": {"PUBLIC_NOTE": "synthetic-private-context"},
        "results": [] if status == "armed" else [{"phase": kind, "name": "synthetic-hook", "status": "failed", "elapsedMs": 1, "exitCode": 7, "reason": "command-failed"}],
    }


def setup(task, path):
    return {
        "schemaVersion": 1, "socket": None, "workspaceId": None, "path": str(path),
        "project": "bench" if task else None, "taskId": task["id"] if task else None,
        "status": "ready", "step": "complete", "message": "Setup complete", "installKey": "synthetic-install-key",
        "installOutputs": [{"path": "node_modules/fixture", "kind": "file"}],
        "dependencies": "ready", "skipInstall": False, "runId": UUID,
        "timings": {"id": UUID, "operation": "setup", "startedAt": STAMP, "finishedAt": STAMP, "elapsedMs": 0,
                    "stagesMs": {"route": 1, "finalize": 2}, "dependencies": "reused"},
        "hooks": [{"phase": "afterDependencies", "name": "checked", "status": "succeeded", "elapsedMs": 1, "exitCode": 0}],
    }


def seed_status(fixture, count):
    state = fixture.state
    state.mkdir(parents=True, mode=0o700, exist_ok=True)
    files = {"tasks": [], "setups": [], "adopted": [], "frozen": []}
    for index in range(count):
        name = f"item-{index:03}"
        path = fixture.root / "trees" / name
        url = f"https://fixture.example.test:{5200 + index}" if index % 2 == 0 else None
        task = {
            "schemaVersion": 4, "id": f"bench--{name}", "project": "bench", "repo": str(fixture.repo), "path": str(path),
            "branch": f"task/{name}", "baseCommit": fixture.commit, "socket": None, "workspaceId": None, "tabs": {},
            "localPort": 32000 + index, "httpsPort": 5200 + index if url else None,
            "host": "fixture.example.test" if url else None, "url": url,
            "envOverrides": {"PORT": "{{localPort}}", "PUBLIC_SLUG": "{{taskSlug}}", "TASK_NAME": "{{ worktreeName }}"},
            "overrides": {"PORT": str(32000 + index), "PUBLIC_SLUG": f"bench_item_{index:03}", "TASK_NAME": name},
            "status": ["ready", "preparing", "cleaned", "closed"][index % 4],
        }
        if url:
            task["envOverrides"]["BASE_URL"] = "{{url}}"
            task["overrides"]["BASE_URL"] = url
        if index % 3 == 0:
            task["ports"] = {"api": 36000 + index}
            task["envOverrides"]["API_PORT"] = "{{port.api}}"
            task["overrides"]["API_PORT"] = str(36000 + index)
        if index % 2 == 0:
            task["cleanup"] = plan("cleanup", "failed" if index % 4 == 0 else "armed")
        if index % 3 == 0:
            task["retire"] = plan("retire", "failed")
        files["tasks"].append(write_record(state / (task["id"] + ".json"), task))
        receipt = setup(task, path)
        if index % 3 == 1:
            receipt.update(status="incomplete", step="dependencies", dependencies="failed", message="Installer failed; inspect logs\nnot a second row")
        if index == 0:
            receipt["step"] = "route"
        files["setups"].append(write_record(receipt_path(state, receipt), receipt))
        if index == 0:
            receipt["socket"] = "/synthetic/previous.sock"
            files["setups"].append(write_record(receipt_path(state, receipt), receipt))
    for orphan in ["é", "a", "A", "日本語-👩‍👩‍👧‍👦-é"]:
        receipt = setup(None, fixture.root / "orphan" / orphan)
        receipt.update(status="unmanaged", message="External setup was not changed", dependencies="not-configured")
        files["setups"].append(write_record(receipt_path(state, receipt), receipt))
    for index in range(2):
        record = {
            "schemaVersion": 1, "id": f"other--external-{index}", "project": "other", "repo": str(fixture.repo),
            "path": str(fixture.root / f"external/{index}"), "branch": f"feature/external-{index}", "adoptedAt": STAMP,
            "socket": None, "workspaceId": None, "localPort": None if index == 0 else 3001, "url": None,
            "environment": "external", "dependencies": "pending" if index else "external",
            "hooks": [{"phase": "afterDependencies", "name": "manual", "status": "failed"}] if index else [],
        }
        files["adopted"].append(write_record(state / "adopted" / (record["id"] + ".json"), record))
    for index, status in enumerate(["frozen", "freezing", "thawing", "attention", "thawed"]):
        record_id = f"bench--stored-{index}"
        record = {
            "schemaVersion": 1, "id": record_id, "generation": UUID, "status": status, "frozenAt": STAMP,
            "until": "2100-01-01T12:00:00.000Z" if index % 2 else None, "note": "Return after upstream review",
            "skipInstall": False, "attempts": index, "nextAttemptAt": None, "message": "Synthetic recovery record",
            "thawedAt": STAMP if status == "thawed" else None,
            "removal": {"id": record_id, "path": str(fixture.root / f"trees/stored-{index}"), "repo": str(fixture.repo),
                        "branch": f"task/stored-{index}", "baseCommit": fixture.commit, "socket": None, "workspaceId": None,
                        "currentBranch": f"task/stored-{index}", "currentCommit": fixture.commit, "cleanupRunId": UUID},
        }
        files["frozen"].append(write_record(state / "frozen" / (record_id + ".json"), record))
    return files
