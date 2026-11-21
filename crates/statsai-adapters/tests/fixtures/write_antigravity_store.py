#!/usr/bin/env python3
"""Writes a synthetic Antigravity store for exercising the Antigravity export.

Usage: ./write_antigravity_store.py <dir>

The store has the schema of Antigravity 2.34's conversation databases and
invented values throughout: token counts, timestamps, ids, model ids, and
names. Use it to run `build_fixtures.py --only antigravity` where no real store
exists. It also plants the kinds of strings the export must remove, such as a
home path, a git remote, an email address, and an internal model codename in
the request settings, so a run over it checks the rewrite as well.

It reads no local store.
"""

from __future__ import annotations

import hashlib
import sqlite3
import sys
from pathlib import Path

SCHEMA = """
CREATE TABLE `trajectory_meta` (`trajectory_id` text,`cascade_id` text,`trajectory_type` integer,`source` integer,PRIMARY KEY (`trajectory_id`));
CREATE TABLE `steps` (`idx` integer,`step_type` integer NOT NULL DEFAULT 0,`status` integer NOT NULL DEFAULT 0,`has_subtrajectory` numeric NOT NULL DEFAULT false,`metadata` blob,`error_details` blob,`permissions` blob,`task_details` blob,`render_info` blob,`step_payload` blob,`step_format` integer NOT NULL DEFAULT 0,PRIMARY KEY (`idx`));
CREATE TABLE `gen_metadata` (`idx` integer,`data` blob,`size` integer NOT NULL DEFAULT 0,PRIMARY KEY (`idx`));
CREATE TABLE `executor_metadata` (`idx` integer,`data` blob,PRIMARY KEY (`idx`));
CREATE TABLE `parent_references` (`idx` integer,`data` blob,PRIMARY KEY (`idx`));
CREATE TABLE `trajectory_metadata_blob` (`id` text DEFAULT "main",`data` blob,PRIMARY KEY (`id`));
CREATE TABLE `battle_mode_infos` (`idx` integer,`data` blob,PRIMARY KEY (`idx`));
CREATE INDEX `idx_steps_status` ON `steps`(`status`);
CREATE INDEX `idx_steps_step_type` ON `steps`(`step_type`);
"""

PLANNER, CHECKPOINT, VIEW_FILE, RUN_COMMAND = 15, 23, 8, 21
GEMINI, ANTHROPIC_VERTEX, OPENAI_VERTEX = 24, 26, 31
T0 = 1_788_000_000  # 2026-08-29


class Pb:
    def __init__(self) -> None:
        self.out = bytearray()

    def _varint(self, value: int) -> None:
        while True:
            byte = value & 0x7F
            value >>= 7
            if value:
                self.out.append(byte | 0x80)
            else:
                self.out.append(byte)
                return

    def varint(self, field: int, value: int) -> "Pb":
        self._varint(field << 3)
        self._varint(value)
        return self

    def bytes(self, field: int, value: bytes) -> "Pb":
        self._varint((field << 3) | 2)
        self._varint(len(value))
        self.out += value
        return self

    def string(self, field: int, value: str) -> "Pb":
        return self.bytes(field, value.encode("utf-8"))

    def message(self, field: int, value: "Pb") -> "Pb":
        return self.bytes(field, bytes(value.out))

    def double(self, field: int, value: float) -> "Pb":
        import struct

        self._varint((field << 3) | 1)
        self.out += struct.pack("<d", value)
        return self

    def done(self) -> bytes:
        return bytes(self.out)


def uid(name: str) -> str:
    digest = hashlib.sha256(name.encode()).hexdigest()
    return f"{digest[:8]}-{digest[8:12]}-4{digest[13:16]}-a{digest[17:20]}-{digest[20:32]}"


def ts(seconds: int, millis: int = 0) -> Pb:
    return Pb().varint(1, seconds).varint(2, millis * 1_000_000)


def usage(model: int, provider: int, uncached: int, cached: int, thinking: int, response: int, response_id: str) -> Pb:
    pb = Pb().varint(1, model).varint(2, uncached).varint(3, thinking + response)
    if cached:
        pb.varint(5, cached)
    pb.varint(6, provider)
    pb.message(8, Pb().string(1, "sessionID").string(2, "-4815162342108151623"))
    if thinking:
        pb.varint(9, thinking)
    return pb.varint(10, response).string(11, response_id)


