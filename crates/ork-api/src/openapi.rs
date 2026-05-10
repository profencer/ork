//! ADR-0056 §`OpenAPI emission`: hand-built OpenAPI 3.0 emitter.
//!
//! The emitter walks `app.agents()`, `app.workflows()`, and
//! `app.tools()`, contributing one operation per registered component.
//! Per-DTO JSON Schemas are produced with [`schemars`] and stitched
//! into `components/schemas/...`.
//!
//! Pure function: `manifest -> OpenApiDoc`. Snapshot-tested in
//! `tests/openapi_snapshot.rs`.

use std::collections::BTreeMap;

use indexmap::IndexMap;
use openapiv3::{
    Components, Contact, Info, MediaType, OpenAPI, Operation, Parameter, ParameterData,
    ParameterSchemaOrContent, PathItem, Paths, QueryStyle, ReferenceOr, RequestBody, Response,
    Responses, Schema, SchemaKind, StringType, Type,
};
use ork_app::OrkApp;
use schemars::r#gen::{SchemaGenerator, SchemaSettings};
use serde_json::Value;

use crate::dto::{
    AgentDetail, AgentGenerateInput, AgentGenerateOutput, AgentSummary, AppendMessageInput,
    AppendMessageOutput, OkResponse, ScorerBindingSummary, ScorerRowList, ThreadSummaryDto,
    ToolDetail, ToolInvokeInput, ToolInvokeOutput, ToolSummary, WorkflowDetail, WorkflowRunInput,
    WorkflowRunStarted, WorkflowSummary, WorkingMemoryRead, WorkingMemoryWrite,
};
use crate::error::ErrorEnvelope;

const OPENAPI_VERSION: &str = "3.0.3";

/// Build an OpenAPI document for the given OrkApp.
#[must_use]
pub fn openapi_spec(app: &OrkApp) -> OpenAPI {
    let mut paths_map: BTreeMap<String, ReferenceOr<PathItem>> = BTreeMap::new();
    let mut components = Components::default();

    register_dto_schemas(&mut components);

    // Static routes
    paths_map.insert(
        "/api/manifest".into(),
        ReferenceOr::Item(static_manifest_path()),
    );
    paths_map.insert("/healthz".into(), ReferenceOr::Item(static_health_path()));

    // /api/agents
    paths_map.insert(
        "/api/agents".into(),
        ReferenceOr::Item(static_list_path("AgentSummary", "list registered agents")),
    );
    paths_map.insert(
        "/api/workflows".into(),
        ReferenceOr::Item(static_list_path(
            "WorkflowSummary",
            "list registered workflows",
        )),
    );
    paths_map.insert(
        "/api/tools".into(),
        ReferenceOr::Item(static_list_path("ToolSummary", "list registered tools")),
    );
    paths_map.insert(
        "/api/scorers".into(),
        ReferenceOr::Item(static_list_path(
            "ScorerBindingSummary",
            "list registered scorer bindings",
        )),
    );
    paths_map.insert(
        "/api/scorer-results".into(),
        ReferenceOr::Item(static_object_path(
            "ScorerRowList",
            "list recent scorer rows",
        )),
    );

    // Per-agent routes
    let mut agent_ids: Vec<&str> = app.agents().map(|(id, _)| id).collect();
    agent_ids.sort();
    for id in agent_ids {
        let detail_path = format!("/api/agents/{id}");
        let generate_path = format!("/api/agents/{id}/generate");
        let stream_path = format!("/api/agents/{id}/stream");
        paths_map.insert(
            detail_path,
            ReferenceOr::Item(static_object_path("AgentDetail", "get agent detail")),
        );
        paths_map.insert(
            generate_path,
            ReferenceOr::Item(post_path(
                "AgentGenerateInput",
                "AgentGenerateOutput",
                "invoke an agent and return the final assistant message",
            )),
        );
        paths_map.insert(
            stream_path,
            ReferenceOr::Item(post_sse_path(
                "AgentGenerateInput",
                "stream incremental events for an agent run (SSE)",
            )),
        );
    }

    // Per-workflow routes
    let mut wf_ids: Vec<&str> = app.workflows().map(|(id, _)| id).collect();
    wf_ids.sort();
    for id in wf_ids {
        paths_map.insert(
            format!("/api/workflows/{id}"),
            ReferenceOr::Item(static_object_path("WorkflowDetail", "get workflow detail")),
        );
        paths_map.insert(
            format!("/api/workflows/{id}/run"),
            ReferenceOr::Item(post_path(
                "WorkflowRunInput",
                "WorkflowRunStarted",
                "start a workflow run; returns run_id immediately",
            )),
        );
    }

    // Per-tool routes
    let mut tool_ids: Vec<&str> = app.tools().map(|(id, _)| id).collect();
    tool_ids.sort();
    for id in tool_ids {
        paths_map.insert(
            format!("/api/tools/{id}"),
            ReferenceOr::Item(static_object_path("ToolDetail", "get tool detail")),
        );
        paths_map.insert(
            format!("/api/tools/{id}/invoke"),
            ReferenceOr::Item(post_path(
                "ToolInvokeInput",
                "ToolInvokeOutput",
                "invoke a tool",
            )),
        );
    }

    // Memory + working
    paths_map.insert(
        "/api/memory/threads".into(),
        ReferenceOr::Item(static_list_path("ThreadSummaryDto", "list memory threads")),
    );
    paths_map.insert(
        "/api/memory/working".into(),
        ReferenceOr::Item(working_memory_path()),
    );

    let info = Info {
        title: "ork — auto-generated REST + SSE surface".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        description: Some(
            "Auto-generated from OrkApp::manifest() per ADR-0056. Routes are a pure function of the registered components; refactoring an agent updates this document on the next boot."
                .into(),
        ),
        contact: Some(Contact {
            name: Some("ork".into()),
            url: Some("https://github.com/anthropic/ork".into()),
            email: None,
            extensions: Default::default(),
        }),
        ..Default::default()
    };

    let paths = Paths {
        paths: paths_map.into_iter().collect(),
        ..Default::default()
    };

    OpenAPI {
        openapi: OPENAPI_VERSION.into(),
        info,
        paths,
        components: Some(components),
        ..Default::default()
    }
}

