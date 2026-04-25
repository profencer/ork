use ork_a2a::AgentCard;

const FIXTURE: &str = include_str!("fixtures/agent_card.json");

#[test]
fn agent_card_golden_json_roundtrips() {
    let card: AgentCard = serde_json::from_str(FIXTURE).expect("fixture deserializes to AgentCard");
    let v = serde_json::to_value(&card).expect("serialize");
    let again: AgentCard = serde_json::from_value(v.clone()).expect("deserialize again");
    let v2 = serde_json::to_value(&again).expect("serialize again");
    assert_eq!(
        v, v2,
        "serde round-trip through Value is idempotent (stable wire JSON)"
    );
}
