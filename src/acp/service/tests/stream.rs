use super::*;
use agent_client_protocol::schema::v1::StopReason;

#[test]
fn maps_typed_turn_stop_reasons_to_acp() {
    assert_eq!(
        acp_stop_reason(Some(crate::llm::TurnStopReason::MaxTokens)),
        StopReason::MaxTokens
    );
    assert_eq!(
        acp_stop_reason(Some(crate::llm::TurnStopReason::Refusal)),
        StopReason::Refusal
    );
    assert_eq!(
        acp_stop_reason(Some(crate::llm::TurnStopReason::MaxTurnRequests)),
        StopReason::MaxTurnRequests
    );
    assert_eq!(
        acp_stop_reason(Some(crate::llm::TurnStopReason::MaxTurnRequests)),
        StopReason::MaxTurnRequests
    );
    assert_eq!(acp_stop_reason(None), StopReason::EndTurn);
}
