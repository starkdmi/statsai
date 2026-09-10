#!/usr/bin/env python3
"""Hand-authored synthetic activity fixtures (Phase 0). Does not read local stores."""

from __future__ import annotations

import hashlib
import json
import sqlite3
from pathlib import Path

ROOT = Path(__file__).resolve().parent
TS = "2026-01-01T00:00:01.000Z"
MS = 1_767_225_600_000


def write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def jsonl(path: Path, rows: list[object]) -> None:
    lines = []
    for row in rows:
        if isinstance(row, str):
            lines.append(row)
        else:
            lines.append(json.dumps(row, separators=(",", ":")))
    write(path, "\n".join(lines) + "\n")


def codex_native() -> None:
    jsonl(
        ROOT / "codex/activity-native/sessions/2026/01/01/rollout-fixture-activity-native.jsonl",
        [
            {
                "timestamp": TS,
                "type": "session_meta",
                "payload": {
                    "id": "id_fixture_activity_native",
                    "timestamp": TS,
                    "cwd": "/fixture/project",
                    "originator": "codex_cli_rs",
                    "cli_version": "0.0.0",
                },
            },
            {
                "timestamp": TS,
                "type": "turn_context",
                "payload": {"model": "gpt-5.4"},
            },
            {
                "timestamp": TS,
                "type": "event_msg",
                "ordinal": 12,
                "payload": {
                    "type": "item_completed",
                    "thread_id": "thread_fixture_001",
                    "turn_id": "turn_fixture_001",
                    "started_at_ms": MS,
                    "completed_at_ms": MS + 420,
                    "item": {
                        "type": "CommandExecution",
                        "id": "item_fixture_001",
                        "status": "completed",
                        "exit_code": 0,
                        "command": ["bash", "-lc", "cat SKILL.md"],
                        "cwd": "/fixture/project",
                        "parsed_cmd": [
                            {
                                "type": "read",
                                "cmd": "cat SKILL.md",
                                "name": "SKILL.md",
                                "path": "/fixture/project/.agents/skills/example-skill/SKILL.md",
                            }
                        ],
                        "duration": {"secs": 0, "nanos": 420000000},
                    },
                },
            },
            {
                "timestamp": "2026-01-01T00:00:02.000Z",
                "type": "event_msg",
                "payload": {
                    "type": "item_completed",
                    "thread_id": "thread_fixture_001",
                    "started_at_ms": MS + 1000,
                    "completed_at_ms": MS + 1420,
                    "item": {
                        "type": "McpToolCall",
                        "id": "item_fixture_002",
                        "server": "example_server",
                        "tool": "example_tool",
                        "status": "completed",
                        "duration": {"secs": 0, "nanos": 420000000},
                        "readOnlyHint": True,
                    },
                },
            },
            {
                "timestamp": "2026-01-01T00:00:03.000Z",
                "type": "event_msg",
                "payload": {
                    "type": "item_completed",
                    "thread_id": "thread_fixture_001",
                    "item": {
                        "type": "McpToolCall",
                        "id": "item_fixture_003",
                        "server": "example_server",
                        "tool": "failing_tool",
                        "status": "failed",
                        "duration": {"secs": 1, "nanos": 0},
                    },
                },
            },
            {
                "timestamp": "2026-01-01T00:00:04.000Z",
                "type": "event_msg",
                "payload": {
                    "type": "item_completed",
                    "thread_id": "thread_fixture_001",
                    "item": {
                        "type": "WebSearch",
                        "id": "item_fixture_004",
                        "query": "fixture query",
                        "action": {"type": "search", "queries": ["fixture query"]},
                        "results": [],
                    },
                },
            },
            {
                "timestamp": "2026-01-01T00:00:05.000Z",
                "type": "event_msg",
                "payload": {
                    "type": "item_completed",
                    "thread_id": "thread_fixture_001",
                    "item": {
                        "type": "DynamicToolCall",
                        "id": "item_fixture_005",
                        "namespace": "ns",
                        "tool": "dyn",
                        "status": "completed",
                        "success": True,
                        "duration": {"secs": 0, "nanos": 1000000},
                    },
                },
            },
            {
                "timestamp": "2026-01-01T00:00:06.000Z",
                "type": "event_msg",
                "payload": {
                    "type": "item_completed",
                    "thread_id": "thread_fixture_001",
                    "item": {
                        "type": "FileChange",
                        "id": "item_fixture_006",
                        "status": "completed",
                        "changes": [],
                        "stdout": "",
                    },
                },
            },
            {
                "timestamp": "2026-01-01T00:00:07.000Z",
                "type": "event_msg",
                "payload": {
                    "type": "item_completed",
                    "thread_id": "thread_fixture_001",
                    "started_at_ms": None,
                    "completed_at_ms": 0,
                    "item": {
                        "type": "Plan",
                        "id": "item_fixture_plan",
                        "status": "completed",
                    },
                },
            },
            {
                "timestamp": "2026-01-01T00:00:08.000Z",
                "type": "event_msg",
                "payload": {
                    "type": "item_completed",
                    "thread_id": "thread_fixture_001",
                    "started_at_ms": MS + 7000,
                    "completed_at_ms": MS + 7420,
                    "item": {
                        "type": "CommandExecution",
                        "id": "item_fixture_relative_skill",
                        "status": "completed",
                        "command": ["bash", "-lc", "cat SKILL.md"],
                        "parsed_cmd": [
                            {
                                "type": "read",
                                "cmd": "cat SKILL.md",
                                "name": "SKILL.md",
                                "path": ".agents/skills/example-skill/SKILL.md",
                            }
                        ],
                    },
                },
            },
        ],
    )


