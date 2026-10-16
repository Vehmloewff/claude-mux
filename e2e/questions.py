#!/usr/bin/env python3
"""Drive `claude-mux questions` against the installed Claude Code.

These are not cargo tests. They build the debug binaries, start a private
daemon, and walk real dialogs: folder trust, a permission prompt, and
AskUserQuestion (one question, several, a checkbox, notes, and Chat about this).

Question sessions use the `haiku` model alias at `--effort low`, which is the
cheap Claude Code model. Trust dialogs never send a prompt, so they make no
model call.

    python3 e2e/questions.py
    python3 e2e/questions.py trust-yes single

A failing run leaves its daemon home and a per-case dump on disk; the path is
printed at the start.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

MODEL = "haiku"
DIALOG_TIMEOUT = 150
SETTLE_TIMEOUT = 180
POLL = 1.5

# Tokens that appear only in the answer path, never in the prompt that asked.
NOTE = "note-cedar-19"
CHAT = "pebble-7741"
PERM = "perm-marker-42"

ROOT = Path(__file__).resolve().parents[1]


class CaseFailure(Exception):
    pass


class Mux:
    def __init__(self, binary: Path, home: Path):
        self.binary = binary
        self.home = home
        self.work = home.parent / "work"
        self.artifacts = home.parent / "artifacts"
        self.work.mkdir()
        self.artifacts.mkdir()
        self.env = os.environ.copy()
        self.env["CLAUDE_MUX_HOME"] = str(home)
        self.env["CLAUDE_MUX_SLEEP_AFTER"] = "never"
        self.env.pop("CLAUDE_MUX_SOCKET", None)

    def run(self, *args: str, check: bool = True, timeout: float = 240) -> subprocess.CompletedProcess[str]:
        try:
            cp = subprocess.run(
                [str(self.binary), *args],
                cwd=self.work,
                env=self.env,
                text=True,
                capture_output=True,
                timeout=timeout,
                check=False,
            )
        except subprocess.TimeoutExpired as e:
            raise CaseFailure(f"timed out: claude-mux {' '.join(args)}") from e
        if check and cp.returncode != 0:
            raise CaseFailure(
                f"claude-mux {' '.join(args)} exited {cp.returncode}\n{cp.stderr.strip()}\n{cp.stdout.strip()}"
            )
        return cp

    def shutdown(self) -> None:
        subprocess.run(
            [str(self.binary), "shutdown"],
            cwd=self.work,
            env=self.env,
            text=True,
            capture_output=True,
            timeout=30,
            check=False,
        )

    def questions(self) -> list[dict]:
        cp = self.run("questions", "--json")
        return json.loads(cp.stdout or "[]")

    def status(self, sid: str) -> dict:
        return json.loads(self.run("status", sid, "--json").stdout)

    def peek(self, sid: str) -> str:
        return self.run("peek", sid, check=False).stdout

    def rm(self, sid: str) -> None:
        self.run("rm", sid, check=False)

    def workdir(self, name: str) -> Path:
        path = self.work / name
        path.mkdir()
        return path

    def create(
        self,
        cwd: Path,
        prompt: str | None,
        *,
        name: str,
        trust: bool,
        yolo: bool,
        claude_args: tuple[str, ...] = (),
    ) -> str:
        cmd = ["create", "-C", str(cwd), "--description", name, "--model", MODEL]
        if prompt is not None:
            cmd.insert(1, prompt)
        if trust:
            cmd.append("--trust")
        if yolo:
            cmd.append("--yolo")
        if claude_args:
            cmd.append("--")
            cmd.extend(claude_args)
        return self.run(*cmd).stdout.strip()

    def send(self, sid: str, message: str) -> None:
        self.run("send", sid, message)

    def dump(self, name: str, sid: str | None, error: BaseException) -> None:
        dest = self.artifacts / name
        dest.mkdir(exist_ok=True)
        (dest / "error.txt").write_text(str(error))
        try:
            (dest / "questions.json").write_text(json.dumps(self.questions(), indent=2))
        except Exception as e:  # noqa: BLE001
            (dest / "questions.json").write_text(f"unavailable: {e}\n")
        if sid:
            try:
                (dest / "status.json").write_text(json.dumps(self.status(sid), indent=2))
            except Exception as e:  # noqa: BLE001
                (dest / "status.json").write_text(f"unavailable: {e}\n")
            (dest / "screen.txt").write_text(self.peek(sid))
            try:
                info = json.loads((dest / "status.json").read_text())
                text = transcript(info)
                if text:
                    (dest / "transcript.jsonl").write_text(text)
            except Exception:
                pass
        log = self.home / "daemon.log"
        if log.exists():
            lines = log.read_text(errors="replace").splitlines()
            (dest / "daemon.log").write_text("\n".join(lines[-200:]) + "\n")


def claude_config() -> Path:
    override = os.environ.get("CLAUDE_CONFIG_DIR")
    return Path(override) if override else Path.home() / ".claude"


def encode_cwd(cwd: str) -> str:
    return "".join(c if c.isascii() and c.isalnum() else "-" for c in cwd)


def transcript(info: dict) -> str:
    path = claude_config() / "projects" / encode_cwd(info["cwd"]) / f"{info['id']}.jsonl"
    if not path.exists():
        return ""
    return path.read_text(errors="replace")


def block_text(content) -> str:
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts: list[str] = []
        for item in content:
            if isinstance(item, str):
                parts.append(item)
            elif isinstance(item, dict):
                text = item.get("text")
                parts.append(text if isinstance(text, str) else json.dumps(item, ensure_ascii=False))
        return "\n".join(parts)
    return json.dumps(content, ensure_ascii=False)


def tool_result_text(jsonl: str) -> list[str]:
    found: list[str] = []
    for line in jsonl.splitlines():
        if "tool_result" not in line:
            continue
        try:
            entry = json.loads(line)
        except json.JSONDecodeError:
            continue
        content = entry.get("message", {}).get("content")
        if not isinstance(content, list):
            continue
        for block in content:
            if isinstance(block, dict) and block.get("type") == "tool_result":
                found.append(block_text(block.get("content")))
    return found


def latest_answer(jsonl: str) -> str:
    results = tool_result_text(jsonl)
    if not results:
        raise CaseFailure(f"no tool result in transcript\n{jsonl[-2000:]}")
    return results[-1]


def ask_prompt(tool_input: dict) -> str:
    payload = json.dumps(tool_input, indent=2)
    return (
        "Call the AskUserQuestion tool immediately with exactly this input. "
        "Do not change headers, question text, labels, descriptions, previews, or multiSelect. "
        "Do not call any other tool. Do not answer the question yourself. Do not explain.\n\n"
        f"{payload}\n"
    )


QUESTION_ARGS = (
    "--effort",
    "low",
    "--tools",
    "AskUserQuestion",
    "--disable-slash-commands",
)


def log(name: str, message: str) -> None:
    print(f"[{name}] {message}", file=sys.stderr, flush=True)


def row_for(rows: list[dict], sid: str) -> dict | None:
    return next((r for r in rows if r.get("id") == sid), None)


def questions_of(row: dict) -> list[dict]:
    dialog = row.get("dialog") or {}
    return dialog.get("questions") or []


def option_number(question: dict, label_part: str) -> int:
    part = label_part.lower()
    for i, opt in enumerate(question["options"], start=1):
        if part in opt["label"].lower():
            return i
    labels = [o["label"] for o in question["options"]]
    raise CaseFailure(f"no option containing {label_part!r} in {labels}")


def headers(row: dict) -> list[str | None]:
    return [q.get("header") for q in questions_of(row)]


def poll(mux: Mux, sid: str, pred, timeout: float, what: str):
    """Call pred(status, rows) until it returns a value other than None."""
    deadline = time.monotonic() + timeout
    last_status: dict = {}
    last_rows: list[dict] = []
    while time.monotonic() < deadline:
        last_status = mux.status(sid)
        last_rows = mux.questions()
        got = pred(last_status, last_rows)
        if got is not None:
            return got
        if last_status.get("status") in {"exited", "stopped"}:
            raise CaseFailure(f"{what}: session {last_status.get('status')}\n{mux.peek(sid)}")
        time.sleep(POLL)
    peek = mux.peek(sid)
    raise CaseFailure(
        f"timed out waiting for {what}\nstatus={last_status.get('status')} detail={last_status.get('status_detail')}\n{peek}"
    )


def wait_for_dialog(mux: Mux, sid: str, match, timeout: float = DIALOG_TIMEOUT) -> dict:
    def pred(status, rows):
        row = row_for(rows, sid)
        if row and questions_of(row):
            if match(row):
                return row
            # The transcript or screen dialog arrives whole; a mismatch will not repair itself.
            return ("wrong", row)
        if status.get("status") == "idle" and status.get("queued", 0) == 0:
            return False
        return None

    got = poll(mux, sid, pred, timeout, "dialog")
    if got is False:
        raise CaseFailure(
            f"session went idle without the expected dialog\n{mux.peek(sid)}\n{mux.run('last', sid, check=False).stdout}"
        )
    if isinstance(got, tuple):
        raise CaseFailure(f"dialog did not match\n{json.dumps(got[1], indent=2)}\n{mux.peek(sid)}")
    return got


def ensure_dialog(mux: Mux, sid: str, prompt: str, match) -> dict:
    try:
        return wait_for_dialog(mux, sid, match)
    except CaseFailure as first:
        if "went idle" not in str(first):
            raise
        log("retry", "model finished without asking; sending the prompt again")
        mux.send(sid, prompt)
        return wait_for_dialog(mux, sid, match)


def wait_settled(mux: Mux, sid: str, timeout: float = SETTLE_TIMEOUT) -> dict:
    cp = mux.run("wait", sid, "--timeout", str(int(timeout)), "--json", check=False, timeout=timeout + 30)
    if cp.returncode == 124:
        raise CaseFailure(f"wait timed out\n{mux.peek(sid)}")
    if cp.returncode != 0:
        raise CaseFailure(f"wait exited {cp.returncode}\n{cp.stderr}\n{cp.stdout}\n{mux.peek(sid)}")
    sessions = json.loads(cp.stdout)
    info = sessions[0]
    if info.get("status") != "idle":
        raise CaseFailure(f"settled as {info.get('status')}: {info.get('status_detail')}\n{mux.peek(sid)}")
    return info


def assert_answer(info: dict, *parts: str) -> str:
    body = latest_answer(transcript(info))
    missing = [p for p in parts if p not in body]
    if missing:
        raise CaseFailure(f"tool result missing {missing}\n{body}")
    return body


def assert_not_waiting(mux: Mux, sid: str) -> None:
    row = row_for(mux.questions(), sid)
    if row is not None:
        raise CaseFailure(f"session still waiting\n{json.dumps(row, indent=2)}\n{mux.peek(sid)}")


def recorded_map(row: dict) -> dict[str, dict]:
    return {item["question"]: item for item in row.get("recorded") or []}


def must_fail(mux: Mux, *args: str, contains: str) -> None:
    cp = mux.run(*args, check=False)
    text = f"{cp.stderr}\n{cp.stdout}"
    if cp.returncode == 0 or contains not in text:
        raise CaseFailure(f"expected failure containing {contains!r}, got {cp.returncode}: {text.strip()}")


def open_question_session(mux: Mux, name: str, tool_input: dict) -> tuple[str, str]:
    prompt = ask_prompt(tool_input)
    cwd = mux.workdir(name)
    log(name, "starting haiku session")
    sid = mux.create(cwd, prompt, name=name, trust=True, yolo=True, claude_args=QUESTION_ARGS)
    return sid, prompt


def expect_headers(wanted: list[str]):
    def match(row: dict) -> bool:
        got = headers(row)
        return got == wanted and all(questions_of(row))

    return match


def test_trust_yes(mux: Mux) -> None:
    name = "trust-yes"
    cwd = mux.workdir(name)
    log(name, "starting claude with no prompt, so the trust dialog is the whole test")
    sid = mux.create(cwd, None, name=name, trust=False, yolo=False)
    row = wait_for_dialog(mux, sid, lambda r: r.get("screen") and questions_of(r))
    question = questions_of(row)[0]
    if "?" not in question["question"] or "trust" not in question["question"].lower():
        raise CaseFailure(f"unexpected trust question: {question}")
    yes = option_number(question, "yes")
    listed = mux.run("questions").stdout
    if "claude-mux questions answer" not in listed or "claude-mux questions submit" not in listed:
        raise CaseFailure(f"listing does not explain how to answer\n{listed}")
    must_fail(mux, "questions", "answer", question["question"], "9", contains="is not on")
    mux.run("questions", "answer", question["question"], str(yes))
    again = row_for(mux.questions(), sid)
    if again is None or recorded_map(again).get(question["question"], {}).get("options") != [yes]:
        raise CaseFailure(f"choice was not recorded\n{json.dumps(again, indent=2)}")
    selected = next((i for i, opt in enumerate(question["options"], start=1) if opt.get("selected")), None)
    if selected == yes:
        raise CaseFailure(f"Yes is already highlighted, so this would not move the selection: {question}")
    log(name, "submitting Yes, which is not the highlighted row")
    submitted = mux.run("questions", "submit").stdout
    if f"submitted {sid}" not in submitted:
        raise CaseFailure(submitted)

    def cleared(status, rows):
        if status.get("status") != "needs_input" and row_for(rows, sid) is None:
            return True
        return None

    poll(mux, sid, cleared, 45, "trust dialog to close")


def test_trust_no(mux: Mux) -> None:
    name = "trust-no"
    cwd = mux.workdir(name)
    log(name, "declining trust on the highlighted row")
    sid = mux.create(cwd, None, name=name, trust=False, yolo=False)
    row = wait_for_dialog(mux, sid, lambda r: r.get("screen") and questions_of(r))
    question = questions_of(row)[0]
    no = option_number(question, "no")
    yes = option_number(question, "yes")
    if no == yes:
        raise CaseFailure(f"yes and no resolved to the same option: {question}")
    mux.run("questions", "answer", question["question"], str(no))
    mux.run("questions", "submit")

    def exited(status, _rows):
        return True if status.get("status") == "exited" else None

    poll(mux, sid, exited, 45, "claude to exit")


def test_single(mux: Mux) -> None:
    name = "single"
    tool_input = {
        "questions": [
            {
                "header": "State",
                "question": "Which state should the example use?",
                "multiSelect": False,
                "options": [
                    {"label": "Draft", "description": "Keep it unpublished"},
                    {"label": "Review", "description": "Send it for review"},
                    {"label": "Live", "description": "Publish it"},
                ],
            }
        ]
    }
    sid, prompt = open_question_session(mux, name, tool_input)
    log(name, "waiting for the State question")
    row = ensure_dialog(mux, sid, prompt, expect_headers(["State"]))
    question = questions_of(row)[0]
    labels = [o["label"] for o in question["options"]]
    if labels != ["Draft", "Review", "Live"] or question.get("multi_select"):
        raise CaseFailure(f"unexpected question shape: {question}")
    must_fail(mux, "questions", "answer", "state", "1,2", contains="takes one option")
    # Header match is case-insensitive. Option 3 is Live, which submits on its number.
    mux.run("questions", "answer", "state", "3")
    recorded = recorded_map(row_for(mux.questions(), sid) or {})
    if recorded.get("State", {}).get("options") != [3]:
        raise CaseFailure(f"recorded choice missing: {recorded}")
    log(name, "submitting")
    mux.run("questions", "submit")
    info = wait_settled(mux, sid)
    assert_answer(info, '"Which state should the example use?"="Live"')
    assert_not_waiting(mux, sid)


def test_multi(mux: Mux) -> None:
    name = "multi"
    tool_input = {
        "questions": [
            {
                "header": "Color",
                "question": "Which color should the badge use?",
                "multiSelect": False,
                "options": [
                    {"label": "Red", "description": "A red badge"},
                    {"label": "Blue", "description": "A blue badge"},
                ],
            },
            {
                "header": "Size",
                "question": "Which size should the badge use?",
                "multiSelect": False,
                "options": [
                    {"label": "Small", "description": "A small badge"},
                    {"label": "Large", "description": "A large badge"},
                ],
            },
        ]
    }
    sid, prompt = open_question_session(mux, name, tool_input)
    log(name, "waiting for both questions")
    ensure_dialog(mux, sid, prompt, expect_headers(["Color", "Size"]))
    mux.run("questions", "answer", "Color", "2")
    mid = recorded_map(row_for(mux.questions(), sid) or {})
    if set(mid) != {"Color"}:
        raise CaseFailure(f"expected only Color recorded so far: {mid}")
    mux.run("questions", "answer", "Size", "1")
    both = recorded_map(row_for(mux.questions(), sid) or {})
    if both.get("Color", {}).get("options") != [2] or both.get("Size", {}).get("options") != [1]:
        raise CaseFailure(f"both choices were not recorded: {both}")
    log(name, "submitting both, then the review confirmation")
    mux.run("questions", "submit")
    info = wait_settled(mux, sid)
    assert_answer(
        info,
        '"Which color should the badge use?"="Blue"',
        '"Which size should the badge use?"="Small"',
    )
    assert_not_waiting(mux, sid)


def test_checkbox(mux: Mux) -> None:
    name = "checkbox"
    tool_input = {
        "questions": [
            {
                "header": "Extras",
                "question": "Which extras should be included?",
                "multiSelect": True,
                "options": [
                    {"label": "Index", "description": "Add an index"},
                    {"label": "Seed", "description": "Add seed data"},
                    {"label": "Hooks", "description": "Add hooks"},
                ],
            }
        ]
    }
    sid, prompt = open_question_session(mux, name, tool_input)
    log(name, "waiting for the checkbox")
    row = ensure_dialog(mux, sid, prompt, expect_headers(["Extras"]))
    if not questions_of(row)[0].get("multi_select"):
        raise CaseFailure("Extras was not multi-select")
    must_fail(mux, "questions", "answer", "Extras", "1", "--notes", "nope", contains="takes no notes")
    mux.run("questions", "answer", "Extras", "1,3")
    recorded = recorded_map(row_for(mux.questions(), sid) or {})
    if recorded.get("Extras", {}).get("options") != [1, 3]:
        raise CaseFailure(f"checkbox choice was not recorded: {recorded}")
    log(name, "submitting Index and Hooks")
    mux.run("questions", "submit")
    info = wait_settled(mux, sid)
    body = assert_answer(info, '"Which extras should be included?"=')
    value = body.split('"Which extras should be included?"=', 1)[1]
    if "Index" not in value or "Hooks" not in value or "Seed" in value:
        raise CaseFailure(f"checkbox result was not Index and Hooks only\n{body}")
    assert_not_waiting(mux, sid)


def test_notes(mux: Mux) -> None:
    name = "notes"
    tool_input = {
        "questions": [
            {
                "header": "Layout",
                "question": "Which layout should the page use?",
                "multiSelect": False,
                "options": [
                    {
                        "label": "Stacked",
                        "description": "One column",
                        "preview": "<pre>stacked</pre>",
                    },
                    {
                        "label": "Split",
                        "description": "Two columns",
                        "preview": "<pre>split</pre>",
                    },
                ],
            }
        ]
    }
    sid, prompt = open_question_session(mux, name, tool_input)
    log(name, "waiting for the preview question")
    row = ensure_dialog(mux, sid, prompt, expect_headers(["Layout"]))
    previews = [o.get("preview") for o in questions_of(row)[0]["options"]]
    if not all(previews):
        raise CaseFailure(f"options have no preview, so notes cannot be tested: {previews}")

    def notes_visible(_status, _rows):
        return True if "add notes" in mux.peek(sid).lower() else None

    log(name, "waiting for the notes hint on screen")
    poll(mux, sid, notes_visible, 30, "add notes hint")
    mux.run("questions", "answer", "Layout", "2", "--notes", NOTE)
    recorded = recorded_map(row_for(mux.questions(), sid) or {})
    if recorded.get("Layout", {}).get("notes") != NOTE:
        raise CaseFailure(f"notes were not recorded: {recorded}")
    log(name, "submitting Split with notes")
    mux.run("questions", "submit")
    info = wait_settled(mux, sid)
    assert_answer(info, '"Which layout should the page use?"="Split"', f"notes: {NOTE}")
    assert_not_waiting(mux, sid)


def test_chat(mux: Mux) -> None:
    name = "chat"
    tool_input = {
        "questions": [
            {
                "header": "Topic",
                "question": "Which topic should the note cover?",
                "multiSelect": False,
                "options": [
                    {"label": "Rivers", "description": "Write about rivers"},
                    {"label": "Caves", "description": "Write about caves"},
                ],
            }
        ]
    }
    sid, prompt = open_question_session(mux, name, tool_input)
    log(name, "waiting for the topic question")
    ensure_dialog(mux, sid, prompt, expect_headers(["Topic"]))
    if len([r for r in mux.questions() if r.get("dialog")]) != 1:
        raise CaseFailure("chat needs exactly one waiting dialog")
    log(name, "choosing Chat about this")
    out = mux.run("questions", "chat", f"mention {CHAT}").stdout
    if f"{sid}  chat" not in out:
        raise CaseFailure(out)
    info = wait_settled(mux, sid)
    text = transcript(info)
    if CHAT not in text:
        raise CaseFailure(f"chat message never reached the session\n{text[-2000:]}")
    assert_not_waiting(mux, sid)


def test_permission(mux: Mux) -> None:
    name = "permission"
    cwd = mux.workdir(name)
    # `echo` is allowed without a prompt on this install. Removing a path is not.
    prompt = (
        "Use the Bash tool to run exactly this command and then stop: "
        f"rm -rf {PERM} && mkdir {PERM} && echo ok > {PERM}/out.txt"
    )
    log(name, "starting haiku so a bash command raises a permission dialog")
    sid = mux.create(
        cwd,
        prompt,
        name=name,
        trust=True,
        yolo=False,
        claude_args=("--effort", "low", "--tools", "Bash", "--disable-slash-commands", "--permission-mode", "manual"),
    )
    row = wait_for_dialog(mux, sid, lambda r: r.get("screen") and questions_of(r))
    question = questions_of(row)[0]
    must_fail(mux, "questions", "answer", question["question"], "1", "--notes", "nope", contains="takes no notes")
    must_fail(mux, "questions", "chat", "no chat here", contains="permission prompt")
    yes = option_number(question, "yes")
    log(name, f"allowing {question['question']!r}")
    mux.run("questions", "answer", question["question"], str(yes))
    mux.run("questions", "submit")
    info = wait_settled(mux, sid)
    wrote = Path(info["cwd"]) / PERM / "out.txt"
    if wrote.read_text().strip() != "ok":
        raise CaseFailure(f"bash did not run after the dialog; missing {wrote}")
    assert_not_waiting(mux, sid)


CASES = [
    ("trust-yes", test_trust_yes),
    ("trust-no", test_trust_no),
    ("single", test_single),
    ("multi", test_multi),
    ("checkbox", test_checkbox),
    ("notes", test_notes),
    ("chat", test_chat),
    ("permission", test_permission),
]


def build() -> Path:
    print("building claude-mux", file=sys.stderr, flush=True)
    subprocess.run(["cargo", "build", "--bins", "--offline"], cwd=ROOT, check=True)
    binary = ROOT / "target" / "debug" / "claude-mux"
    if not binary.is_file():
        raise SystemExit(f"missing {binary}")
    if shutil.which("claude") is None:
        raise SystemExit("claude is not on PATH")
    return binary


def main(argv: list[str]) -> int:
    names = [name for name, _fn in CASES]
    unknown = [a for a in argv if a not in names]
    if unknown:
        raise SystemExit(f"unknown case {unknown[0]!r}; choose from: {', '.join(names)}")
    selected = argv or names
    binary = build()
    parent = Path(tempfile.mkdtemp(prefix="claude-mux-e2e-"))
    home = parent / "home"
    home.mkdir()
    print(f"e2e home: {parent}", file=sys.stderr, flush=True)
    print(f"model: {MODEL} --effort low", file=sys.stderr, flush=True)
    mux = Mux(binary, home)
    failed: list[str] = []
    try:
        empty = mux.run("questions", check=False)
        if empty.returncode != 0 or "no sessions waiting for input" not in empty.stderr:
            raise CaseFailure(f"empty questions listing failed\n{empty.stderr}\n{empty.stdout}")
        must_fail(mux, "questions", "submit", contains="nothing to submit")
        for name, fn in CASES:
            if name not in selected:
                continue
            started = time.monotonic()
            try:
                fn(mux)
            except Exception as e:  # noqa: BLE001
                failed.append(name)
                print(f"FAIL {name}: {e}", file=sys.stderr, flush=True)
                sid = None
                try:
                    rows = mux.questions()
                    sid = next((r["id"] for r in rows if r.get("description") == name or (r.get("session") or {}).get("description") == name), None)
                except Exception:
                    sid = None
                if sid is None:
                    listing = mux.run("list", "--json", check=False).stdout
                    try:
                        sid = next((s["id"] for s in json.loads(listing or "[]") if s.get("description") == name), None)
                    except json.JSONDecodeError:
                        sid = None
                mux.dump(name, sid, e)
                if sid:
                    mux.rm(sid)
                continue
            print(f"PASS {name} ({time.monotonic() - started:.0f}s)", flush=True)
    finally:
        mux.shutdown()
    if failed:
        print(f"failed: {', '.join(failed)}", file=sys.stderr)
        print(f"dumps: {parent / 'artifacts'}", file=sys.stderr)
        return 1
    print(f"all {len(selected)} cases passed")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
