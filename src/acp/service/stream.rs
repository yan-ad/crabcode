use agent_client_protocol::schema::v1::StopReason;

pub(super) fn acp_stop_reason(reason: Option<crate::llm::TurnStopReason>) -> StopReason {
    match reason {
        Some(crate::llm::TurnStopReason::MaxTokens) => StopReason::MaxTokens,
        Some(crate::llm::TurnStopReason::MaxTurnRequests) => StopReason::MaxTurnRequests,
        Some(crate::llm::TurnStopReason::Refusal) => StopReason::Refusal,
        None => StopReason::EndTurn,
    }
}

#[cfg(test)]
#[path = "tests/stream.rs"]
mod tests;
