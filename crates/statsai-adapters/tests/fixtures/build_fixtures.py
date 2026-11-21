#!/usr/bin/env python3
"""Build privacy-safe StatsAI provider fixtures from local trace schemas.

Original provider stores are opened read-only. Outputs are written only below
--output (default: tests/fixtures beside this script). Conversation text,
projects, users, paths, URLs, git identifiers, and provider record IDs are
replaced with deterministic synthetic values while schema, numeric usage,
timestamps, public model names, and provider enums are retained.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import sqlite3
import sys
import tempfile
from collections import defaultdict, deque
from pathlib import Path
from typing import Any, Callable, Iterable


HOME = Path.home()
DEFAULT_CLAUDE = HOME / ".claude"
DEFAULT_CODEX = HOME / ".codex"
DEFAULT_OPENCODE = HOME / ".local/share/opencode"
DEFAULT_GROK = HOME / ".grok"
DEFAULT_ANTIGRAVITY = HOME / ".gemini/antigravity"

SAFE_ENUM_KEYS = {
    "type", "role", "stop_reason", "stop_sequence", "entrypoint", "origin",
    "originator", "source", "userType", "permissionMode", "promptSource",
    "approval_policy", "sandbox_policy", "sandbox_profile", "effort",
    "reasoning_effort", "speed", "service_tier", "inference_geo", "status",
    "priority", "operation", "method", "mode", "agent", "finish", "vcs",
    "chat_format_version", "version", "cli_version", "model_provider",
}
MODEL_KEYS = {
    "model", "model_id", "modelID", "model_name", "current_model_id",
    "providerID", "provider_id",
}
ID_KEYS = {
    "id", "uuid", "sessionId", "session_id", "conversation_id", "thread_id",
    "promptId", "prompt_id", "requestId", "request_id", "parentUuid",
    "parent_id", "leafUuid", "sourceToolAssistantUUID", "tool_use_id",
    "call_id", "message_id", "project_id", "workspace_id", "userID",
    "machineID", "accountUuid", "sid", "trace_id", "agent_id",
}
PATH_KEYS = {
    "cwd", "directory", "worktree", "git_root_dir", "grok_home", "root",
    "project_path", "workspace_path", "file_path", "path",
}
TEXT_KEYS = {
    "content", "message", "text", "thinking", "instructions", "lastPrompt",
    "customTitle", "title", "summary", "session_summary", "generated_title",
    "description", "todo", "reason", "synthetic_reason", "output",
    "toolUseResult", "name_hint", "display_name", "slug", "command",
    "patch", "diff", "summary_diffs", "error",
}
EMAIL_KEYS = {"email", "emailAddress", "email_address"}
URL_KEYS = {"url", "share_url", "icon_url", "repository_url", "remote_url"}
BRANCH_KEYS = {"gitBranch", "head_branch", "branch", "branch_name"}
COMMIT_KEYS = {"head_commit", "commit", "commit_hash", "revision"}
TIMESTAMP_KEYS = {
    "timestamp", "created_at", "updated_at", "profileFetchedAt", "time",
    "firstStartTime", "firstSessionDate", "lastComputedDate",
}
# A public model name, not merely a string that starts like one. Requiring a
# version-ish component keeps local identifiers such as `claude-code-guide`,
# `grok-build-plan`, or a custom agent name from being retained as "public".
PUBLIC_MODEL_RE = re.compile(
    r"^(?:openai/|anthropic/|google/|xai/|opencode-go/)?"
    r"(?:claude|gpt|o[1-9]|grok|gemini|deepseek|qwen|llama|mistral)"
    r"[A-Za-z0-9._:/+\-]*"
    r"(?:[0-9]|[0-9][a-z]?|latest|preview|thinking|mini|opus|sonnet|haiku|turbo)$",
    re.IGNORECASE,
)

# Length is a poor way to tell an encoded string from ordinary numeric data:
# `/Users/someone` is 15 bytes, so any threshold near that size leaves exactly
# the payload worth catching unexamined. The floor only excludes arrays too
# short to carry anything identifying; what actually separates text from data is
# the content test in `decode_byte_array`.
BYTE_ARRAY_MIN_LEN = 4

# Session columns already replaced with synthetic values before insertion; every
# other string column is sanitized so a live one cannot ride through untouched.
SESSION_VERBATIM_COLUMNS = {"id", "title", "path", "directory", "worktree", "projectID", "project_id"}
PROJECT_VERBATIM_COLUMNS = {"id", "name", "worktree", "path", "directory", "sandboxes", "commands"}


def decode_byte_array(value: list[Any]) -> str | None:
    """Returns the text a list-of-bytes encodes, when it plausibly encodes text.

    Providers record shell output as an array of integers rather than a string.
    Nothing in a string-oriented sanitizer or validator can see inside that, so
    a home directory, username, git remote, or directory listing rides through
    every check untouched. Decoding is what makes those payloads visible.
    """
    if len(value) < BYTE_ARRAY_MIN_LEN:
        return None
    if not all(
        isinstance(item, int) and not isinstance(item, bool) and 0 <= item <= 255
        for item in value
    ):
        return None
    try:
        text = bytes(value).decode("utf-8")
    except (UnicodeDecodeError, ValueError):
        return None
    printable = sum(character.isprintable() or character in "\r\n\t" for character in text)
    if printable < len(text) * 0.9:
        return None
    # Requiring a letter is what keeps genuine numeric data intact: token
    # counts, byte sizes, and timing samples decode to control characters or to
    # digits, never to words, while a path, username, or command always carries
    # letters.
    return text if any(character.isalpha() for character in text) else None
ISO_DATE_RE = re.compile(r"^\d{4}-\d{2}-\d{2}(?:[T ][0-9:.+\-Z]+)?$")
EMAIL_RE = re.compile(r"(?i)\b[A-Z0-9._%+\-]+@[A-Z0-9.\-]+\.[A-Z]{2,}\b")
URL_RE = re.compile(r"(?i)\b(?:https?|ssh)://\S+")
SECRET_RE = re.compile(
    r"(?i)(?:sk-[A-Za-z0-9_-]{16,}|Bearer\s+[A-Za-z0-9._-]{16,}|"
    r"eyJ[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{10,})"
)


class Sanitizer:
    def __init__(self) -> None:
        self.ids: dict[str, str] = {}
        self.counters: defaultdict[str, int] = defaultdict(int)
        self.forbidden_tokens: set[str] = set()

    def fake_id(self, key: str, value: str) -> str:
        bucket = normalize_key(key)
        if value not in self.ids:
            self.counters[bucket] += 1
            prefix = {
                "sessionid": "ses", "session_id": "ses", "sid": "ses",
                "uuid": "uuid", "id": "id", "message_id": "msg",
                "call_id": "call", "requestid": "req", "request_id": "req",
                "project_id": "prj", "workspace_id": "wsp",
                "accountuuid": "acct", "userid": "usr", "machineid": "machine",
            }.get(bucket, bucket[:12] or "id")
            self.ids[value] = f"{prefix}_fixture_{self.counters[bucket]:03d}"
        return self.ids[value]

    def sanitize(self, value: Any, key: str = "", parent: dict[str, Any] | None = None) -> Any:
        if value is None or isinstance(value, (bool, int, float)):
            return value
        if isinstance(value, list):
            decoded = decode_byte_array(value)
            if decoded is not None:
                # Re-encoded so the fixture still exercises the byte-array
                # decoding path that consumers have to handle.
                self._remember_sensitive(decoded, include_basename=False)
                replacement = self.sanitize(decoded, key or "output", parent)
                if not isinstance(replacement, str):
                    replacement = "fixture tool output\n"
                return list(replacement.encode("utf-8"))
            return [self.sanitize(item, key, parent) for item in value]
        if isinstance(value, dict):
            return {str(k): self.sanitize(v, str(k), value) for k, v in value.items()}
        if not isinstance(value, str):
            return value

        normalized = normalize_key(key)
        if key in ID_KEYS or normalized in {normalize_key(k) for k in ID_KEYS}:
            return self.fake_id(key, value)
        if key in EMAIL_KEYS or "email" in normalized:
            self._remember_sensitive(value, include_basename=False)
            return "fixture-user@example.invalid"
        if key in PATH_KEYS or looks_like_path(value):
            self._remember_sensitive(value, include_basename=True)
            return fake_path(value)
        if key in URL_KEYS or "url" in normalized or URL_RE.search(value):
            self._remember_sensitive(value, include_basename=False)
            return "https://example.invalid/acme/sample-project"
        if key in BRANCH_KEYS or "branch" in normalized:
            return "fixture/main"
        if key in COMMIT_KEYS:
            return "0123456789abcdef0123456789abcdef01234567"
        if key in MODEL_KEYS:
            return value if PUBLIC_MODEL_RE.fullmatch(value.strip()) else "fixture-model-v1"
        if key == "name" and re.fullmatch(r"[A-Za-z][A-Za-z0-9_.:\-]{0,63}", value):
            return value
        if key == "msg" and value == "shell.turn.inference_done":
            return value
        if key in SAFE_ENUM_KEYS and safe_enum(value):
            return value
        if key in TIMESTAMP_KEYS and ISO_DATE_RE.fullmatch(value.strip()):
            return value
        if key == "arguments":
            return json.dumps({"path": "/workspace/sample-project/README.md"}, separators=(",", ":"))
        if key == "encrypted_content":
            return ""
        if key == "output":
            return json.dumps({"output": "fixture tool output", "metadata": {"exit_code": 0}}, separators=(",", ":"))
        if key in TEXT_KEYS or normalized in {normalize_key(k) for k in TEXT_KEYS}:
            return fake_text(key)
        if ISO_DATE_RE.fullmatch(value.strip()):
            return value
        if PUBLIC_MODEL_RE.fullmatch(value.strip()):
            return value
        if value == "":
            return value
        return self.fake_id(key or "value", value)

    def _remember_sensitive(self, value: str, include_basename: bool) -> None:
        if len(value) >= 5:
            self.forbidden_tokens.add(value)
        if include_basename:
            basename = Path(value).name
            if len(basename) >= 5 and basename not in {".grok", "projects", "sessions", "summary.json", "signals.json", "events.jsonl", "updates.jsonl", "chat_history.jsonl"}:
                self.forbidden_tokens.add(basename)


def normalize_key(key: str) -> str:
    return re.sub(r"[^a-z0-9_]", "", key.lower())


def safe_enum(value: str) -> bool:
    return bool(re.fullmatch(r"[A-Za-z0-9_.:/+\-]{0,96}", value))


def looks_like_path(value: str) -> bool:
    return (
        value.startswith(("/Users/", "/home/", "/workspace/", "~/"))
        or re.match(r"^[A-Za-z]:\\", value) is not None
    )


def fake_path(value: str) -> str:
    suffix = Path(value).suffix if len(value) < 4096 else ""
    if suffix and len(suffix) <= 12:
        return f"/workspace/sample-project/fixture{suffix}"
    return "/workspace/sample-project"


def fake_text(key: str) -> str:
    normalized = normalize_key(key)
    if "title" in normalized or normalized == "slug":
        return "Implement fixture parser"
    if normalized in {"thinking", "reason", "synthetic_reason"}:
        return "Consider the fixture schema and validate the parser."
    if normalized in {"instructions"}:
        return "You are a fixture coding assistant working in a synthetic project."
    if normalized in {"command"}:
        return "pwd"
    if normalized in {"patch", "diff", "summary_diffs"}:
        return "*** synthetic fixture diff ***"
    if normalized == "error":
        return "synthetic fixture error"
    return "Create a parser for the synthetic fixture and add validation."


def readonly_snapshot(paths: Iterable[Path]) -> dict[Path, tuple[int, int, int]]:
    result = {}
    for path in paths:
        stat = path.stat()
        result[path] = (stat.st_ino, stat.st_size, stat.st_mtime_ns)
    return result


def assert_unchanged(snapshot: dict[Path, tuple[int, int, int]]) -> None:
    changed = [path for path, before in snapshot.items() if not path.exists() or readonly_snapshot([path])[path] != before]
    if changed:
        raise RuntimeError("an original trace changed while fixtures were built")


def jsonl_paths(root: Path) -> list[Path]:
    return sorted((p for p in root.rglob("*.jsonl") if p.is_file()), key=lambda p: (p.stat().st_size, str(p)))


def valid_jsonl(path: Path) -> Iterable[tuple[int, dict[str, Any]]]:
    with path.open("r", encoding="utf-8", errors="replace") as handle:
        for index, line in enumerate(handle):
            if not line.strip():
                continue
            try:
                value = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(value, dict):
                yield index, value


def choose_jsonl(files: list[Path], predicate: Callable[[dict[str, Any]], bool]) -> tuple[Path, int]:
    for path in files:
        for index, value in valid_jsonl(path):
            if predicate(value):
                return path, index
    raise RuntimeError("no representative trace matched a required fixture feature")


def records_around(path: Path, target_index: int, before: int = 10, after: int = 18) -> list[dict[str, Any]]:
    prefix: list[dict[str, Any]] = []
    window: deque[tuple[int, dict[str, Any]]] = deque(maxlen=before + 1)
    suffix: list[dict[str, Any]] = []
    found = False
    for index, value in valid_jsonl(path):
        if len(prefix) < 4:
            prefix.append(value)
        window.append((index, value))
        if index == target_index:
            found = True
            suffix = [item for _, item in window]
            continue
        if found:
            suffix.append(value)
            if len(suffix) >= before + after + 1:
                break
    records = prefix + suffix
    unique: list[dict[str, Any]] = []
    seen = set()
    for record in records:
        marker = id(record)
        if marker not in seen:
            seen.add(marker)
            unique.append(record)
    return unique


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def write_jsonl(path: Path, records: Iterable[Any], malformed: str | None = None) -> int:
    path.parent.mkdir(parents=True, exist_ok=True)
    count = 0
    with path.open("w", encoding="utf-8", newline="\n") as handle:
        for record in records:
            handle.write(json.dumps(record, separators=(",", ":"), ensure_ascii=False) + "\n")
            count += 1
        if malformed is not None:
            handle.write(malformed + "\n")
    return count


def claude_cache_write(value: dict[str, Any]) -> bool:
    usage = value.get("message", {}).get("usage", {}) if isinstance(value.get("message"), dict) else {}
    if not isinstance(usage, dict):
        return False
    direct = usage.get("cache_creation_input_tokens", 0)
    nested = usage.get("cache_creation", {})
    nested_total = sum(v for v in nested.values() if isinstance(v, (int, float))) if isinstance(nested, dict) else 0
    return isinstance(direct, (int, float)) and direct > 0 or nested_total > 0


def has_claude_usage(value: dict[str, Any]) -> bool:
    return isinstance(value.get("message"), dict) and isinstance(value["message"].get("usage"), dict)


def codex_payload_type(value: dict[str, Any]) -> str:
    payload = value.get("payload")
    return str(payload.get("type", "")) if isinstance(payload, dict) else ""


def codex_has_usage(value: dict[str, Any]) -> bool:
    if codex_payload_type(value) != "token_count":
        return False
    payload = value.get("payload")
    info = payload.get("info") if isinstance(payload, dict) else None
    if not isinstance(info, dict):
        return False
    stack = [info]
    while stack:
        current = stack.pop()
        for key, item in current.items():
            if isinstance(item, dict):
                stack.append(item)
            elif key in {"input_tokens", "output_tokens", "total_tokens", "cached_input_tokens", "reasoning_output_tokens"} and isinstance(item, (int, float)) and item > 0:
                return True
    return False


def build_claude(root: Path, output: Path, sanitizer: Sanitizer, sources: set[Path]) -> dict[str, Any]:
    files = jsonl_paths(root / "projects")
    if not files:
        raise RuntimeError("Claude project traces are unavailable")
    basic_path, basic_index = choose_jsonl(files, has_claude_usage)
    cache_path, cache_index = choose_jsonl(files, claude_cache_write)
    sources.update({basic_path, cache_path})

    basic_records = [sanitizer.sanitize(v) for v in records_around(basic_path, basic_index)]
    cache_records = [sanitizer.sanitize(v) for v in records_around(cache_path, cache_index)]
    basic_count = write_jsonl(output / "claude/basic/projects/-workspace-sample-project/session-basic.jsonl", basic_records)
    cache_count = write_jsonl(output / "claude/cache-write/projects/-workspace-cache-project/session-cache-write.jsonl", cache_records)
    malformed_count = write_jsonl(
        output / "claude/malformed-record/projects/-workspace-malformed-project/session-malformed.jsonl",
        basic_records,
        '{"type":"assistant","message":',
    )

    by_project: dict[Path, list[Path]] = defaultdict(list)
    for path in files:
        try:
            project = path.relative_to(root / "projects").parts[0]
        except (ValueError, IndexError):
            continue
        by_project[Path(project)].append(path)
    project_samples: list[tuple[Path, int]] = []
    for project_files in by_project.values():
        try:
            project_samples.append(choose_jsonl(project_files, has_claude_usage))
        except RuntimeError:
            continue
        if len(project_samples) >= 2:
            break
    if len(project_samples) < 2:
        raise RuntimeError("Claude multi-project fixture needs two project stores")
    multi_counts = []
    for number, (path, usage_index) in enumerate(project_samples, start=1):
        selected = records_around(path, usage_index, before=8, after=12)
        sources.add(path)
        count = write_jsonl(
            output / f"claude/multi-project/projects/-workspace-project-{number}/session-project-{number}.jsonl",
            [sanitizer.sanitize(v) for v in selected],
        )
        multi_counts.append(count)

    stats_path = root / "stats-cache.json"
    if not stats_path.is_file():
        raise RuntimeError("Claude stats-cache.json is unavailable")
    sources.add(stats_path)
    stats = json.loads(stats_path.read_text(encoding="utf-8"))
    write_json(output / "claude/subscription/stats-cache.json", sanitizer.sanitize(stats))
    write_json(
        output / "claude/subscription/.claude.json",
        {
            "oauthAccount": {
                "accountUuid": "acct_fixture_claude_001",
                "emailAddress": "fixture-user@example.invalid",
                "profileFetchedAt": "2026-08-17T09:00:00Z",
            }
        },
    )
    projects_marker = output / "claude/subscription/projects/.gitkeep"
    projects_marker.parent.mkdir(parents=True, exist_ok=True)
    projects_marker.write_text("", encoding="utf-8")
    return {
        "basic": basic_count,
        "cache-write": cache_count,
        "malformed-record": malformed_count,
        "multi-project": multi_counts,
        "subscription": {"stats_cache_models": len(stats.get("modelUsage", {})), "profile": "synthetic"},
    }


def build_codex(root: Path, output: Path, sanitizer: Sanitizer, sources: set[Path]) -> dict[str, Any]:
    files = jsonl_paths(root / "sessions") + jsonl_paths(root / "archived_sessions")
    if not files:
        raise RuntimeError("Codex session traces are unavailable")
    basic_path, basic_index = choose_jsonl(files, codex_has_usage)
    reasoning_path, reasoning_index = choose_jsonl(
        files, lambda v: codex_payload_type(v) in {"reasoning", "agent_reasoning"}
    )
    compaction_path, compaction_index = choose_jsonl(
        files,
        lambda v: codex_payload_type(v) in {"compacted", "context_compacted", "compaction", "compaction_summary"}
        or v.get("type") in {"compacted", "context_compacted", "compaction"},
    )
    sources.update({basic_path, reasoning_path, compaction_path})

    def emit(name: str, path: Path, index: int, malformed: str | None = None) -> int:
        records = [sanitizer.sanitize(v) for v in records_around(path, index, before=14, after=24)]
        return write_jsonl(
            output / f"codex/{name}/sessions/2026/08/17/rollout-fixture-{name}.jsonl",
            records,
            malformed,
        )

    basic_count = emit("basic", basic_path, basic_index)
    reasoning_count = emit("reasoning", reasoning_path, reasoning_index)
    compaction_count = emit("compaction", compaction_path, compaction_index)
    malformed_count = emit("malformed", basic_path, basic_index, '{"timestamp":"2026-08-17T09:00:00Z","type":"event_msg","payload":{"type":"token_count"')
    return {
        "basic": basic_count,
        "reasoning": reasoning_count,
        "compaction": compaction_count,
        "malformed": malformed_count,
    }


def sqlite_ro(path: Path) -> sqlite3.Connection:
    quoted = str(path).replace("?", "%3f").replace("#", "%23")
    connection = sqlite3.connect(f"file:{quoted}?mode=ro&immutable=1", uri=True)
    connection.execute("PRAGMA query_only=ON")
    return connection


def table_exists(connection: sqlite3.Connection, name: str) -> bool:
    return connection.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", (name,)).fetchone() is not None


def create_opencode_v1(path: Path, row: sqlite3.Row, columns: list[str], sanitizer: Sanitizer) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    out = sqlite3.connect(path)
    out.executescript(
        """
        PRAGMA foreign_keys=ON;
        CREATE TABLE session (
          id TEXT PRIMARY KEY, title TEXT, model TEXT, cost REAL NOT NULL DEFAULT 0,
          tokens_input INTEGER NOT NULL DEFAULT 0, tokens_output INTEGER NOT NULL DEFAULT 0,
          tokens_reasoning INTEGER NOT NULL DEFAULT 0, tokens_cache_read INTEGER NOT NULL DEFAULT 0,
          tokens_cache_write INTEGER NOT NULL DEFAULT 0, time_created INTEGER NOT NULL,
          time_updated INTEGER NOT NULL, directory TEXT NOT NULL
        );
        """
    )
    data = dict(zip(columns, row))
    out.execute(
        "INSERT INTO session VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
        (
            # `model` is free-form: a locally configured provider/model blob
            # identifies the machine's setup, so it goes through the sanitizer
            # rather than being copied.
            "ses_fixture_v1", "Implement fixture parser",
            sanitizer.sanitize(data.get("model"), "model") or "gpt-5",
            data.get("cost") or 0.0, data.get("tokens_input") or 0,
            data.get("tokens_output") or 0, data.get("tokens_reasoning") or 0,
            data.get("tokens_cache_read") or 0, data.get("tokens_cache_write") or 0,
            data.get("time_created") or 1786957200000, data.get("time_updated") or 1786957260000,
            "/workspace/sample-project",
        ),
    )
    out.commit()
    out.close()


def create_opencode_v2(path: Path, source: sqlite3.Connection, session_row: sqlite3.Row, session_columns: list[str], sanitizer: Sanitizer) -> dict[str, int]:
    path.parent.mkdir(parents=True, exist_ok=True)
    out = sqlite3.connect(path)
    for table in ["project", "session", "message", "part", "todo"]:
        schema_row = source.execute("SELECT sql FROM sqlite_master WHERE type='table' AND name=?", (table,)).fetchone()
        if schema_row and schema_row[0]:
            out.execute(schema_row[0])

    session = dict(zip(session_columns, session_row))
    old_session_id = session["id"]
    old_project_id = session.get("project_id")
    fake_session_id = "ses_fixture_v2"
    fake_project_id = "prj_fixture_v2"
    replacements = {
        "id": fake_session_id, "project_id": fake_project_id, "parent_id": None,
        "slug": "fixture-session", "directory": "/workspace/sample-project",
        "title": "Implement fixture parser", "share_url": None,
        "summary_diffs": "*** synthetic fixture diff ***", "workspace_id": "wsp_fixture_v2",
        "path": "/workspace/sample-project",
    }
    for key, value in replacements.items():
        if key in session:
            session[key] = value
    for key in ["revert", "permission", "metadata"]:
        if key in session and isinstance(session[key], str) and session[key]:
            try:
                session[key] = json.dumps(sanitizer.sanitize(json.loads(session[key])), separators=(",", ":"))
            except json.JSONDecodeError:
                session[key] = "{}"

    if table_exists(out, "project"):
        project_columns = [r[1] for r in source.execute("PRAGMA table_info(project)")]
        project_row = source.execute("SELECT * FROM project WHERE id=?", (old_project_id,)).fetchone()
        if project_row is not None:
            project = dict(zip(project_columns, project_row))
            project.update({"id": fake_project_id, "worktree": "/workspace/sample-project", "name": "sample-project", "icon_url": None})
            for key in ["sandboxes", "commands"]:
                if key in project and isinstance(project[key], str) and project[key]:
                    try:
                        project[key] = json.dumps(sanitizer.sanitize(json.loads(project[key])), separators=(",", ":"))
                    except json.JSONDecodeError:
                        project[key] = "[]" if key == "sandboxes" else None
            # Same reasoning as the session row: only the columns overwritten
            # above are synthetic, so every other string is whatever the live
            # project held. `icon_url_override` is null here but is free-form,
            # and a personal avatar URL would pass every check the validator
            # makes.
            for column, cell in list(project.items()):
                if column in PROJECT_VERBATIM_COLUMNS or not isinstance(cell, str):
                    continue
                project[column] = sanitizer.sanitize(cell, column)
            placeholders = ",".join("?" for _ in project_columns)
            out.execute(f"INSERT INTO project ({','.join(project_columns)}) VALUES ({placeholders})", [project[c] for c in project_columns])

    # Explicitly overwritten columns above are already synthetic; everything
    # else in the row is whatever the live session held. `model` in particular
    # is free-form and can name a locally configured provider, so the remaining
    # string columns are sanitized rather than copied.
    for column, cell in list(session.items()):
        if column in SESSION_VERBATIM_COLUMNS or not isinstance(cell, str):
            continue
        session[column] = sanitizer.sanitize(cell, column)
    placeholders = ",".join("?" for _ in session_columns)
    out.execute(f"INSERT INTO session ({','.join(session_columns)}) VALUES ({placeholders})", [session[c] for c in session_columns])

    message_count = 0
    message_ids: dict[str, str] = {}
    if table_exists(source, "message") and table_exists(out, "message"):
        message_columns = [r[1] for r in source.execute("PRAGMA table_info(message)")]
        for row in source.execute("SELECT * FROM message WHERE session_id=? ORDER BY time_created,id LIMIT 48", (old_session_id,)):
            message = dict(zip(message_columns, row))
            fake_message_id = f"msg_fixture_{message_count + 1:03d}"
            message_ids[message["id"]] = fake_message_id
            message["id"] = fake_message_id
            message["session_id"] = fake_session_id
            try:
                message["data"] = json.dumps(sanitizer.sanitize(json.loads(message["data"])), separators=(",", ":"))
            except (json.JSONDecodeError, TypeError):
                message["data"] = "{}"
            placeholders = ",".join("?" for _ in message_columns)
            out.execute(f"INSERT INTO message ({','.join(message_columns)}) VALUES ({placeholders})", [message[c] for c in message_columns])
            message_count += 1

    part_count = 0
    if table_exists(source, "part") and table_exists(out, "part") and message_ids:
        part_columns = [r[1] for r in source.execute("PRAGMA table_info(part)")]
        for old_message_id, fake_message_id in message_ids.items():
            for row in source.execute("SELECT * FROM part WHERE message_id=? ORDER BY time_created,id LIMIT 8", (old_message_id,)):
                part = dict(zip(part_columns, row))
                part["id"] = f"part_fixture_{part_count + 1:03d}"
                part["message_id"] = fake_message_id
                part["session_id"] = fake_session_id
                try:
                    part["data"] = json.dumps(sanitizer.sanitize(json.loads(part["data"])), separators=(",", ":"))
                except (json.JSONDecodeError, TypeError):
                    part["data"] = "{}"
                placeholders = ",".join("?" for _ in part_columns)
                out.execute(f"INSERT INTO part ({','.join(part_columns)}) VALUES ({placeholders})", [part[c] for c in part_columns])
                part_count += 1

    todo_count = 0
    if table_exists(source, "todo") and table_exists(out, "todo"):
        todo_columns = [r[1] for r in source.execute("PRAGMA table_info(todo)")]
        for row in source.execute("SELECT * FROM todo WHERE session_id=? ORDER BY position LIMIT 3", (old_session_id,)):
            todo = dict(zip(todo_columns, row))
            todo["session_id"] = fake_session_id
            todo["content"] = "Validate the synthetic fixture"
            placeholders = ",".join("?" for _ in todo_columns)
            out.execute(f"INSERT INTO todo ({','.join(todo_columns)}) VALUES ({placeholders})", [todo[c] for c in todo_columns])
            todo_count += 1

    out.commit()
    out.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    out.close()
    return {"sessions": 1, "messages": message_count, "parts": part_count, "todos": todo_count}


def build_opencode(root: Path, output: Path, sanitizer: Sanitizer, sources: set[Path]) -> dict[str, Any]:
    db_path = root / "opencode.db"
    if not db_path.is_file():
        raise RuntimeError("OpenCode opencode.db is unavailable")
    sources.add(db_path)
    source = sqlite_ro(db_path)
    source.row_factory = sqlite3.Row
    columns = [r[1] for r in source.execute("PRAGMA table_info(session)")]
    required = {"id", "model", "cost", "tokens_input", "tokens_output", "tokens_reasoning", "tokens_cache_read", "tokens_cache_write", "time_created", "time_updated", "directory"}
    if not required.issubset(columns):
        raise RuntimeError("OpenCode session schema lacks required aggregate columns")
    select = "SELECT * FROM session WHERE tokens_input+tokens_output+tokens_reasoning+tokens_cache_read+tokens_cache_write > 0 ORDER BY time_updated DESC LIMIT 1"
    session_row = source.execute(select).fetchone()
    if session_row is None:
        raise RuntimeError("OpenCode has no representative usage session")
    create_opencode_v1(output / "opencode/sqlite-v1/opencode.db", session_row, columns, sanitizer)

    v2_row = source.execute(
        "SELECT s.* FROM session s WHERE EXISTS (SELECT 1 FROM message m WHERE m.session_id=s.id "
        "AND (json_extract(m.data,'$.variant') IS NOT NULL OR json_extract(m.data,'$.model.variant') IS NOT NULL)) "
        "AND s.tokens_input+s.tokens_output+s.tokens_reasoning+s.tokens_cache_read+s.tokens_cache_write > 0 "
        "ORDER BY s.time_updated DESC LIMIT 1"
    ).fetchone()
    if v2_row is None:
        v2_row = source.execute(
            "SELECT s.* FROM session s WHERE EXISTS (SELECT 1 FROM message m WHERE m.session_id=s.id) "
            "AND s.tokens_input+s.tokens_output+s.tokens_reasoning+s.tokens_cache_read+s.tokens_cache_write > 0 "
            "ORDER BY s.time_updated DESC LIMIT 1"
        ).fetchone() or session_row
    v2_counts = create_opencode_v2(output / "opencode/sqlite-v2/opencode.db", source, v2_row, columns, sanitizer)
    source.close()
    return {"sqlite-v1": {"sessions": 1, "shape": "aggregate"}, "sqlite-v2": v2_counts}


def build_grok(root: Path, output: Path, sanitizer: Sanitizer, sources: set[Path]) -> dict[str, Any]:
    summaries = sorted((root / "sessions").rglob("summary.json"))
    if not summaries:
        raise RuntimeError("Grok session summaries are unavailable")
    summary_path = summaries[0]
    session_dir = summary_path.parent
    summary = json.loads(summary_path.read_text(encoding="utf-8"))
    old_session_id = str(summary.get("info", {}).get("id") or session_dir.name)
    fake_session_id = "ses_fixture_grok_001"
    target = output / "grok/basic/sessions" / fake_session_id
    counts: dict[str, int] = {}

    for name in ["summary.json", "signals.json"]:
        path = session_dir / name
        if not path.is_file():
            continue
        sources.add(path)
        value = json.loads(path.read_text(encoding="utf-8"))
        sanitized = sanitizer.sanitize(value)
        if name == "summary.json" and isinstance(sanitized, dict):
            sanitized.setdefault("info", {})["id"] = fake_session_id
            sanitized["git_root_dir"] = "/workspace/sample-project"
            sanitized["grok_home"] = "/workspace/.grok"
            sanitized["head_branch"] = "fixture/main"
            sanitized["head_commit"] = "0123456789abcdef0123456789abcdef01234567"
            sanitized["session_summary"] = "Implement fixture parser and validate cloud ingestion."
        write_json(target / name, sanitized)
        counts[name] = 1

    for name in ["chat_history.jsonl", "updates.jsonl", "events.jsonl"]:
        path = session_dir / name
        if not path.is_file():
            continue
        sources.add(path)
        records = [sanitizer.sanitize(value) for _, value in valid_jsonl(path)]
        counts[name] = write_jsonl(target / name, records[:40])

    unified = root / "logs/unified.jsonl"
    unified_records = []
    if unified.is_file():
        sources.add(unified)
        for _, value in valid_jsonl(unified):
            if str(value.get("sid", "")) != old_session_id:
                continue
            sanitized = sanitizer.sanitize(value)
            sanitized["sid"] = fake_session_id
            unified_records.append(sanitized)
            if len(unified_records) >= 40:
                break
    counts["unified.jsonl"] = write_jsonl(output / "grok/basic/logs/unified.jsonl", unified_records)
    return {"basic": counts}


# --- Antigravity -------------------------------------------------------------
#
# Antigravity keeps one SQLite file per conversation, with its records as
# protobuf blobs. Every blob is parsed and re-serialized: it must round-trip
# byte-exactly first, so the tree shape, field numbers, varints, and fixed-width
# fields stay as they were, and then every string leaf is rewritten. Numbers,
# timestamps, and enums are varints and stay; long numeric ids such as the
# `sessionID` value are strings and are replaced.

ANTIGRAVITY_PLANNER_RESPONSE = 15
ANTIGRAVITY_CHECKPOINT = 23
# Metadata keys and literals that identify nothing.
ANTIGRAVITY_KEEP_STRINGS = {
    "", "main", "true", "false", "sessionID", "model_enum", "trajectory_id", "request_id",
    "cascade_id", "root_cascade_id", "last_step_index", "last_execution_id",
    "used_claude", "used_claude_conservative", "used_non_gemini_model",
}
# Built-in sub-agent names. A custom agent's name is the user's text.
ANTIGRAVITY_AGENT_NAMES = {"research"}
ANTIGRAVITY_COVERAGE = [
    "gemini-thinking", "claude-vertex", "gpt-oss", "checkpoint",
    "retried-request", "planner-without-usage", "subagent",
]
# Strings the export writes itself: the synthetic stand-ins, the adapter's
# fallback model name, and expected.json's schema, keys and scenario names.
ANTIGRAVITY_WRITTEN_STRINGS = {
    "file:///workspace/sample-project", "https://example.invalid/acme/sample-project.git",
    "acme/sample-project", "fixture/main", "fixture-agent", "text_fixture_",
    "00000000-0000-4000-8000-", "antigravity-model-", "statsai-antigravity-expected.v1",
    "schema", "covers", "unreadable_history_files", "conversations", "planner",
    "checkpoint", "planner_without_usage", "models", "parent", "agent", "branch",
    "calls", "input", "cache_write", "cache_read", "thinking", "response",
    "gemini-thinking", "claude-vertex", "gpt-oss", "retried-request",
    "planner-without-usage", "subagent", "label-elsewhere",
}
# Steps a conversation named elsewhere may add, with its namers, to the fixtures.
ANTIGRAVITY_LABEL_ELSEWHERE_STEP_BUDGET = 120
ANTIGRAVITY_FAKE_URI = "file:///workspace/sample-project"
ANTIGRAVITY_FAKE_REMOTE = "https://example.invalid/acme/sample-project.git"
ANTIGRAVITY_FAKE_SLUG = "acme/sample-project"
ANTIGRAVITY_SYNTHETIC_MARKER = "STATSAI_SYNTHETIC_STORE"
UUID_RE = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
# Names a request was served under or selected with, such as `gemini-3-flash-a`
# or `gpt-oss-120b-medium`. Only names seen in those two places are kept; the
# same shape elsewhere can be an internal codename.
ANTIGRAVITY_MODEL_NAME_RE = re.compile(r"^(?:gemini|claude|gpt)-[A-Za-z0-9._\-]*[0-9][A-Za-z0-9._\-]*$")


def pb_varint(data: bytes, index: int) -> tuple[int, int]:
    value = 0
    for shift in range(0, 64, 7):
        byte = data[index]
        index += 1
        value |= (byte & 0x7F) << shift
        if not byte & 0x80:
            return value, index
    raise ValueError("varint too long")


def pb_parse(data: bytes) -> list[tuple[int, int, Any]] | None:
    """(field, wire type, value) for each field, or None if `data` is not a message."""
    fields = []
    index = 0
    try:
        while index < len(data):
            key, index = pb_varint(data, index)
            field, wire_type = key >> 3, key & 7
            if field == 0:
                return None
            if wire_type == 0:
                value, index = pb_varint(data, index)
            elif wire_type in (1, 5):
                width = 8 if wire_type == 1 else 4
                if index + width > len(data):
                    return None
                value, index = data[index:index + width], index + width
            elif wire_type == 2:
                length, index = pb_varint(data, index)
                if index + length > len(data):
                    return None
                value, index = data[index:index + length], index + length
            else:
                return None
            fields.append((field, wire_type, value))
    except (IndexError, ValueError):
        return None
    return fields


def pb_encode_varint(value: int) -> bytes:
    out = bytearray()
    while True:
        byte = value & 0x7F
        value >>= 7
        if value:
            out.append(byte | 0x80)
        else:
            out.append(byte)
            return bytes(out)


def pb_text(value: bytes) -> str | None:
    """`value` as text, when it is UTF-8 with no control characters but whitespace."""
    try:
        text = value.decode("utf-8")
    except UnicodeDecodeError:
        return None
    if any(ord(ch) < 32 and ch not in "\n\r\t" for ch in text):
        return None
    return text


def pb_sub(data: bytes, number: int) -> list[bytes]:
    return [value for field, wire_type, value in (pb_parse(data) or []) if field == number and wire_type == 2]


def pb_ints(data: bytes) -> dict[int, int]:
    return {field: value for field, wire_type, value in (pb_parse(data) or []) if wire_type == 0}


def pb_string(data: bytes, number: int) -> str | None:
    values = pb_sub(data, number)
    text = pb_text(values[-1]) if values else None
    return text.strip() or None if text is not None else None


class AntigravityRewriter:
    """Rewrites every string in Antigravity blobs and text columns.

    Ids map to fake ids consistently across files, so a parent link and the
    conversation id in each step still hold. Model names are kept only when
    they name a request: the request settings also carry internal codenames.
    """

    def __init__(self, sanitizer: Sanitizer, keep_models: set[str]) -> None:
        self.sanitizer = sanitizer
        self.keep_models = keep_models
        self.uuids: dict[str, str] = {}
        self.numbers: dict[str, str] = {}
        # Everything the output may say on purpose, in lower case: what is
        # kept, the synthetic stand-ins, and expected.json's own vocabulary.
        self.written = {
            text.lower()
            for text in ANTIGRAVITY_KEEP_STRINGS | ANTIGRAVITY_AGENT_NAMES | keep_models | ANTIGRAVITY_WRITTEN_STRINGS
        }

    def fake_uuid(self, value: str) -> str:
        if value not in self.uuids:
            self.uuids[value] = f"00000000-0000-4000-8000-{len(self.uuids) + 1:012d}"
        return self.uuids[value]

    def fake_number(self, value: str) -> str:
        digits = value.lstrip("-")
        if value not in self.numbers:
            seed = str(len(self.numbers) + 1)
            self.numbers[value] = ("-" if value.startswith("-") else "") + ("9" * len(digits))[: len(digits) - len(seed)] + seed
        return self.numbers[value]

    def string(self, text: str, table: str, path: tuple[int, ...]) -> str:
        if text in ANTIGRAVITY_KEEP_STRINGS or text in self.keep_models:
            return text
        if table == "trajectory_metadata_blob":
            if path[:2] == (1, 3):
                return ANTIGRAVITY_FAKE_REMOTE if looks_like_git_remote(text) else ANTIGRAVITY_FAKE_SLUG
            if path == (1, 4):
                return "fixture/main"
            if path in {(4, 1), (8, 1)}:
                return text if text in ANTIGRAVITY_AGENT_NAMES else "fixture-agent"
        if UUID_RE.fullmatch(text):
            return self.fake_uuid(text)
        if ISO_DATE_RE.fullmatch(text.strip()):
            return text
        if re.fullmatch(r"-?\d+", text):
            return text if len(text.lstrip("-")) <= 6 else self.fake_number(text)
        if text.startswith("file://"):
            self.remember(text)
            return ANTIGRAVITY_FAKE_URI
        if text.startswith('["file://'):
            try:
                return json.dumps([ANTIGRAVITY_FAKE_URI for _ in json.loads(text)])
            except ValueError:
                pass
        self.remember(text)
        return self.sanitizer.fake_id("text", text)

    def remember(self, text: str) -> None:
        """Forbids a rewritten original from appearing anywhere in the output.

        Short originals are too likely to occur by chance to check. So is one
        the output contains on purpose: `ANTIGRAVITY` in executor metadata is
        rewritten, yet expected.json names unlabeled models
        `antigravity-model-<id>`, and a settings codename such as
        `gemini-3-flash` is rewritten while the served name `gemini-3-flash-a`
        stays. `validate_antigravity_strings` still checks every string left.
        """
        lowered = text.lower()
        if len(text) >= 8 and not any(lowered in written for written in self.written):
            self.sanitizer.forbidden_tokens.add(text)

    def rebuild(self, data: bytes, table: str | None, path: tuple[int, ...] = ()) -> bytes | None:
        """Re-serializes `data`; with `table` None, unchanged (the round trip)."""
        fields = pb_parse(data)
        if fields is None or len(path) > 40:
            return None
        out = bytearray()
        for field, wire_type, value in fields:
            out += pb_encode_varint((field << 3) | wire_type)
            if wire_type == 0:
                out += pb_encode_varint(value)
            elif wire_type in (1, 5):
                out += value
            else:
                inner = self.rebuild_value(value, table, path + (field,))
                out += pb_encode_varint(len(inner)) + inner
        return bytes(out)

    def rebuild_value(self, value: bytes, table: str | None, path: tuple[int, ...]) -> bytes:
        text = pb_text(value)
        if text is not None and (text.isprintable() or pb_parse(value) is None):
            return value if table is None else self.string(text, table, path).encode("utf-8")
        nested = self.rebuild(value, table, path) if value else None
        if nested is not None:
            return nested
        # Opaque bytes: the length class stays, the content does not.
        return value if table is None else b"\0" * min(len(value), 16)

    def blob(self, value: bytes, table: str) -> bytes:
        if self.rebuild(value, None) != value:
            raise RuntimeError("an Antigravity blob does not round-trip; refusing to rewrite it")
        rewritten = self.rebuild(value, table)
        return rewritten if rewritten is not None else b"\0" * min(len(value), 16)

    def cell(self, value: Any, table: str) -> Any:
        if isinstance(value, bytes) and value:
            return self.blob(value, table)
        if isinstance(value, str):
            return self.string(value, table, ())
        return value


def looks_like_git_remote(value: str) -> bool:
    if "://" in value:
        return True
    match = re.match(r"^[^@\s]+@([^:/\s]+):\S+$", value)
    return match is not None


def sqlite_live_ro(path: Path) -> sqlite3.Connection:
    """Read-only, without `immutable`: Antigravity may be writing the file."""
    quoted = str(path).replace("?", "%3f").replace("#", "%23")
    connection = sqlite3.connect(f"file:{quoted}?mode=ro", uri=True)
    connection.execute("PRAGMA query_only=ON")
    return connection


def antigravity_usage(usage: bytes) -> dict[str, int]:
    """`ModelUsageStats` as the adapter counts it: output is the response
    alone (field 10), or the total output (3) less thinking (9) without it."""
    values = pb_ints(usage)
    thinking = values.get(9, 0)
    response = values.get(10, 0) or max(values.get(3, 0) - thinking, 0)
    return {
        "input": values.get(2, 0), "cache_write": values.get(4, 0),
        "cache_read": values.get(5, 0), "thinking": thinking, "response": response,
    }


def antigravity_calls(connection: sqlite3.Connection) -> tuple[list[dict[str, Any]], int]:
    """Every model request with usage, and the planner steps without any."""
    calls = []
    without_usage = 0
    rows = connection.execute(
        "SELECT idx, step_type, metadata FROM steps WHERE step_type IN (?, ?) ORDER BY idx",
        (ANTIGRAVITY_PLANNER_RESPONSE, ANTIGRAVITY_CHECKPOINT),
    )
    for _, step_type, metadata in rows:
        metadata = metadata or b""
        if pb_parse(metadata) is None:
            continue
        if step_type == ANTIGRAVITY_PLANNER_RESPONSE:
            usages = pb_sub(metadata, 9)
            if not usages or pb_parse(usages[-1]) is None:
                without_usage += 1
                continue
            calls.append({"kind": "planner", "usage": usages[-1]})
            continue
        retries = pb_sub(metadata, 28)
        for retry in retries:
            for usage in pb_sub(retry, 2)[-1:]:
                calls.append({"kind": "checkpoint", "usage": usage})
        if not retries:
            for usage in pb_sub(metadata, 9)[-1:]:
                calls.append({"kind": "checkpoint", "usage": usage})
    return calls, without_usage


def antigravity_gen_names(connection: sqlite3.Connection) -> tuple[list[tuple[int, str]], list[tuple[int, str]], int]:
    """(model id, served name) and (model id, selected label) pairs, and the
    number of requests that were retried. Only fields 1 and 3 are opened."""
    served, labels, retried = [], [], 0
    for (data,) in connection.execute("SELECT data FROM gen_metadata"):
        data = data or b""
        requests = pb_sub(data, 1)
        for request in requests:
            if len(pb_sub(request, 17)) > 1:
                retried += 1
            model = pb_ints(request).get(3)
            name = pb_string(request, 19)
            if model and name:
                served.append((model, name))
        for settings in pb_sub(data, 3):
            model = pb_ints(settings).get(1) or next((pb_ints(r).get(3) for r in requests if pb_ints(r).get(3)), None)
            label = pb_string(settings, 28)
            if model and label:
                labels.append((model, label))
    return served, labels, retried


def antigravity_trajectory(connection: sqlite3.Connection) -> dict[str, Any]:
    row = connection.execute(
        "SELECT data FROM trajectory_metadata_blob ORDER BY (id = 'main') DESC, rowid LIMIT 1"
    ).fetchone()
    data = (row[0] if row else b"") or b""
    workspace = (pb_sub(data, 1) or [b""])[-1]
    agent = None
    for number in (8, 4):
        for message in pb_sub(data, number)[-1:]:
            agent = agent or pb_string(message, 1)
    return {
        "parent": pb_string(data, 5),
        "agent": agent,
        "branch": pb_string(workspace, 4),
        "workspace": pb_string(workspace, 1) or pb_string(data, 7) or pb_string(workspace, 2),
    }


def antigravity_conversation_id(connection: sqlite3.Connection, path: Path) -> str:
    ids = [row[0] for row in connection.execute(
        "SELECT DISTINCT cascade_id FROM trajectory_meta WHERE cascade_id IS NOT NULL AND cascade_id <> '' LIMIT 2"
    )]
    return ids[0].strip() if len(ids) == 1 else path.stem


def antigravity_features(connection: sqlite3.Connection) -> set[str]:
    calls, without_usage = antigravity_calls(connection)
    _, _, retried = antigravity_gen_names(connection)
    features = set()
    for call in calls:
        values = pb_ints(call["usage"])
        if call["kind"] == "checkpoint":
            features.add("checkpoint")
            continue
        provider = values.get(6)
        if provider == 24 and values.get(9, 0) > 0:
            features.add("gemini-thinking")
        elif provider == 26:
            features.add("claude-vertex")
        elif provider == 31:
            features.add("gpt-oss")
    if without_usage:
        features.add("planner-without-usage")
    if retried:
        features.add("retried-request")
    if antigravity_trajectory(connection)["parent"]:
        features.add("subagent")
    return features


def build_antigravity(root: Path, output: Path, sanitizer: Sanitizer, sources: set[Path]) -> dict[str, Any]:
    conversations = root / "conversations"
    db_paths = sorted(path for path in conversations.glob("*.db") if path.is_file())
    if not db_paths:
        raise RuntimeError("Antigravity conversation databases are unavailable")
    staging = Path(tempfile.mkdtemp(prefix="antigravity-copies-"))
    try:
        # Consistent private copies first: the live files may be written to
        # while they are read.
        copies: dict[Path, Path] = {}
        info: dict[Path, dict[str, Any]] = {}
        for path in db_paths:
            copy = staging / path.name
            source = sqlite_live_ro(path)
            target = sqlite3.connect(copy)
            source.backup(target)
            source.close()
            target.close()
            connection = sqlite3.connect(copy)
            _, labels, _ = antigravity_gen_names(connection)
            calls, _ = antigravity_calls(connection)
            info[path] = {
                "id": antigravity_conversation_id(connection, path),
                "features": antigravity_features(connection),
                "parent": antigravity_trajectory(connection)["parent"],
                "steps": connection.execute("SELECT count(*) FROM steps").fetchone()[0],
                "labeled": {model for model, _ in labels},
                "models": {pb_ints(call["usage"]).get(1) for call in calls} - {None, 0},
            }
            connection.close()
            copies[path] = copy

        # A conversation that switched models names only its latest one; the
        # adapter names the others from conversations that selected them.
        for path, value in info.items():
            elsewhere = {
                model for model in value["models"] - value["labeled"]
                if any(model in other["labeled"] for other_path, other in info.items() if other_path != path)
            }
            if elsewhere:
                value["features"].add("label-elsewhere")
            value["elsewhere"] = elsewhere

        # The fewest, smallest conversations that cover every scenario. A
        # sub-agent comes with its parent.
        by_id = {value["id"]: path for path, value in info.items()}
        selected: list[Path] = []
        covered: set[str] = set()

        def select(path: Path) -> None:
            if path not in selected:
                selected.append(path)
                covered.update(info[path]["features"] - {"label-elsewhere"})

        subagents = sorted(
            (path for path, value in info.items() if value["parent"] in by_id),
            key=lambda path: (info[path]["steps"], path.name),
        )
        if subagents:
            select(subagents[0])
            select(by_id[info[subagents[0]]["parent"]])
        for path in sorted(db_paths, key=lambda path: (info[path]["steps"], path.name)):
            if info[path]["features"] - covered - {"subagent", "label-elsewhere"}:
                select(path)

        # Optional: a conversation whose model is named only elsewhere, with
        # the conversations that name it, when that adds little.
        best: tuple[int, list[Path]] | None = None
        for path in sorted(db_paths, key=lambda path: (info[path]["steps"], path.name)):
            if not info[path]["elsewhere"]:
                continue
            extra = [] if path in selected else [path]
            for model in sorted(info[path]["elsewhere"]):
                if any(model in info[chosen]["labeled"] for chosen in selected + extra):
                    continue
                extra.append(min(
                    (other for other in db_paths if other != path and model in info[other]["labeled"]),
                    key=lambda other: (info[other]["steps"], other.name),
                ))
            cost = sum(info[chosen]["steps"] for chosen in extra)
            if best is None or cost < best[0]:
                best = (cost, extra)
        if best is not None and best[0] <= ANTIGRAVITY_LABEL_ELSEWHERE_STEP_BUDGET:
            for path in best[1]:
                select(path)
        missing = [scenario for scenario in ANTIGRAVITY_COVERAGE if scenario not in covered]
        if missing:
            raise RuntimeError("Antigravity store lacks fixture scenarios: " + ", ".join(missing))

        keep_models: set[str] = set()
        for path in selected:
            connection = sqlite3.connect(copies[path])
            served, labels, _ = antigravity_gen_names(connection)
            connection.close()
            keep_models.update(
                name for _, name in served + labels if ANTIGRAVITY_MODEL_NAME_RE.fullmatch(name)
            )
        rewriter = AntigravityRewriter(sanitizer, keep_models)
        target_dir = output / "antigravity/store/conversations"
        target_dir.mkdir(parents=True, exist_ok=True)
        for path in selected:
            sources.add(path)
            connection = sqlite3.connect(copies[path])
            connection.execute("PRAGMA journal_mode=DELETE")
            tables = [row[0] for row in connection.execute("SELECT name FROM sqlite_master WHERE type='table'")]
            for table in tables:
                columns = [row[1] for row in connection.execute(f"PRAGMA table_info(`{table}`)")]
                rows = connection.execute(f"SELECT * FROM `{table}`").fetchall()
                connection.execute(f"DELETE FROM `{table}`")
                marks = ", ".join("?" for _ in columns)
                names = ", ".join(f"`{column}`" for column in columns)
                for row in rows:
                    cells = [rewriter.cell(cell, table) for cell in row]
                    if table == "gen_metadata" and "size" in columns and "data" in columns:
                        data = cells[columns.index("data")]
                        cells[columns.index("size")] = len(data) if isinstance(data, bytes) else 0
                    connection.execute(f"INSERT INTO `{table}` ({names}) VALUES ({marks})", cells)
            connection.commit()
            # A fresh file: no page of the original survives as free space.
            connection.execute("PRAGMA page_size=1024")
            connection.execute("VACUUM")
            connection.close()
            fake_id = rewriter.fake_uuid(info[path]["id"])
            shutil.copyfile(copies[path], target_dir / f"{fake_id}.db")

        # A stand-in for a legacy encrypted conversation: invented bytes, never
        # a real file, even an encrypted one.
        encrypted = target_dir / f"{rewriter.fake_uuid('encrypted-legacy-conversation')}.pb"
        stream = b""
        while len(stream) < 512:
            stream += hashlib.sha256(b"statsai-antigravity-fixture" + len(stream).to_bytes(4, "big")).digest()
        encrypted.write_bytes(stream[:512])

        expected = antigravity_expected(output / "antigravity/store")
        write_json(output / "antigravity/expected.json", expected)
        return {
            "conversations": len(selected),
            "encrypted": 1,
            "covers": expected["covers"],
            "derived_from_local_store": not (root / ANTIGRAVITY_SYNTHETIC_MARKER).exists(),
        }
    finally:
        shutil.rmtree(staging, ignore_errors=True)


def most_common_name(names: dict[str, int]) -> str | None:
    if not names:
        return None
    return min(names.items(), key=lambda item: (-item[1], item[0]))[0]


def antigravity_expected(store: Path) -> dict[str, Any]:
    """What the adapter must read from the exported store, from this decoder."""
    conversations = {}
    global_served: dict[int, dict[str, int]] = defaultdict(lambda: defaultdict(int))
    global_labels: dict[int, dict[str, int]] = defaultdict(lambda: defaultdict(int))
    per_file = []
    covers: set[str] = set()
    for path in sorted((store / "conversations").glob("*.db")):
        connection = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
        served, labels, _ = antigravity_gen_names(connection)
        local_served: dict[int, dict[str, int]] = defaultdict(lambda: defaultdict(int))
        local_labels: dict[int, dict[str, int]] = defaultdict(lambda: defaultdict(int))
        for model, name in served:
            local_served[model][name] += 1
            global_served[model][name] += 1
        for model, name in labels:
            local_labels[model][name] += 1
            global_labels[model][name] += 1
        calls, without_usage = antigravity_calls(connection)
        covers.update(antigravity_features(connection))
        per_file.append((
            antigravity_conversation_id(connection, path), calls, without_usage,
            antigravity_trajectory(connection), local_served, local_labels,
        ))
        connection.close()
    for conversation_id, calls, without_usage, trajectory, local_served, local_labels in per_file:
        totals = {kind: {"calls": 0, "input": 0, "cache_write": 0, "cache_read": 0, "thinking": 0, "response": 0} for kind in ("planner", "checkpoint")}
        models: dict[str, int] = defaultdict(int)
        for call in calls:
            usage = antigravity_usage(call["usage"])
            if sum(usage.values()) == 0:
                continue
            bucket = totals[call["kind"]]
            bucket["calls"] += 1
            for key, value in usage.items():
                bucket[key] += value
            model = pb_ints(call["usage"]).get(1)
            if not model:
                models["|"] += 1
                continue
            label = most_common_name(local_labels.get(model, {})) or most_common_name(global_labels.get(model, {}))
            served = most_common_name(local_served.get(model, {})) or most_common_name(global_served.get(model, {}))
            name = label or served or f"antigravity-model-{model}"
            models[f"{name}|{served or ''}"] += 1
        conversations[conversation_id] = {
            **totals,
            "planner_without_usage": without_usage,
            "models": dict(sorted(models.items())),
            "parent": trajectory["parent"],
            "agent": trajectory["agent"] if trajectory["parent"] else None,
            "branch": trajectory["branch"],
        }
    unreadable = sum(
        1 for path in (store / "conversations").glob("*.pb") if not path.with_suffix(".db").exists()
    )
    for conversation_id, calls, _, _, _, local_labels in per_file:
        for call in calls:
            model = pb_ints(call["usage"]).get(1)
            if model and model not in local_labels and model in global_labels:
                covers.add("label-elsewhere")
    return {
        "schema": "statsai-antigravity-expected.v1",
        "covers": sorted(covers),
        "unreadable_history_files": unreadable,
        "conversations": dict(sorted(conversations.items())),
    }


def antigravity_string_leaves(data: bytes, depth: int = 0) -> Iterable[str]:
    for _, wire_type, value in pb_parse(data) or []:
        if wire_type != 2:
            continue
        text = pb_text(value)
        if text is not None and (text.isprintable() or pb_parse(value) is None):
            yield text
        elif depth < 40:
            yield from antigravity_string_leaves(value, depth + 1)


ANTIGRAVITY_SAFE_STRING_RE = re.compile(
    r"^(?:text_fixture_\d{3,}|fixture/main|fixture-agent|"
    + re.escape(ANTIGRAVITY_FAKE_URI) + "|" + re.escape(ANTIGRAVITY_FAKE_REMOTE) + "|"
    + re.escape(ANTIGRAVITY_FAKE_SLUG) + r"|00000000-0000-4000-8000-\d{12}|-?\d+|"
    r"\d{4}-\d{2}-\d{2}(?:[T ][0-9:.+\-Z]+)?)$"
)


def validate_antigravity_strings(root: Path) -> None:
    """Every string left in the Antigravity fixtures is a known-safe form.

    A model name passes only where requests are named (`1.19`, `3.28`); the
    same name anywhere else, such as a codename in the request settings, has
    to have been rewritten like any other string.
    """
    paths = sorted((root / "antigravity").rglob("*.db"))
    model_names: set[str] = set()
    for path in paths:
        connection = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
        served, labels, _ = antigravity_gen_names(connection)
        connection.close()
        model_names.update(name for _, name in served + labels if ANTIGRAVITY_MODEL_NAME_RE.fullmatch(name))
    allowed = ANTIGRAVITY_KEEP_STRINGS | ANTIGRAVITY_AGENT_NAMES | model_names
    stray: set[str] = set()
    for path in paths:
        connection = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
        for (table,) in connection.execute("SELECT name FROM sqlite_master WHERE type='table'").fetchall():
            for row in connection.execute(f"SELECT * FROM `{table}`"):
                for cell in row:
                    if isinstance(cell, bytes):
                        strings = list(antigravity_string_leaves(cell))
                    else:
                        strings = [cell] if isinstance(cell, str) else []
                    for text in strings:
                        if text in allowed or ANTIGRAVITY_SAFE_STRING_RE.fullmatch(text):
                            continue
                        if text.startswith("[") and text == json.dumps([ANTIGRAVITY_FAKE_URI] * text.count(ANTIGRAVITY_FAKE_URI)):
                            continue
                        stray.add(text[:40])
        connection.close()
    if stray:
        raise RuntimeError(f"Antigravity fixtures keep {len(stray)} unexpected strings")


CURSOR_CSV_HEADER = (
    "Date,Cloud Agent ID,Automation ID,Kind,Model,Max Mode,"
    "Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,"
    "Output Tokens,Total Tokens,Cost"
)

# Cursor keeps no local usage trace, so these are authored rather than derived:
# every value is invented, including the token counts. They reproduce the
# dashboard export's header, quoting, and enum vocabulary.
CURSOR_FIXTURES: dict[str, str] = {
    "cursor/basic/usage-events-basic.csv": CURSOR_CSV_HEADER + """
