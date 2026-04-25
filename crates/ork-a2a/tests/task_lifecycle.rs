use ork_a2a::{
    Artifact, Message, Part, Task, TaskArtifactUpdateEvent, TaskEvent, TaskId, TaskState,
    TaskStatus, TaskStatusUpdateEvent,
};

const TASK_FIXTURE: &str = include_str!("fixtures/task_lifecycle.json");

#[test]
fn task_mixed_parts_roundtrips() {
    let v: serde_json::Value = serde_json::from_str(TASK_FIXTURE).expect("parse task JSON");
    let task: Task = serde_json::from_value(v).expect("deserialize Task");
    assert_eq!(task.history.len(), 3);
    assert_eq!(task.status.state, TaskState::Working);
    let w = serde_json::to_value(&task).expect("serialize Task");
    let again: Task = serde_json::from_value(w.clone()).expect("deserialize again");
    assert_eq!(w, serde_json::to_value(&again).unwrap());
}

#[test]
fn task_event_status_and_artifact_roundtrips() {
    let st = TaskEvent::StatusUpdate(TaskStatusUpdateEvent {
        task_id: "22222222-2222-7222-8222-222222222201"
            .parse::<TaskId>()
            .expect("task id"),
        status: TaskStatus {
            state: TaskState::InputRequired,
            message: None,
        },
        is_final: false,
    });
    let art = TaskEvent::ArtifactUpdate(TaskArtifactUpdateEvent {
        task_id: "22222222-2222-7222-8222-222222222202"
            .parse::<TaskId>()
            .expect("task id"),
        artifact: Artifact {
            artifact_id: "z".to_string(),
            name: None,
            description: None,
            parts: vec![Part::text("x")],
            metadata: None,
        },
    });
    for ev in [st, art] {
        let j = serde_json::to_value(&ev).expect("task event to json");
        let e2: TaskEvent = serde_json::from_value(j.clone()).expect("from json");
        assert_eq!(j, serde_json::to_value(&e2).unwrap());
    }
}

#[test]
fn task_event_message_kind_roundtrips() {
    let m = Message::user_text("ping");
    let ev = TaskEvent::Message(m);
    let j = serde_json::to_value(&ev).expect("to json");
    let e2: TaskEvent = serde_json::from_value(j.clone()).expect("from json");
    assert_eq!(j, serde_json::to_value(&e2).unwrap());
}
