"""Pydantic mirrors of `crates/ork-a2a` wire JSON (snake_case; matches ork's serde, not a2a-sdk)."""

from __future__ import annotations

from enum import StrEnum
from typing import Any, Literal

from pydantic import BaseModel, Field, TypeAdapter


class TaskState(StrEnum):
    submitted = "submitted"
    working = "working"
    input_required = "input_required"
    auth_required = "auth_required"
    completed = "completed"
    failed = "failed"
    canceled = "canceled"
    rejected = "rejected"


class Role(StrEnum):
    user = "user"
    agent = "agent"


class PartText(BaseModel):
    kind: Literal["text"] = "text"
    text: str
    metadata: dict[str, Any] | None = None


class PartData(BaseModel):
    kind: Literal["data"] = "data"
    data: Any
    metadata: dict[str, Any] | None = None


class FileUri(BaseModel):
    name: str | None = None
    mime_type: str | None = None
    uri: str


class PartFile(BaseModel):
    kind: Literal["file"] = "file"
    file: FileUri
    metadata: dict[str, Any] | None = None


class Message(BaseModel):
    role: Role
    parts: list[PartText | PartData | PartFile] = Field(default_factory=list)
    message_id: str
    task_id: str | None = None
    context_id: str | None = None
    metadata: dict[str, Any] | None = None


class TaskStatus(BaseModel):
    state: TaskState
    message: str | None = None


class Artifact(BaseModel):
    artifact_id: str
    name: str | None = None
    description: str | None = None
    parts: list[PartText | PartData | PartFile] = Field(default_factory=list)
    metadata: dict[str, Any] | None = None


class Task(BaseModel):
    id: str
    context_id: str
    status: TaskStatus
    history: list[Message] = Field(default_factory=list)
    artifacts: list[Artifact] = Field(default_factory=list)
    metadata: dict[str, Any] | None = None


# --- Agent card ---


class AgentCapabilities(BaseModel):
    streaming: bool
    push_notifications: bool
    state_transition_history: bool


class AgentSkill(BaseModel):
    id: str
    name: str
    description: str
    tags: list[str]
    examples: list[str]
    input_modes: list[str] | None = None
    output_modes: list[str] | None = None


class AgentCard(BaseModel):
    name: str
    description: str
    version: str
    url: str
    provider: Any | None = None
    capabilities: AgentCapabilities
    default_input_modes: list[str]
    default_output_modes: list[str]
    skills: list[AgentSkill]
    security_schemes: dict[str, Any] | None = None
    security: list[dict[str, list[str]]] | None = None
    extensions: list[dict[str, Any]] | None = None


class MessageSendParams(BaseModel):
    message: Message
    configuration: dict[str, Any] | None = None
    metadata: dict[str, Any] | None = None


class TaskStatusUpdateEvent(BaseModel):
    """`TaskEvent::StatusUpdate` body (kind is added in sse_task_event_dict)."""

    task_id: str
    status: TaskStatus
    is_final: bool = False


class TaskArtifactUpdateEvent(BaseModel):
    task_id: str
    artifact: Artifact


def sse_task_event_dict(ev: Any) -> dict[str, Any]:
    """
    Map TaskEvent to JSON for SSE `data:` line — matches `ork_a2a::TaskEvent` serde
    (tag=kind, snake_case, status update uses `final` not is_final).
    """
    if isinstance(ev, TaskStatusUpdateEvent):
        return {
            "kind": "status_update",
            "task_id": ev.task_id,
            "status": ev.status.model_dump(mode="json", exclude_none=True),
            "final": ev.is_final,
        }
    if isinstance(ev, TaskArtifactUpdateEvent):
        return {
            "kind": "artifact_update",
            "task_id": ev.task_id,
            "artifact": ev.artifact.model_dump(mode="json", exclude_none=True),
        }
    if isinstance(ev, Message):
        return {
            "kind": "message",
            "role": ev.role,
            "parts": [p.model_dump(mode="json", exclude_none=True) for p in ev.parts],
            "message_id": ev.message_id,
            "task_id": ev.task_id,
            "context_id": ev.context_id,
            "metadata": ev.metadata,
        }
    raise TypeError(f"unknown task event: {ev!r}")


# JSON-RPC

JsonRpcId = int | str | None


class JsonRpcRequestGeneric(BaseModel):
    jsonrpc: str = "2.0"
    id: JsonRpcId = None
    method: str
    params: Any | None = None


class JsonRpcErrorBody(BaseModel):
    code: int
    method: str = ""
    message: str
    data: Any | None = None


def jsonrpc_error(id_val: JsonRpcId, code: int, message: str) -> dict[str, Any]:
    return {
        "jsonrpc": "2.0",
        "id": id_val,
        "error": {"code": code, "message": message},
    }


def part_from_payload(p: Any) -> PartText | PartData | PartFile:
    return TypeAdapter(PartText | PartData | PartFile).validate_python(p)


SendMessageResult = Task | Message