def codex_legacy() -> None:
    jsonl(
        ROOT / "codex/activity-legacy/sessions/2026/01/01/rollout-fixture-activity-legacy.jsonl",
        [
            {
                "timestamp": TS,
                "type": "session_meta",
                "payload": {
                    "id": "id_fixture_activity_legacy",
                    "timestamp": TS,
                    "cwd": "/fixture/project",
                },
            },
            {
                "timestamp": TS,
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "mcp__example_server__example_tool",
                    "arguments": "{}",
                    "call_id": "call_fixture_001",
                    "id": "fc_fixture_001",
                },
            },
            {
                "timestamp": "2026-01-01T00:00:02.000Z",
                "type": "response_item",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "call_fixture_001",
                    "output": "...",
                },
            },
            {
                "timestamp": "2026-01-01T00:00:03.000Z",
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call",
                    "name": "exec",
                    "input": "ls /fixture/project",
                    "call_id": "call_fixture_002",
                    "status": "completed",
                },
            },
            {
                "timestamp": "2026-01-01T00:00:04.000Z",
                "type": "response_item",
                "payload": {
                    "type": "web_search_call",
                    "status": "completed",
                    "action": {"type": "search"},
                },
            },
            {
                "timestamp": "2026-01-01T00:00:05.000Z",
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "apply_patch",
                    "arguments": "{}",
                    "call_id": "call_fixture_003",
                    "id": "fc_fixture_003",
                },
            },
            {
                "timestamp": "2026-01-01T00:00:06.000Z",
                "type": "response_item",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "call_fixture_003",
                    "output": json.dumps({"output": "...", "metadata": {"exit_code": 1}}),
                },
            },
        ],
    )


