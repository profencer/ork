"""Lock snake_case A2A JSON shapes to `ork-a2a` (see `crates/ork-a2a/tests/fixtures/`)."""

from __future__ import annotations

import json
from pathlib import Path

from demo_langgraph_agent.a2a_types import (
    Message,
    PartText,
    Role,
    Task,
    TaskState,
    TaskStatus,
    TaskStatusUpdateEvent,
    sse_task_event_dict,
)

REPO = Path(__file__).resolve().parents[3]
A2A_FIX = REPO / "crates" / "ork-a2a" / "tests" / "fixtures" / "task_lifecycle.json"


def test_task_from_ork_a2a_fixture() -> None:
    raw = A2A_FIX.read_text()
    t = Task.model_validate_json(raw)
    assert t.id
    assert t.status.state == TaskState.working
    assert len(t.history) == 3
    rep = t.model_dump_json()
    t2 = Task.model_validate_json(rep)
    assert t2 == t


def test_task_status_event_final_key() -> None:
    e = TaskStatusUpdateEvent(
        task_id="22222222-2222-7222-8222-222222222201",
        status=TaskStatus(state=TaskState.working, message="x"),
        is_final=False,
    )
    d = sse_task_event_dict(e)
    assert d["kind"] == "status_update"
    assert "final" in d
    assert d["final"] is False
    # matches serde `is_final` -> `final` in ork
    s = json.dumps(d, separators=(",", ":"))
    assert '"final":false' in s


def test_message_roundtrip() -> None:
    m = Message(
        role=Role.user,
        parts=[PartText(text="hi")],
        message_id="11111111-1111-7111-8111-111111111111",
    )
    j = m.model_dump_json()
    m2 = Message.model_validate_json(j)
    assert m2.role == m.role
    assert isinstance(m2.parts[0], PartText)