"2026-01-05T10:30:00.000Z","bc-fixture-0001","","Included","cursor-grok-4.6-high-fast","No","1000","2000","3000","4000","10000","Included"
"2026-01-05T10:20:00.000Z","bc-fixture-0001","","Included","cursor-grok-4.6-high","No","1000","2000","3000","4000","10000","Included"
"2026-01-05T09:15:00.000Z","bc-fixture-0002","au-fixture-0001","Included","grok-bot-automation","No","500","1500","2500","3500","8000","Included"
"2026-01-04T18:45:00.000Z","","","Included","claude-4.5-sonnet","No","100","200","300","400","1000","Included"
"2026-01-04T17:05:00.000Z","","","Included","claude-fable-5-1-thinking-max","Yes","0","1000","9000","500","10500","Included"
"2026-01-04T08:00:00.000Z","","","Included","gemini-3.1-pro","No","0","600","0","400","1000","Free"
""",
    # Usage-based rows past the monthly quota carry a real charge. No export
    # available today contains one, so the case exists only here.
    "cursor/usage-based/usage-events-charged.csv": CURSOR_CSV_HEADER + """
"2026-01-06T12:00:00.000Z","bc-fixture-0010","","Usage-based","cursor-grok-4.6-high","No","1000","2000","3000","4000","10000","$0.1234"
"2026-01-06T11:30:00.000Z","","","Usage-based","claude-4.5-sonnet","No","100","200","300","400","1000","2.50"
"2026-01-06T11:00:00.000Z","","","Included","cursor-grok-4.6-medium","No","100","200","300","400","1000","Included"
"2026-01-06T10:30:00.000Z","","","Free","cursor-grok-4.6-low","No","100","200","300","400","1000","Free"
"2026-01-06T10:00:00.000Z","","","Trial","cursor-grok-4.5-high","No","100","200","300","400","1000","Promotional"
""",
    # Two exports of overlapping ranges: the grok row grew between them, and
    # the later export adds a row the earlier one predates.
    "cursor/snapshots/export-early.csv": CURSOR_CSV_HEADER + """