def codex_class_b() -> None:
    jsonl(
        ROOT / "codex/activity-class-b/sessions/2026/01/01/rollout-fixture-activity-class-b.jsonl",
        [
            {
                "timestamp": TS,
                "type": "session_meta",
                "payload": {
                    "id": "id_fixture_activity_class_b",
                    "timestamp": TS,
                    "cwd": "/fixture/project",
                },
            },
            {
                "timestamp": TS,
                "type": "event_msg",
                "payload": {
                    "type": "item_completed",
                    "thread_id": "thread_fixture_class_b",
                    "item": {"type": "Reasoning", "id": "item_fixture_reasoning"},
                },
            },
            {
                "timestamp": "2026-01-01T00:00:02.000Z",
                "type": "event_msg",
                "payload": {
                    "type": "item_completed",
                    "thread_id": "thread_fixture_class_b",
                    "item": {"type": "AgentMessage", "id": "item_fixture_agent"},
                },
            },
            {
                "timestamp": "2026-01-01T00:00:03.000Z",
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "exec_command",
                    "arguments": json.dumps({"command": ["bash", "-lc", "cargo test"]}),
                    "call_id": "call_fixture_class_b_exec",
                    "id": "fc_fixture_class_b_exec",
                },
            },
            {
                "timestamp": "2026-01-01T00:00:04.000Z",
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "apply_patch",
                    "arguments": "{}",
                    "call_id": "call_fixture_class_b_patch",
                    "id": "fc_fixture_class_b_patch",
                },
            },
            {
                "timestamp": "2026-01-01T00:00:05.000Z",
                "type": "response_item",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "call_fixture_class_b_patch",
                    "output": json.dumps({"output": "...", "metadata": {"exit_code": 0}}),
                },
            },
        ],
    )


def codex_command_shapes() -> None:
    jsonl(
        ROOT / "codex/activity-commands/sessions/2026/01/01/rollout-fixture-activity-commands.jsonl",
        [
            {
                "timestamp": TS,
                "type": "session_meta",
                "payload": {"id": "id_fixture_activity_commands", "timestamp": TS},
            },
            {
                "timestamp": TS,
                "type": "event_msg",
                "payload": {
                    "type": "item_started",
                    "thread_id": "thread_fixture_commands",
                    "item": {
                        "type": "CommandExecution",
                        "id": "item_fixture_started_only",
                        "status": "in_progress",
                        "command": "bash -lc 'npm test'",
                    },
                },
            },
            {
                "timestamp": "2026-01-01T00:00:02.000Z",
                "type": "event_msg",
                "payload": {
                    "type": "item_completed",
                    "thread_id": "thread_fixture_commands",
                    "item": {
                        "type": "CommandExecution",
                        "id": "item_fixture_started_only",
                        "status": "completed",
                    },
                },
            },
            {
                "timestamp": "2026-01-01T00:00:03.000Z",
                "type": "event_msg",
                "payload": {
                    "type": "item_completed",
                    "thread_id": "thread_fixture_commands",
                    "item": {
                        "type": "CommandExecution",
                        "id": "item_fixture_string_command",
                        "status": "completed",
                        "command": "git status",
                    },
                },
            },
        ],
    )


def codex_malformed() -> None:
    jsonl(
        ROOT / "codex/activity-malformed/sessions/2026/01/01/rollout-fixture-activity-malformed.jsonl",
        [
            {
                "timestamp": TS,
                "type": "session_meta",
                "payload": {"id": "id_fixture_activity_malformed", "timestamp": TS},
            },
            "{not-json",
            {
                "timestamp": "2026-01-01T00:00:02.000Z",
                "type": "event_msg",
                "payload": {
                    "type": "item_completed",
                    "thread_id": "thread_fixture_malformed",
                    "item": {
                        "type": "CommandExecution",
                        "id": "item_fixture_malformed",
                        "status": "completed",
                        "parsed_cmd": [{"type": "unknown", "cmd": "rg"}],
                    },
                },
            },
        ],
    )


def claude_assistant(uuid, parent, session, ts, tools):
    return {
        "type": "assistant",
        "uuid": uuid,
        "parentUuid": parent,
        "sessionId": session,
        "requestId": "r1",
        "timestamp": ts,
        "isSidechain": False,
        "version": "2.1.0",
        "message": {
            "role": "assistant",
            "model": "claude-fixture",
            "content": tools,
            "usage": {"input_tokens": 1, "output_tokens": 1},
        },
    }


