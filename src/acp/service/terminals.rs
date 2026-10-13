use super::tool_locations::absolute_tool_path;
use agent_client_protocol::schema::v1::CreateTerminalRequest;
use agent_client_protocol::schema::v1::KillTerminalRequest;
use agent_client_protocol::schema::v1::ReleaseTerminalRequest;
use agent_client_protocol::schema::v1::SessionNotification;
use agent_client_protocol::schema::v1::SessionUpdate;
use agent_client_protocol::schema::v1::Terminal;
use agent_client_protocol::schema::v1::TerminalOutputRequest;
use agent_client_protocol::schema::v1::ToolCallContent;
use agent_client_protocol::schema::v1::ToolCallUpdate;
use agent_client_protocol::schema::v1::ToolCallUpdateFields;
use agent_client_protocol::schema::v1::WaitForTerminalExitRequest;
use agent_client_protocol::Client;
use agent_client_protocol::ConnectionTo;
use std::path::Path;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

pub(super) async fn bridge_terminal_session(
    connection: &ConnectionTo<Client>,
    session_id: &str,
    session_cwd: &Path,
    request: crate::tools::TerminalSessionRequest,
    cancellation: &CancellationToken,
) {
    let start = request.start;
    let control_tx = request.control_tx;
    let cwd = start
        .workdir
        .as_deref()
        .map(PathBuf::from)
        .map(|path| absolute_tool_path(&path.to_string_lossy(), session_cwd))
        .unwrap_or_else(|| session_cwd.to_path_buf());
    let create = CreateTerminalRequest::new(session_id.to_string(), "bash")
        .args(vec!["-c".to_string(), start.command.clone()])
        .cwd(cwd)
        .output_byte_limit(crate::tools::terminal_session::MAX_TRANSCRIPT_BYTES as u64);
    let create_request = connection.send_request(create).block_task();
    tokio::pin!(create_request);
    let terminal_id = match tokio::select! {
        _ = cancellation.cancelled() => {
            let _ = control_tx.send(crate::tools::TerminalSessionControl::ExternalResult(
                crate::tools::terminal_session::external_terminal_result(
                    &start, "", false, None, true,
                ),
            ));
            return;
        }
        response = &mut create_request => response,
    } {
        Ok(response) => response.terminal_id,
        Err(error) => {
            let _ = control_tx.send(crate::tools::TerminalSessionControl::ExternalError(
                format!("ACP client could not create terminal: {error}"),
            ));
            return;
        }
    };

    let terminal_update = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        start.tool_call_id.clone(),
        ToolCallUpdateFields::new().content(vec![ToolCallContent::Terminal(Terminal::new(
            terminal_id.clone(),
        ))]),
    ));
    if connection
        .send_notification(SessionNotification::new(
            session_id.to_string(),
            terminal_update,
        ))
        .is_err()
    {
        let _ = connection
            .send_request(ReleaseTerminalRequest::new(
                session_id.to_string(),
                terminal_id,
            ))
            .block_task()
            .await;
        let _ = control_tx.send(crate::tools::TerminalSessionControl::ExternalError(
            "ACP client could not embed terminal".to_string(),
        ));
        return;
    }

    let wait = connection
        .send_request(WaitForTerminalExitRequest::new(
            session_id.to_string(),
            terminal_id.clone(),
        ))
        .block_task();
    tokio::pin!(wait);
    let (stopped_by_user, exit_code, wait_error) = tokio::select! {
        _ = cancellation.cancelled() => {
            let _ = connection
                .send_request(KillTerminalRequest::new(session_id.to_string(), terminal_id.clone()))
                .block_task()
                .await;
            (true, None, None)
        }
        response = &mut wait => match response {
            Ok(response) => (
                false,
                response.exit_status.exit_code.and_then(|code| i32::try_from(code).ok()),
                None,
            ),
            Err(error) => (false, None, Some(error.to_string())),
        }
    };

    let output = connection
        .send_request(TerminalOutputRequest::new(
            session_id.to_string(),
            terminal_id.clone(),
        ))
        .block_task()
        .await;
    let _ = connection
        .send_request(ReleaseTerminalRequest::new(
            session_id.to_string(),
            terminal_id,
        ))
        .block_task()
        .await;

    match (wait_error, output) {
        (None, Ok(output)) => {
            let result = crate::tools::terminal_session::external_terminal_result(
                &start,
                &output.output,
                output.truncated,
                exit_code,
                stopped_by_user,
            );
            let _ = control_tx.send(crate::tools::TerminalSessionControl::ExternalResult(result));
        }
        (Some(error), _) => {
            let _ = control_tx.send(crate::tools::TerminalSessionControl::ExternalError(
                format!("ACP terminal wait failed: {error}"),
            ));
        }
        (None, Err(error)) => {
            let _ = control_tx.send(crate::tools::TerminalSessionControl::ExternalError(
                format!("ACP terminal output failed: {error}"),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::AcpService;

    use std::path::Path;

    #[test]
    fn terminal_support_tracks_client_capability() {
        let service = AcpService::new(Path::new("/tmp")).unwrap();
        assert!(!service.supports_terminals());

        service.set_client_capabilities(
            agent_client_protocol::schema::v1::ClientCapabilities::new().terminal(true),
        );
        assert!(service.supports_terminals());
    }
}