"2026-01-07T09:00:00.000Z","bc-fixture-0020","","Included","cursor-grok-4.6-high","No","1000","2000","3000","4000","10000","Included"
"2026-01-06T20:00:00.000Z","bc-fixture-0021","","Included","composer-2.5-fast","No","500","500","1000","1000","3000","Included"
""",
    "cursor/snapshots/export-late.csv": CURSOR_CSV_HEADER + """
"2026-01-07T11:00:00.000Z","bc-fixture-0022","","Included","cursor-grok-4.6-medium","No","100","200","300","400","1000","Included"
"2026-01-07T09:00:00.000Z","bc-fixture-0020","","Included","cursor-grok-4.6-high","No","4000","8000","12000","16000","40000","Included"
"2026-01-06T20:00:00.000Z","bc-fixture-0021","","Included","composer-2.5-fast","No","500","500","1000","1000","3000","Included"
""",
    # Two rows sharing every immutable field, differing only in tokens, in both
    # orders: Cursor does not keep such a pair in a stable order across exports.
    "cursor/collision/usage-events-collision.csv": CURSOR_CSV_HEADER + """
"2026-01-08T14:00:00.123Z","bc-fixture-0030","au-fixture-0002","Included","cursor-grok-4.6-medium","No","1000","2000","3000","4000","10000","Included"
"2026-01-08T14:00:00.123Z","bc-fixture-0030","au-fixture-0002","Included","cursor-grok-4.6-medium","No","200","400","600","800","2000","Included"
"2026-01-08T13:00:00.000Z","bc-fixture-0031","","Included","cursor-grok-4.6-high","No","100","200","300","400","1000","Included"
""",
    "cursor/collision/usage-events-collision-swapped.csv": CURSOR_CSV_HEADER + """