class Conversation:
    def __init__(self, root: Path, name: str) -> None:
        self.id = uid(name)
        self.trajectory = uid(name + "/trajectory")
        self.execution = uid(name + "/execution")
        self.path = root / "conversations" / f"{self.id}.db"
        self.db = sqlite3.connect(self.path)
        self.db.executescript(SCHEMA)
        self.db.execute("INSERT INTO trajectory_meta VALUES (?, ?, 4, 1)", (self.trajectory, self.id))
        self.step_index = 0
        self.gen_index = 0

    def step_ids(self, index: int) -> Pb:
        return Pb().string(1, self.trajectory).varint(2, index).string(4, self.id)

    def add_step(self, step_type: int, metadata: bytes, payload: bytes = b"") -> int:
        index = self.step_index
        self.db.execute(
            "INSERT INTO steps (idx, step_type, status, metadata, step_payload) VALUES (?, ?, 3, ?, ?)",
            (index, step_type, metadata, payload),
        )
        self.step_index += 1
        return index

    def tool(self, step_type: int, at: int, text: str) -> None:
        metadata = Pb().message(1, ts(at)).varint(3, 2).message(8, ts(at, 400)).message(20, self.step_ids(self.step_index)).done()
        self.add_step(step_type, metadata, Pb().string(1, text).string(2, "/Users/fixture-person/secret-project/src/main.rs").done())

    def planner(self, at: int, model: int, provider: int | None, counts: tuple[int, int, int, int] | None, ttft_ms: int, gen_ms: int) -> int:
        index = self.step_index
        finished_ms = ttft_ms + gen_ms
        pb = (
            Pb().message(1, ts(at)).varint(3, 2)
            .message(6, ts(at + ttft_ms // 1000, ttft_ms % 1000))
            .message(7, ts(at + finished_ms // 1000, finished_ms % 1000))
            .message(8, ts(at + finished_ms // 1000, finished_ms % 1000))
        )
        if counts is not None:
            pb.message(9, usage(model, provider or GEMINI, *counts, f"resp-{self.id[:8]}-{index}"))
        pb.varint(11, model).string(12, self.execution).message(13, Pb().varint(1, model))
        pb.message(20, self.step_ids(index)).varint(21, 1)
        pb.message(26, Pb().message(1, Pb().varint(1, 3).message(2, ts(at + finished_ms // 1000))))
        pb.message(32, ts(at + finished_ms // 1000, finished_ms % 1000))
        return self.add_step(PLANNER, pb.done(), Pb().string(1, "I will read the file and fix the parser.").done())

    def checkpoint(self, at: int, attempts: list[tuple[int, int, int, int] | None], model: int | None, duplicate: bool) -> int:
        index = self.step_index
        pb = Pb().message(1, ts(at)).varint(3, 5).message(8, ts(at + 2))
        last = next((a for a in reversed(attempts) if a is not None), None)
        if duplicate and last is not None:
            pb.message(9, usage(model or 0, GEMINI, *last, f"ckpt-{index}"))
        pb.string(12, self.execution).message(20, self.step_ids(index))
        for attempt in attempts:
            retry = Pb()
            if attempt is not None:
                attempt_usage = usage(model or 0, GEMINI, *attempt, f"ckpt-{index}") if model else (
                    Pb().varint(2, attempt[0]).varint(3, attempt[2] + attempt[3]).varint(6, GEMINI)
                    .string(7, "bot-" + uid(f"{self.id}/bot")).varint(10, attempt[3]).string(11, f"ckpt-{index}")
                )
                retry.message(2, attempt_usage)
            retry.string(4, "Summarize the conversation so far for the user.")
            pb.message(28, retry)
        pb.message(32, ts(at))
        return self.add_step(CHECKPOINT, pb.done())

    def gen(self, model: int, served: str | None, attempts: list[tuple[int, int, int, int, int] | None], label: str | None = None, at: int = T0) -> None:
        prompt = ("You are a coding agent in /Users/fixture-person/secret-project. "
                  "The user fixture-person@example.com asked to fix the parser. " * 8)
        request = Pb().varint(3, model).message(4, Pb()).varint(6, 1).message(7, Pb().string(1, "trajectory-tag"))
        request.message(9, Pb().message(4, ts(at)).varint(5, 1).message(6, Pb().varint(1, 1).string(3, prompt)))
        request.message(11, Pb().varint(1, 2).varint(2, 250_000_000)).message(12, Pb().varint(1, 3).varint(2, 500_000_000))
        request.message(15, Pb().varint(1, 1).varint(2, 64_000).double(5, 0.4).string(9, "stop-sequence"))
        for attempt in attempts:
            entry = Pb()
            if attempt is not None:
                entry.message(2, usage(model, attempt[4], *attempt[:4], f"gen-{self.id[:8]}-{self.gen_index}"))
            request.message(17, entry.string(4, "attempt-tag"))
        if served is not None:
            request.string(19, served)
        for key, value in [("model_enum", f"MODEL_PLACEHOLDER_M{model % 1000}"), ("trajectory_id", self.trajectory), ("last_step_index", str(self.step_index))]:
            request.message(20, Pb().string(1, key).string(2, value))
        row = Pb().bytes(2, b"\x01\x02")
        if label is not None:
            settings = Pb().varint(1, model).varint(6, 65_536).varint(7, 1)
            # Request settings name internal model codenames; none may survive.
            settings.message(13, Pb().message(2, Pb().string(1, "Be concise.")).message(
                39, Pb().message(10, Pb().message(1, Pb().string(3, "router").message(5, Pb().message(2, Pb().string(1, "gemini-9-codename-tier2")))))
            ))
            settings.message(15, Pb().varint(1, model)).string(28, label)
            row.message(3, settings)
        row.string(4, self.execution).bytes(8, hashlib.sha256(self.id.encode()).digest()[:16])
        row.message(1, request)
        data = row.done()
        self.db.execute("INSERT INTO gen_metadata (idx, data, size) VALUES (?, ?, ?)", (self.gen_index, data, len(data)))
        self.gen_index += 1

    def trajectory_blob(self, parent: "Conversation | None" = None, agent: str | None = None) -> None:
        workspace = (
            Pb().string(1, "file:///Users/fixture-person/secret-project")
            .string(2, "file:///Users/fixture-person/secret-project")
            .message(3, Pb().string(1, "fixture-person/secret-project").string(2, "git@git.example.invalid:fixture-person/secret-project.git"))
            .string(4, "feature/secret-branch")
        )
        pb = Pb().message(1, workspace).message(2, ts(T0)).string(3, uid(self.id + "/workspace"))
        if agent:
            pb.message(4, Pb().string(1, agent).string(8, "Look up how the parser handles escapes."))
        if parent:
            pb.string(5, parent.id)
        pb.string(7, "file:///Users/fixture-person/secret-project")
        if agent:
            pb.message(8, Pb().string(1, agent).string(2, "Research the escape handling.").varint(7, 1026).varint(8, 1))
        pb.bytes(15, hashlib.sha256(self.id.encode()).digest()[:16]).string(18, uid("workspace"))
        self.db.execute("INSERT INTO trajectory_metadata_blob VALUES ('main', ?)", (pb.done(),))

    def close(self) -> None:
        self.db.commit()
        self.db.close()


def write_store(root: Path) -> None:
    (root / "conversations").mkdir(parents=True, exist_ok=True)
    (root / "STATSAI_SYNTHETIC_STORE").write_text("Invented values; see write_antigravity_store.py.\n", encoding="utf-8")

    # Gemini with thinking, after a model switch: 1020 was selected first and
    # only the later 1132 keeps a label here. A newer checkpoint has no model id.
    gemini = Conversation(root, "gemini-switch")
    gemini.trajectory_blob()
    gemini.tool(VIEW_FILE, T0 - 5, "README.md")
    gemini.planner(T0, 1020, GEMINI, (5_210, 0, 412, 96), 1_180, 140)
    gemini.gen(1020, "gemini-3-flash-a", [(5_210, 0, 412, 96, GEMINI)])
    gemini.checkpoint(T0 + 20, [(3_900, 0, 0, 210)], None, False)
    gemini.planner(T0 + 30, 1132, GEMINI, (612, 6_144, 203, 88), 960, 75)
    gemini.gen(1132, "gemini-3-flash-a", [(612, 6_144, 203, 88, GEMINI)])
    gemini.tool(RUN_COMMAND, T0 + 35, "cargo test --workspace")
    gemini.planner(T0 + 40, 1132, GEMINI, None, 900, 0)
    gemini.gen(1132, None, [None])
    gemini.planner(T0 + 50, 1132, GEMINI, (701, 6_656, 0, 54), 1_020, 61)
    gemini.gen(1132, "gemini-3-flash-a", [None, (701, 6_656, 0, 54, GEMINI)], label="gemini-3-flash-agent")
    gemini.close()

    # Claude on Vertex, an older file whose checkpoint is copied into field 9.
    claude = Conversation(root, "claude-vertex")
    claude.trajectory_blob()
    claude.planner(T0 + 3_600, 1026, ANTHROPIC_VERTEX, (18_420, 0, 0, 312), 2_460, 3_800)
    claude.gen(1026, "claude-opus-4-6-thinking", [(18_420, 0, 0, 312, ANTHROPIC_VERTEX)])
    claude.checkpoint(T0 + 3_620, [(9_120, 0, 0, 140)], 1050, True)
    claude.planner(T0 + 3_640, 1026, ANTHROPIC_VERTEX, (402, 18_400, 0, 1_204), 2_910, 9_400)
    claude.gen(1026, "claude-opus-4-6-thinking", [None, (402, 18_400, 0, 1_204, ANTHROPIC_VERTEX)], label="claude-opus-4-6-thinking")
    claude.close()

    # GPT-OSS on Vertex.
    gpt = Conversation(root, "gpt-oss")
    gpt.trajectory_blob()
    gpt.planner(T0 + 7_200, 342, OPENAI_VERTEX, (7_330, 0, 0, 158), 1_050, 540)
    gpt.gen(342, "gpt-oss-120b-medium", [(7_330, 0, 0, 158, OPENAI_VERTEX)], label="gpt-oss-120b-medium")
    gpt.close()

    # A parent and its research sub-agent.
    parent = Conversation(root, "parent")
    parent.trajectory_blob()
    parent.planner(T0 + 10_800, 1026, ANTHROPIC_VERTEX, (16_002, 0, 0, 640), 2_200, 4_100)
    parent.gen(1026, "claude-opus-4-6-thinking", [(16_002, 0, 0, 640, ANTHROPIC_VERTEX)], label="claude-opus-4-6-thinking")
    parent.planner(T0 + 10_900, 1026, ANTHROPIC_VERTEX, None, 800, 0)
    parent.gen(1026, None, [None])
    parent.close()
    child = Conversation(root, "child")
    child.trajectory_blob(parent=parent, agent="research")
    child.planner(T0 + 10_820, 1026, ANTHROPIC_VERTEX, (14_800, 0, 0, 402), 2_050, 3_300)
    child.gen(1026, "claude-opus-4-6-thinking", [(14_800, 0, 0, 402, ANTHROPIC_VERTEX)], label="claude-opus-4-6-thinking")
    child.close()

    # Another conversation selected 1020, which names it for the first.
    named = Conversation(root, "label-elsewhere")
    named.trajectory_blob()
    named.planner(T0 + 14_400, 1020, GEMINI, (3_100, 0, 120, 44), 1_100, 90)
    named.gen(1020, "gemini-3-flash-a", [(3_100, 0, 120, 44, GEMINI)], label="gemini-3.5-flash-low")
    named.close()

    # A legacy conversation that only exists encrypted.
    stream = b""
    while len(stream) < 384:
        stream += hashlib.sha256(b"synthetic-encrypted" + len(stream).to_bytes(4, "big")).digest()
    (root / "conversations" / f"{uid('legacy')}.pb").write_bytes(stream[:384])


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__.strip().splitlines()[2], file=sys.stderr)
        return 64
    root = Path(sys.argv[1])
    if (root / "conversations").exists():
        print(f"{root / 'conversations'} exists; choose an empty directory", file=sys.stderr)
        return 1
    write_store(root)
    print(root)
    return 0


if __name__ == "__main__":
    sys.exit(main())