fn register_dto_schemas(components: &mut Components) {
    let mut schema_gen = SchemaGenerator::new(SchemaSettings::draft07());
    macro_rules! add {
        ($t:ty, $name:literal) => {
            let schema = schema_gen.subschema_for::<$t>();
            let val = serde_json::to_value(&schema).unwrap_or(Value::Null);
            components
                .schemas
                .insert($name.into(), ReferenceOr::Item(value_to_schema(val)));
        };
    }
    add!(AgentSummary, "AgentSummary");
    add!(AgentDetail, "AgentDetail");
    add!(AgentGenerateInput, "AgentGenerateInput");
    add!(AgentGenerateOutput, "AgentGenerateOutput");
    add!(WorkflowSummary, "WorkflowSummary");
    add!(WorkflowDetail, "WorkflowDetail");
    add!(WorkflowRunInput, "WorkflowRunInput");
    add!(WorkflowRunStarted, "WorkflowRunStarted");
    add!(ToolSummary, "ToolSummary");
    add!(ToolDetail, "ToolDetail");
    add!(ToolInvokeInput, "ToolInvokeInput");
    add!(ToolInvokeOutput, "ToolInvokeOutput");
    add!(ThreadSummaryDto, "ThreadSummaryDto");
    add!(AppendMessageInput, "AppendMessageInput");
    add!(AppendMessageOutput, "AppendMessageOutput");
    add!(WorkingMemoryRead, "WorkingMemoryRead");
    add!(WorkingMemoryWrite, "WorkingMemoryWrite");
    add!(OkResponse, "OkResponse");
    add!(ScorerBindingSummary, "ScorerBindingSummary");
    add!(ScorerRowList, "ScorerRowList");
    add!(ErrorEnvelope, "ErrorEnvelope");
    let _ = schema_gen;

    // ADR-0056 §`OpenAPI emission`: `ork_app::AppManifest` lives in
    // a crate that does not derive `JsonSchema` (would cascade derives
    // through `ork_eval` and `ork_a2a`). Hand-write a passthrough
    // schema referencing the per-summary types we already registered.
    components.schemas.insert(
        "AppManifest".into(),
        ReferenceOr::Item(value_to_schema(serde_json::json!({
            "type": "object",
            "description": "Snapshot of the registered components on this OrkApp (ADR-0049).",
            "properties": {
                "environment": { "type": "string", "enum": ["development", "staging", "production"] },
                "agents": { "type": "array", "items": { "$ref": "#/components/schemas/AgentSummary" } },
                "workflows": { "type": "array", "items": { "$ref": "#/components/schemas/WorkflowSummary" } },
                "tools": { "type": "array", "items": { "$ref": "#/components/schemas/ToolSummary" } },
                "scorers": { "type": "array", "items": { "$ref": "#/components/schemas/ScorerBindingSummary" } },
                "ork_version": { "type": "string" },
                "built_at": { "type": "string", "format": "date-time" }
            }
        }))),
    );
}

fn value_to_schema(v: Value) -> Schema {
    Schema {
        schema_data: Default::default(),
        schema_kind: SchemaKind::Any(serde_json::from_value(v).unwrap_or_default()),
    }
}