"2026-01-08T14:00:00.123Z","bc-fixture-0030","au-fixture-0002","Included","cursor-grok-4.6-medium","No","200","400","600","800","2000","Included"
"2026-01-08T14:00:00.123Z","bc-fixture-0030","au-fixture-0002","Included","cursor-grok-4.6-medium","No","1000","2000","3000","4000","10000","Included"
"2026-01-08T13:00:00.000Z","bc-fixture-0031","","Included","cursor-grok-4.6-high","No","100","200","300","400","1000","Included"
""",
    # Blank numeric cells occur in real exports; the rest is deliberate.
    "cursor/malformed/usage-events-malformed.csv": CURSOR_CSV_HEADER + ",Experimental Column" + """
"2026-01-09T10:00:00.000Z","bc-fixture-0040","","Included","cursor-grok-4.6-high","No","1000","2000","3000","4000","10000","Included","future"
"2026-01-09T09:00:00.000Z","","","Included","cursor-grok-4.6-medium","No","","","","","","Included","future"
"not-a-date","","","Included","cursor-grok-4.6-low","No","100","200","300","400","1000","Included","future"
"2026-01-09T08:00:00.000Z","","","Included","","No","100","200","300","400","1000","Included","future"
"2026-01-09T07:00:00.000Z","bc-fixture-0041","","Included","composer-2.5-fast","No","100","200","300","400","1000","Included","future"
""",
    "cursor/malformed/usage-events-legacy-header.csv": """Date,Kind,Model,Max Mode,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens,Total Tokens,Cost