def claude_result(uuid, parent, session, ts, tool_use_id, is_error):
    row = {
        "type": "user",
        "uuid": uuid,
        "parentUuid": parent,
        "sessionId": session,
        "timestamp": ts,
        "message": {
            "role": "user",
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": "...",
                }
            ],
        },
        "toolUseResult": {"stdout": "...", "stderr": "", "interrupted": False},
    }
    if is_error is not None:
        row["message"]["content"][0]["is_error"] = is_error
    return row


def claude() -> None:
    session = "s_fixture_001"
    jsonl(
        ROOT / "claude/activity/projects/-workspace-activity/session-activity.jsonl",
        [
            claude_assistant(
                "u1",
                "u0",
                session,
                TS,
                [
                    {
                        "type": "tool_use",
                        "id": "toolu_fixture_001",
                        "name": "Bash",
                        "input": {"command": "ls /fixture/project"},
                        "caller": {"type": "direct"},
                    },
                    {
                        "type": "tool_use",
                        "id": "toolu_fixture_002",
                        "name": "Read",
                        "input": {"file_path": "/fixture/project/README.md"},
                    },
                ],
            ),
            claude_result("u2", "u1", session, "2026-01-01T00:00:03.000Z", "toolu_fixture_001", False),
            claude_result("u3", "u2", session, "2026-01-01T00:00:04.000Z", "toolu_fixture_002", True),
            claude_assistant(
                "u4",
                "u3",
                session,
                "2026-01-01T00:00:05.000Z",
                [
                    {
                        "type": "tool_use",
                        "id": "toolu_fixture_003",
                        "name": "Skill",
                        "input": {"skill": "example-skill"},
                    }
                ],
            ),
            claude_result("u5", "u4", session, "2026-01-01T00:00:06.000Z", "toolu_fixture_003", False),
            claude_assistant(
                "u6",
                "u5",
                session,
                "2026-01-01T00:00:07.000Z",
                [
                    {
                        "type": "tool_use",
                        "id": "toolu_fixture_004",
                        "name": "Skill",
                        "input": {"skill": "acme-plugin:example-skill"},
                    }
                ],
            ),
            claude_result("u7", "u6", session, "2026-01-01T00:00:08.000Z", "toolu_fixture_004", None),
            claude_assistant(
                "u8",
                "u7",
                session,
                "2026-01-01T00:00:09.000Z",
                [
                    {
                        "type": "tool_use",
                        "id": "toolu_fixture_005",
                        "name": "mcp__Server_Name__lookup",
                        "input": {},
                    }
                ],
            ),
            claude_assistant(
                "u9",
                "u8",
                session,
                "2026-01-01T00:00:10.000Z",
                [
                    {
                        "type": "tool_use",
                        "id": "toolu_fixture_unpaired",
                        "name": "Grep",
                        "input": {"pattern": "fixture"},
                    }
                ],
            ),
            "{not-json",
        ],
    )
    jsonl(
        ROOT
        / "claude/activity/projects/-workspace-activity/session-activity/subagents/agent.jsonl",
        [
            claude_assistant(
                "su1",
                "su0",
                "s_fixture_subagent",
                TS,
                [
                    {
                        "type": "tool_use",
                        "id": "toolu_fixture_subagent",
                        "name": "Glob",
                        "input": {"pattern": "*.rs"},
                    }
                ],
            ),
            claude_result(
                "su2",
                "su1",
                "s_fixture_subagent",
                "2026-01-01T00:00:02.000Z",
                "toolu_fixture_subagent",
                False,
            ),
        ],
    )
    jsonl(
        ROOT / "claude/activity/projects/-workspace-activity-fork/session-fork.jsonl",
        [
            claude_assistant(
                "f1",
                "f0",
                "s_fixture_fork",
                TS,
                [
                    {
                        "type": "tool_use",
                        "id": "toolu_fixture_001",
                        "name": "Bash",
                        "input": {"command": "echo fork"},
                    }
                ],
            ),
            claude_result("f2", "f1", "s_fixture_fork", "2026-01-01T00:00:02.000Z", "toolu_fixture_001", False),
        ],
    )