fn json_response(schema_name: &str, description: &str) -> Response {
    let mut content = IndexMap::new();
    content.insert(
        "application/json".into(),
        MediaType {
            schema: Some(ReferenceOr::Reference {
                reference: format!("#/components/schemas/{schema_name}"),
            }),
            ..Default::default()
        },
    );
    Response {
        description: description.into(),
        content,
        ..Default::default()
    }
}

fn sse_response(description: &str) -> Response {
    let mut content = IndexMap::new();
    content.insert(
        "text/event-stream".into(),
        MediaType {
            schema: Some(ReferenceOr::Item(Schema {
                schema_data: Default::default(),
                schema_kind: SchemaKind::Type(Type::String(StringType::default())),
            })),
            ..Default::default()
        },
    );
    Response {
        description: description.into(),
        content,
        ..Default::default()
    }
}

fn json_request_body(schema_name: &str) -> RequestBody {
    let mut content = IndexMap::new();
    content.insert(
        "application/json".into(),
        MediaType {
            schema: Some(ReferenceOr::Reference {
                reference: format!("#/components/schemas/{schema_name}"),
            }),
            ..Default::default()
        },
    );
    RequestBody {
        content,
        required: true,
        ..Default::default()
    }
}

fn ok_responses(response_schema: &str, description: &str) -> Responses {
    let mut responses = IndexMap::new();
    responses.insert(
        openapiv3::StatusCode::Code(200),
        ReferenceOr::Item(json_response(response_schema, description)),
    );
    Responses {
        responses,
        ..Default::default()
    }
}

fn sse_responses(description: &str) -> Responses {
    let mut responses = IndexMap::new();
    responses.insert(
        openapiv3::StatusCode::Code(200),
        ReferenceOr::Item(sse_response(description)),
    );
    Responses {
        responses,
        ..Default::default()
    }
}

fn static_manifest_path() -> PathItem {
    PathItem {
        get: Some(Operation {
            summary: Some("get the application manifest".into()),
            responses: ok_responses("AppManifest", "AppManifest snapshot"),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn static_health_path() -> PathItem {
    PathItem {
        get: Some(Operation {
            summary: Some("liveness".into()),
            responses: {
                let mut r = IndexMap::new();
                r.insert(
                    openapiv3::StatusCode::Code(200),
                    ReferenceOr::Item(Response {
                        description: "ok".into(),
                        ..Default::default()
                    }),
                );
                Responses {
                    responses: r,
                    ..Default::default()
                }
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn static_list_path(item_schema: &str, summary: &str) -> PathItem {
    PathItem {
        get: Some(Operation {
            summary: Some(summary.into()),
            responses: ok_responses(item_schema, "list of items"),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn static_object_path(schema: &str, summary: &str) -> PathItem {
    PathItem {
        get: Some(Operation {
            summary: Some(summary.into()),
            responses: ok_responses(schema, schema),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn post_path(req_schema: &str, resp_schema: &str, summary: &str) -> PathItem {
    PathItem {
        post: Some(Operation {
            summary: Some(summary.into()),
            request_body: Some(ReferenceOr::Item(json_request_body(req_schema))),
            responses: ok_responses(resp_schema, resp_schema),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn post_sse_path(req_schema: &str, summary: &str) -> PathItem {
    PathItem {
        post: Some(Operation {
            summary: Some(summary.into()),
            request_body: Some(ReferenceOr::Item(json_request_body(req_schema))),
            responses: sse_responses("server-sent event stream"),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn working_memory_path() -> PathItem {
    let resource_param = ReferenceOr::Item(Parameter::Query {
        parameter_data: ParameterData {
            name: "resource".into(),
            description: Some("ResourceId".into()),
            required: true,
            deprecated: None,
            format: ParameterSchemaOrContent::Schema(ReferenceOr::Item(Schema {
                schema_data: Default::default(),
                schema_kind: SchemaKind::Type(Type::String(StringType::default())),
            })),
            example: None,
            examples: Default::default(),
            explode: None,
            extensions: Default::default(),
        },
        allow_reserved: false,
        style: QueryStyle::default(),
        allow_empty_value: None,
    });
    PathItem {
        get: Some(Operation {
            summary: Some("read working memory".into()),
            parameters: vec![resource_param.clone()],
            responses: ok_responses("WorkingMemoryRead", "WorkingMemoryRead"),
            ..Default::default()
        }),
        put: Some(Operation {
            summary: Some("write working memory".into()),
            parameters: vec![resource_param],
            request_body: Some(ReferenceOr::Item(json_request_body("WorkingMemoryWrite"))),
            responses: ok_responses("OkResponse", "OkResponse"),
            ..Default::default()
        }),
        ..Default::default()
    }
}