"2026-01-09T12:00:00.000Z","Included","cursor-grok-4.5-high","No","1000","2000","3000","4000","10000","Included"
"2026-01-09T11:00:00.000Z","Included","composer-2.5-fast","No","100","200","300","400","1000","Included"
""",
    "cursor/malformed/usage-events-no-date.csv": """Cloud Agent ID,Kind,Model,Max Mode,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens,Total Tokens,Cost
"bc-fixture-0050","Included","cursor-grok-4.6-high","No","1000","2000","3000","4000","10000","Included"
""",
}


def build_cursor(output: Path) -> dict[str, Any]:
    """Writes the authored Cursor CSV fixtures.

    Unlike every other provider here there is nothing to read: Cursor's local
    cache holds no usage detail, so these files are literals rather than
    sanitized copies, and take no source root.
    """
    for relative, text in CURSOR_FIXTURES.items():
        path = output / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text.lstrip("\n"), encoding="utf-8", newline="\n")
    return {"files": len(CURSOR_FIXTURES), "derived_from_local_store": False}


def fixture_files(root: Path) -> list[Path]:
    return sorted(p for p in root.rglob("*") if p.is_file())


def checksum(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def decoded_byte_payloads(text: str) -> list[str]:
    """Every list-of-bytes inside `text` that decodes to text, decoded."""
    payloads: list[str] = []

    def walk(node: Any) -> None:
        if isinstance(node, list):
            decoded = decode_byte_array(node)
            if decoded is not None:
                payloads.append(decoded)
                return
            for item in node:
                walk(item)
        elif isinstance(node, dict):
            for item in node.values():
                walk(item)
        elif isinstance(node, str):
            candidate = node.strip()
            if candidate.startswith(("{", "[")) and len(candidate) > 20:
                try:
                    walk(json.loads(candidate))
                except (ValueError, TypeError):
                    return

    for line in text.splitlines():
        line = line.strip()
        if not line.startswith(("{", "[")):
            continue
        try:
            walk(json.loads(line))
        except (ValueError, TypeError):
            continue
    return payloads


def validate_privacy(root: Path, forbidden_tokens: set[str]) -> None:
    violations: list[str] = []
    for path in fixture_files(root):
        relative = path.relative_to(root)
        if path.suffix == ".pb":
            # Invented stand-ins for encrypted conversations: random bytes, no text.
            continue
        if path.suffix in {".db", ".sqlite"}:
            blobs = []
            connection = sqlite3.connect(path)
            for table in [r[0] for r in connection.execute("SELECT name FROM sqlite_master WHERE type='table'")]:
                try:
                    for row in connection.execute(f'SELECT * FROM "{table}"'):
                        blobs.extend(str(value) for value in row if isinstance(value, str))
                        # Antigravity keeps its text inside protobuf blobs.
                        for value in row:
                            if isinstance(value, bytes):
                                blobs.extend(antigravity_string_leaves(value))
                except sqlite3.DatabaseError:
                    continue
            connection.close()
            text = "\n".join(blobs)
        else:
            text = path.read_text(encoding="utf-8", errors="replace")
        # The sanitizer and the validator have to see the same bytes. A payload
        # encoded as a list of integers is invisible to every string check
        # below, which is how a home directory and username shipped once
        # already, so anything decodable is appended to the scanned text.
        text = "\n".join([text, *decoded_byte_payloads(text)])
        if SECRET_RE.search(text):
            violations.append(f"{path.relative_to(root)}: secret-like token")
        for match in EMAIL_RE.findall(text):
            if not match.endswith("@example.invalid"):
                violations.append(f"{path.relative_to(root)}: non-fixture email")
        tokens_to_check = {HOME.name, str(HOME)} if relative.as_posix() in {"README.md", "MANIFEST.json"} else forbidden_tokens
        for token in tokens_to_check:
            if token and token.lower() in text.lower():
                violations.append(f"{relative}: local identity/path token")
        if "/Users/" in text or re.search(r"/home/(?!fixture|sample)", text):
            violations.append(f"{path.relative_to(root)}: local absolute path")
    if violations:
        raise RuntimeError("privacy validation failed: " + "; ".join(sorted(set(violations))))


README_TEXT = """# StatsAI cloud fixtures