def grok() -> None:
    events_dir = ROOT / "grok/activity-events/sessions/workspace-fixture/ses_fixture_activity_events"
    jsonl(
        events_dir / "events.jsonl",
        [
            {"type": "phase_changed", "ts": TS, "phase": "phase_fixture_001"},
            {"type": "tool_started", "ts": TS, "tool_name": "read_file"},
            {
                "type": "tool_completed",
                "ts": "2026-01-01T00:00:01.009Z",
                "tool_name": "read_file",
                "tool_call_id": "tc_fixture_001",
                "duration_ms": 9,
                "outcome": "success",
            },
            {
                "type": "tool_completed",
                "ts": "2026-01-01T00:00:02.000Z",
                "tool_name": "run_terminal_command",
                "tool_call_id": "tc_fixture_002",
                "duration_ms": 40,
                "outcome": "error",
            },
            {
                "type": "permission_resolved",
                "ts": TS,
                "tool_name": "run_terminal_command",
                "decision": "allow",
                "wait_ms": 0,
            },
            "{not-json",
        ],
    )
    write(
        events_dir / "summary.json",
        json.dumps(
            {
                "session_id": "ses_fixture_activity_events",
                "current_model_id": "grok-fixture",
                "num_messages": 2,
            },
            indent=2,
        )
        + "\n",
    )
    jsonl(events_dir / "chat_history.jsonl", [{"type": "assistant", "content": "fixture"}])

    chat_dir = ROOT / "grok/activity-chat/sessions/workspace-fixture/ses_fixture_activity_chat"
    jsonl(
        chat_dir / "chat_history.jsonl",
        [
            {
                "type": "assistant",
                "tool_calls": [
                    {
                        "id": "tc_fixture_001",
                        "name": "read_file",
                        "arguments": "{}",
                    }
                ],
            },
            {"type": "tool_result", "tool_call_id": "tc_fixture_001", "content": "..."},
            {
                "type": "backend_tool_call",
                "id": "tc_fixture_backend",
                "name": "backend_tool_call",
            },
            "{not-json",
        ],
    )
    write(
        chat_dir / "summary.json",
        json.dumps(
            {
                "session_id": "ses_fixture_activity_chat",
                "current_model_id": "grok-fixture",
                "num_messages": 1,
            },
            indent=2,
        )
        + "\n",
    )