Generated by `build_fixtures.py` from local provider trace *schemas*. Original
stores are read-only and are checked for size/mtime/inode changes after export.

All prompts, responses, tool arguments/output, projects, paths, git identity,
users, URLs, and provider record IDs are deterministic synthetic values.
Timestamps, numeric usage/cost/latency fields, public model names, enum values,
JSON shapes, and SQLite table layouts are retained.

`malformed-record` and `malformed` each add one intentionally invalid JSONL row.
Hand-authored `activity-*` fixtures cover tool/MCP/skill extraction for Codex,
Claude Code, OpenCode, and Grok Build. They are synthetic; regenerate them with
`write_activity_fixtures.py` rather than `build_fixtures.py`.
`opencode/sqlite-v1` is the supported aggregate-session shape; `sqlite-v2`
uses the current local session/message/part/todo schemas with sanitized rows.

`antigravity/store` holds Antigravity conversation databases whose protobuf
blobs were re-serialized after a byte-exact round trip, with every string
rewritten. Model names survive only where a request is named. The `.pb` file
is invented bytes standing in for an encrypted legacy conversation.
`antigravity/expected.json` is what the export's own decoder reads from those
files; the adapter test must agree with it. `scenarios.antigravity` in
`MANIFEST.json` records whether the export ran over a real store
(`derived_from_local_store`) or over the invented one that
`write_antigravity_store.py` writes, whose values are all made up. Refresh just
this provider with:

```sh
./build_fixtures.py --only antigravity --antigravity-root ~/.gemini/antigravity
```

Quit Antigravity first: the export checks that the store did not change while
it was read.

`cursor/` is the exception: those CSVs are hand-authored rather than derived
from a local store, because Cursor keeps no local usage trace. They reproduce
the dashboard export's header, quoting, and enum values, and every value in
them is invented — including the token counts, which elsewhere are real.

Rerun from this directory:

```sh
./build_fixtures.py
```
"""


def write_readme(root: Path) -> None:
    (root / "README.md").write_text(README_TEXT, encoding="utf-8")


PROVIDER_DIRS = ["claude", "codex", "opencode", "grok", "cursor", "antigravity"]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--output", type=Path, default=None,
        help="default: tests/fixtures below this directory, or this directory with --only",
    )
    parser.add_argument("--claude-root", type=Path, default=DEFAULT_CLAUDE)
    parser.add_argument("--codex-root", type=Path, default=DEFAULT_CODEX)
    parser.add_argument("--opencode-root", type=Path, default=DEFAULT_OPENCODE)
    parser.add_argument("--grok-root", type=Path, default=DEFAULT_GROK)
    parser.add_argument("--antigravity-root", type=Path, default=DEFAULT_ANTIGRAVITY)
    parser.add_argument(
        "--only", choices=PROVIDER_DIRS, action="append",
        help="rebuild only these providers' fixtures, keeping the others and updating MANIFEST.json",
    )
    return parser.parse_args()


def run_builders(args: argparse.Namespace, names: list[str], staging: Path, sanitizer: Sanitizer, sources: set[Path]) -> dict[str, Any]:
    builders: dict[str, Callable[[], dict[str, Any]]] = {
        "claude": lambda: build_claude(args.claude_root.resolve(), staging, sanitizer, sources),
        "codex": lambda: build_codex(args.codex_root.resolve(), staging, sanitizer, sources),
        "opencode": lambda: build_opencode(args.opencode_root.resolve(), staging, sanitizer, sources),
        "grok": lambda: build_grok(args.grok_root.resolve(), staging, sanitizer, sources),
        "cursor": lambda: build_cursor(staging),
        "antigravity": lambda: build_antigravity(args.antigravity_root.resolve(), staging, sanitizer, sources),
    }
    return {name: builders[name]() for name in names}


def manifest_entry(path: Path, root: Path) -> dict[str, Any]:
    return {"path": path.relative_to(root).as_posix(), "bytes": path.stat().st_size, "sha256": checksum(path)}


def main() -> int:
    args = parse_args()
    script_root = Path(__file__).resolve().parent
    only = sorted(set(args.only or []), key=PROVIDER_DIRS.index)
    output = (args.output or (script_root if only else script_root / "tests/fixtures")).resolve()
    roots = {args.claude_root, args.codex_root, args.opencode_root, args.grok_root, args.antigravity_root}
    if output == Path("/") or output == HOME or output in {root.resolve() for root in roots}:
        raise RuntimeError("refusing unsafe output path")
    output.parent.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix="fixtures-staging-", dir=output.parent))
    sources: set[Path] = set()
    sanitizer = Sanitizer()
    try:
        stats = run_builders(args, only or PROVIDER_DIRS, staging, sanitizer, sources)
        snapshot = readonly_snapshot(sources)
        forbidden = {HOME.name, str(HOME), *sanitizer.forbidden_tokens}
        validate_privacy(staging, forbidden)
        if "antigravity" in stats:
            validate_antigravity_strings(staging)
        assert_unchanged(snapshot)
        if only:
            return merge_partial_build(output, staging, only, stats, sources)
        write_readme(staging)
        files = fixture_files(staging)
        manifest = {
            "schema": "statsai-cloud-fixtures.v1",
            "privacy": "synthetic identities/content; real provider shapes and numeric telemetry",
            "original_sources": {"mode": "read-only", "files_read": len(sources), "paths_recorded": False},
            "scenarios": stats,
            "files": [manifest_entry(path, staging) for path in files],
        }
        write_json(staging / "MANIFEST.json", manifest)
        validate_privacy(staging, forbidden)
        assert_unchanged(snapshot)

        backup = None
        if output.exists():
            backup = output.with_name(output.name + ".previous")
            if backup.exists():
                shutil.rmtree(backup)
            output.rename(backup)
        staging.rename(output)
        if backup is not None:
            shutil.rmtree(backup)
        print(json.dumps({"output": str(output), "fixture_files": len(fixture_files(output)), "original_files_read_only": len(sources)}, sort_keys=True))
        return 0
    except Exception:
        shutil.rmtree(staging, ignore_errors=True)
        raise


def merge_partial_build(output: Path, staging: Path, only: list[str], stats: dict[str, Any], sources: set[Path]) -> int:
    """Replaces the rebuilt providers' directories in `output` and their
    entries in its MANIFEST.json, leaving every other fixture as it is."""
    manifest_path = output / "MANIFEST.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8")) if manifest_path.exists() else {
        "schema": "statsai-cloud-fixtures.v1",
        "privacy": "synthetic identities/content; real provider shapes and numeric telemetry",
        "original_sources": {"mode": "read-only", "files_read": 0, "paths_recorded": False},
        "scenarios": {},
        "files": [],
    }
    for name in only:
        target = output / name
        if target.exists():
            shutil.rmtree(target)
        built = staging / name
        if built.exists():
            shutil.copytree(built, target)
    shutil.rmtree(staging, ignore_errors=True)
    write_readme(output)
    rebuilt = tuple(f"{name}/" for name in only)
    files = [entry for entry in manifest.get("files", []) if not entry["path"].startswith(rebuilt) and entry["path"] != "README.md"]
    files.append(manifest_entry(output / "README.md", output))
    for name in only:
        files.extend(manifest_entry(path, output) for path in fixture_files(output / name))
    manifest["files"] = sorted(files, key=lambda entry: entry["path"])
    manifest.setdefault("scenarios", {}).update(stats)
    write_json(manifest_path, manifest)
    print(json.dumps({"output": str(output), "rebuilt": only, "original_files_read_only": len(sources)}, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