def opencode() -> None:
    db_path = ROOT / "opencode/activity/opencode.db"
    db_path.parent.mkdir(parents=True, exist_ok=True)
    if db_path.exists():
        db_path.unlink()
    conn = sqlite3.connect(db_path)
    conn.executescript(
        """
        CREATE TABLE session (
          id TEXT PRIMARY KEY, title TEXT, model TEXT, cost REAL NOT NULL DEFAULT 0,
          tokens_input INTEGER NOT NULL DEFAULT 0, tokens_output INTEGER NOT NULL DEFAULT 0,
          tokens_reasoning INTEGER NOT NULL DEFAULT 0, tokens_cache_read INTEGER NOT NULL DEFAULT 0,
          tokens_cache_write INTEGER NOT NULL DEFAULT 0, time_created INTEGER NOT NULL,
          time_updated INTEGER NOT NULL, directory TEXT NOT NULL
        );
        CREATE TABLE message (
          id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
          time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL
        );
        CREATE TABLE part (
          id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
          time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL
        );
        """
    )
    conn.execute(
        "INSERT INTO session VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
        (
            "ses_fixture_activity",
            "Implement fixture parser",
            "gpt-5",
            0.0,
            1,
            1,
            0,
            0,
            0,
            MS,
            MS + 3000,
            "/fixture/project",
        ),
    )
    conn.execute(
        "INSERT INTO message VALUES (?,?,?,?,?)",
        (
            "msg_fixture_001",
            "ses_fixture_activity",
            MS,
            MS + 3000,
            json.dumps(
                {
                    "id": "msg_fixture_001",
                    "providerID": "opencode",
                    "modelID": "gpt-5.2-codex",
                },
                separators=(",", ":"),
            ),
        ),
    )
    parts = [
        (
            "part_fixture_001",
            json.dumps(
                {
                    "type": "tool",
                    "callID": "call_fixture_001",
                    "tool": "read",
                    "state": {
                        "status": "completed",
                        "input": {"filePath": "/fixture/project/README.md"},
                        "output": "...",
                        "title": "read",
                        "metadata": {},
                        "time": {"start": MS, "end": MS + 3},
                    },
                },
                separators=(",", ":"),
            ),
            MS,
            MS + 3,
        ),
        (
            "part_fixture_002",
            json.dumps(
                {
                    "type": "tool",
                    "callID": "call_fixture_002",
                    "tool": "bash",
                    "state": {
                        "status": "error",
                        "input": {"command": "ls /fixture/project"},
                        "error": "...",
                        "time": {"start": MS + 1000, "end": MS + 1500},
                    },
                },
                separators=(",", ":"),
            ),
            MS + 1000,
            MS + 1500,
        ),
        (
            "part_fixture_003",
            json.dumps(
                {
                    "type": "tool",
                    "callID": "call_fixture_003",
                    "tool": "skill",
                    "state": {
                        "status": "completed",
                        "input": {"name": "example-skill"},
                        "output": "...",
                        "time": {"start": MS + 2000, "end": MS + 2001},
                    },
                },
                separators=(",", ":"),
            ),
            MS + 2000,
            MS + 2001,
        ),
        (
            "part_fixture_004",
            json.dumps(
                {
                    "type": "tool",
                    "callID": "call_fixture_004",
                    "tool": "example_server_example_tool",
                    "state": {
                        "status": "completed",
                        "input": {},
                        "output": "...",
                        "time": {"start": MS + 2500, "end": MS + 2510},
                    },
                },
                separators=(",", ":"),
            ),
            MS + 2500,
            MS + 2510,
        ),
        (
            "part_fixture_005",
            json.dumps(
                {
                    "type": "tool",
                    "callID": "call_fixture_005",
                    "tool": "bash",
                    "state": {
                        "status": "running",
                        "input": {},
                        "time": {"start": MS + 2800},
                    },
                },
                separators=(",", ":"),
            ),
            MS + 2800,
            MS + 2800,
        ),
        (
            "part_fixture_text",
            json.dumps({"type": "text", "text": "fixture"}, separators=(",", ":")),
            MS,
            MS,
        ),
        (
            "part_fixture_malformed",
            "{not-json",
            MS,
            MS + 1,
        ),
    ]
    for part_id, data, created, updated in parts:
        conn.execute(
            "INSERT INTO part VALUES (?,?,?,?,?,?)",
            (part_id, "msg_fixture_001", "ses_fixture_activity", created, updated, data),
        )
    conn.commit()
    conn.close()
    write(
        ROOT / "opencode/activity/opencode.json",
        """{
  // fixture MCP catalog
  "mcp": {
    "example_server": {
      "type": "local"
    }
  }
}
""",
    )


def update_manifest() -> None:
    existing = json.loads((ROOT / "MANIFEST.json").read_text(encoding="utf-8"))
    by_path = {entry["path"]: entry for entry in existing.get("files", [])}
    for path in ROOT.rglob("*"):
        if not path.is_file():
            continue
        if path.name in {"build_fixtures.py", "write_activity_fixtures.py", "MANIFEST.json"}:
            continue
        rel = path.relative_to(ROOT).as_posix()
        data = path.read_bytes()
        by_path[rel] = {
            "bytes": len(data),
            "path": rel,
            "sha256": hashlib.sha256(data).hexdigest(),
        }
    existing["files"] = [by_path[key] for key in sorted(by_path)]
    write(ROOT / "MANIFEST.json", json.dumps(existing, indent=2) + "\n")


def main() -> None:
    codex_native()
    codex_legacy()
    codex_class_b()
    codex_command_shapes()
    codex_malformed()
    claude()
    grok()
    opencode()
    update_manifest()
    print("wrote synthetic activity fixtures")


if __name__ == "__main__":
    main()
