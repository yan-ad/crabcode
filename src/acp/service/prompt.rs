use super::attachments::model_supports_audio;
use super::attachments::prompt_content;
use super::attachments::ManagedAttachmentGuard;
use super::commands::command_text;
use super::commands::compact_command;
use super::commands::expand_slash_command;
use super::commands::SlashExpansion;
use super::config::find_selectable_model;
use super::config::model_context_window;
use super::config::resolved_reasoning;
use super::internal_error;
use super::internal_error_with;
use super::notifications::send_replay_text;
use super::notifications::send_text;
use super::notifications::send_tool_call;
use super::notifications::send_tool_result;
use super::permissions::request_permission;
use super::permissions::tool_permissions;
use super::questions::request_questions;
use super::questions::skipped_question_answers;
use super::stream::acp_stop_reason;
use super::stream::record_tool_call;
use super::stream::record_tool_result;
use super::terminals::bridge_terminal_session;
use super::usage::acp_context_tokens;
use super::usage::acp_session_cost;
use super::usage::cumulative_cost;
use super::usage::estimate_session_usage_cost;
use super::usage::send_live_usage;
use super::usage::send_usage;
use super::usage::AcpLiveUsage;
use super::AcpService;
use agent_client_protocol::schema::v1::ContentBlock;
use agent_client_protocol::schema::v1::PromptResponse;
use agent_client_protocol::schema::v1::StopReason;
use agent_client_protocol::Client;
use agent_client_protocol::ConnectionTo;
use agent_client_protocol::Error;
use tokio_util::sync::CancellationToken;

pub(super) fn acp_request_messages(
    history: &[crate::session::types::Message],
    system_message: crate::session::types::Message,
) -> Vec<crate::session::types::Message> {
    let mut messages = crate::session::compaction::filter_messages_for_context(history);
    messages.insert(0, system_message);
    messages
}

impl AcpService {
    pub async fn prompt(
        &self,
        session_id: String,
        prompt: Vec<ContentBlock>,
        connection: ConnectionTo<Client>,
    ) -> Result<PromptResponse, Error> {
        let mut session = self
            .sessions
            .lock()
            .await
            .get(&session_id)
            .cloned()
            .ok_or_else(|| Error::invalid_params().data("unknown session"))?;
        let compact_text = command_text(&prompt);
        if compact_command(&compact_text)? {
            if prompt
                .iter()
                .any(|part| !matches!(part, ContentBlock::Text(_)))
            {
                return Err(Error::invalid_params().data("/compact does not accept attachments"));
            }
            return self.compact_session(&session_id, session, connection).await;
        }
        let expansion = if compact_text.trim_start().starts_with('/') {
            Some(expand_slash_command(&session, &compact_text).await?)
        } else {
            None
        };
        let expanded_prompt = match expansion {
            Some(SlashExpansion::LocalResult(text)) => {
                if prompt
                    .iter()
                    .any(|part| !matches!(part, ContentBlock::Text(_)))
                {
                    return Err(Error::invalid_params()
                        .data("local ACP commands do not accept attachments"));
                }
                let message_id = cuid2::create_id();
                send_replay_text(&connection, &session_id, &message_id, &text, false, false)?;
                return Ok(PromptResponse::new(StopReason::EndTurn));
            }
            Some(SlashExpansion::Prompt {
                prompt,
                agent,
                model,
            }) => {
                if let Some(agent) = agent {
                    if session
                        .config
                        .merged_config
                        .agent_registry
                        .get(&agent)
                        .is_none()
                    {
                        return Err(Error::invalid_params()
                            .data(format!("custom command references unknown agent: {agent}")));
                    }
                    session.agent = agent;
                }
                if let Some(model_ref) = model {
                    let (provider, model) = crate::app::parse_model_ref(&model_ref);
                    let canonical = format!("{provider}/{model}");
                    let model = find_selectable_model(&session.models, &canonical)?;
                    session.provider.clone_from(&model.provider_id);
                    session.model.clone_from(&model.id);
                    session.reasoning = resolved_reasoning(&session, session.reasoning_selection);
                    session.context_window = model_context_window(
                        &session.config,
                        &session.models,
                        &session.provider,
                        &session.model,
                    );
                }
                Some(prompt)
            }
            Some(SlashExpansion::Btw(question)) => {
                if prompt
                    .iter()
                    .any(|part| !matches!(part, ContentBlock::Text(_)))
                {
                    return Err(Error::invalid_params().data("/btw does not accept attachments"));
                }
                return self
                    .btw_session(&session_id, session, question, connection)
                    .await;
            }
            None => None,
        };
        let supports_images = session
            .models
            .iter()
            .find(|model| model.provider_id == session.provider && model.id == session.model)
            .is_some_and(|model| model.attachment);
        let supports_audio =
            model_supports_audio(&session.config, &session.provider, &session.model);
        let (prompt, local_image_paths, local_audio_paths) = prompt_content(
            prompt,
            expanded_prompt.as_deref(),
            supports_images,
            supports_audio,
            &session_id,
            &session,
        )?;
        let mut managed_paths = local_image_paths.clone();
        managed_paths.extend(local_audio_paths.clone());
        let mut attachment_guard = ManagedAttachmentGuard::new(managed_paths);
        if prompt.trim().is_empty() {
            return Err(Error::invalid_params().data("prompt must include text content"));
        }
        let cancellation = CancellationToken::new();
        {
            let mut sessions = self.sessions.lock().await;
            let Some(current) = sessions.get_mut(&session_id) else {
                return Err(Error::invalid_params().data("unknown session"));
            };
            if current.cancellation.is_some() {
                return Err(Error::invalid_params().data("session already has an active prompt"));
            }
            current.cancellation = Some(cancellation.clone());
        }

        let mut messages = {
            let mut manager = self.session_manager.lock().map_err(|_| internal_error())?;
            let stored = manager
                .get_session(&session_id)
                .ok_or_else(|| Error::invalid_params().data("unknown session"))?;
            stored.messages.clone()
        };
        let mut user_message = crate::session::types::Message::user(&prompt);
        user_message.local_image_paths = local_image_paths;
        user_message.local_audio_paths = local_audio_paths;
        user_message.provider = Some(session.provider.clone());
        user_message.model = Some(session.model.clone());
        user_message.agent_mode = Some(session.agent.clone());
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let process_registry = std::sync::Arc::new(crate::tools::ProcessRegistry::new());
        let prompt_registry = crate::tools::initialize_tool_registry_with_dynamic_config(
            Some(sender.clone()),
            tool_permissions(&session),
            session.config.merged_config.agent_registry.clone(),
            cancellation.clone(),
            Some(&session.provider),
            &session.config.merged_config.websearch,
            &session.config.merged_config.mcp,
            &session.cwd,
            process_registry.clone(),
        )
        .await;
        let is_git_repo =
            crate::utils::git::is_git_repo(&session.cwd.to_string_lossy()).unwrap_or(false);
        let system_prompt = crate::prompt::SystemPromptComposer::new(
            &session.model,
            session.cwd.to_string_lossy(),
            is_git_repo,
            std::env::consts::OS,
        )
        .with_tool_registry(prompt_registry.clone())
        .with_agent_registry(session.config.merged_config.agent_registry.clone())
        .with_active_agent(session.agent.clone())
        .compose()
        .await;
        let system_message = crate::session::types::Message::system(system_prompt);
        let prompt_tokens = crate::session::compaction::message_context_tokens(&system_message);
        let auto_compacted = self
            .maybe_auto_compact_session(
                &session_id,
                &session,
                &connection,
                cancellation.clone(),
                prompt_tokens,
                crate::session::compaction::message_context_tokens(&user_message),
            )
            .await?;
        if cancellation.is_cancelled() {
            if let Some(current) = self.sessions.lock().await.get_mut(&session_id) {
                current.cancellation = None;
            }
            return Ok(PromptResponse::new(StopReason::Cancelled));
        }
        if auto_compacted {
            messages = self
                .session_manager
                .lock()
                .map_err(|_| internal_error())?
                .get_session_ref(&session_id)
                .map(|stored| stored.messages.clone())
                .ok_or_else(|| Error::invalid_params().data("unknown session"))?;
        }
        {
            let mut manager = self.session_manager.lock().map_err(|_| internal_error())?;
            manager
                .add_message_to_session(&session_id, &user_message)
                .map_err(|_| internal_error())?;
            attachment_guard.commit();
            manager
                .set_session_status(
                    &session_id,
                    crate::session::types::SessionStatus::Streaming,
                    None,
                )
                .map_err(|_| internal_error())?;
        }
        messages.push(user_message);
        let base_context_tokens = acp_context_tokens(&messages, prompt_tokens);
        let base_cost = acp_session_cost(&messages);
        messages = acp_request_messages(&messages, system_message);
        send_usage(
            &connection,
            &session_id,
            &session,
            base_context_tokens,
            (base_cost > 0.0).then_some(base_cost),
            None,
        )?;

        let stream_session_id = session_id.clone();
        let stream_cancellation = cancellation.clone();
        let stream_sender = sender.clone();
        let stream_session = session.clone();
        let stream_tool_registry = prompt_registry;
        let stream_process_registry = process_registry;
        tokio::spawn(async move {
            let result = crate::llm::client::stream_llm_with_cancellation(
                stream_cancellation,
                stream_session_id,
                stream_session.provider.clone(),
                stream_session.model.clone(),
                stream_session.reasoning,
                stream_session.agent.clone(),
                stream_session
                    .config
                    .merged_config
                    .agent_registry
                    .get(&stream_session.agent)
                    .and_then(|agent| agent.max_steps),
                stream_session.config.merged_config.agent_registry.clone(),
                tool_permissions(&stream_session),
                stream_session.config.merged_config.websearch.clone(),
                stream_session.config.merged_config.mcp.clone(),
                stream_session.config.merged_config.compaction.clone(),
                stream_session.cwd.to_string_lossy().to_string(),
                Some(stream_tool_registry),
                messages,
                sender,
                stream_process_registry,
            )
            .await;
            if let Err(error) = result {
                let _ = stream_sender.send(crate::llm::ChunkMessage::Failed(error.to_string()));
            }
            let _ = stream_sender.send(crate::llm::ChunkMessage::End);
        });

        let mut assistant = crate::session::types::Message::incomplete("");
        let message_id = assistant.id.clone();
        assistant.provider = Some(session.provider.clone());
        assistant.model = Some(session.model.clone());
        assistant.agent_mode = Some(session.agent.clone());
        let mut failed = None;
        let mut cancelled = false;
        let mut turn_stop_reason = None;
        let mut live_usage = AcpLiveUsage::default();

        while let Some(chunk) = receiver.recv().await {
            match chunk {
                crate::llm::ChunkMessage::Text(text) => {
                    assistant.append(&text);
                    send_text(&connection, &session_id, &message_id, text, false)?;
                    send_live_usage(
                        &connection,
                        &session_id,
                        &session,
                        base_context_tokens,
                        base_cost,
                        &assistant,
                        &live_usage,
                    )?;
                }
                crate::llm::ChunkMessage::Reasoning(text) => {
                    assistant.append_reasoning(&text);
                    send_text(&connection, &session_id, &message_id, text, true)?;
                    send_live_usage(
                        &connection,
                        &session_id,
                        &session,
                        base_context_tokens,
                        base_cost,
                        &assistant,
                        &live_usage,
                    )?;
                }
                crate::llm::ChunkMessage::ToolCalls(tool_calls) => {
                    for tool_call in &tool_calls {
                        record_tool_call(&mut assistant, tool_call);
                    }
                    for tool_call in tool_calls {
                        send_tool_call(&connection, &session_id, tool_call, &session.cwd)?;
                    }
                    send_live_usage(
                        &connection,
                        &session_id,
                        &session,
                        base_context_tokens,
                        base_cost,
                        &assistant,
                        &live_usage,
                    )?;
                }
                crate::llm::ChunkMessage::ToolResult(result) => {
                    record_tool_result(&mut assistant, &result);
                    send_tool_result(&connection, &session_id, result, &session.cwd)?;
                    send_live_usage(
                        &connection,
                        &session_id,
                        &session,
                        base_context_tokens,
                        base_cost,
                        &assistant,
                        &live_usage,
                    )?;
                }
                crate::llm::ChunkMessage::Metrics {
                    token_count,
                    duration_ms,
                    usage,
                    cost,
                } => {
                    assistant.token_count = Some(token_count);
                    assistant.duration_ms = Some(duration_ms);
                    if let Some(usage) = usage {
                        assistant.apply_usage(usage, cost);
                        live_usage.cumulative = usage;
                    }
                    live_usage.cost = cost.unwrap_or(live_usage.cost);
                    send_usage(
                        &connection,
                        &session_id,
                        &session,
                        live_usage
                            .context_tokens(base_context_tokens, &assistant)
                            .max(base_context_tokens.saturating_add(token_count)),
                        cumulative_cost(base_cost, live_usage.cost),
                        usage,
                    )?;
                }
                crate::llm::ChunkMessage::Usage(usage) => {
                    live_usage.observe_provider_usage(&assistant, usage);
                    live_usage.cost += estimate_session_usage_cost(&session, &usage);
                    assistant
                        .parts
                        .push(crate::session::types::MessagePart::usage(
                            usage.input,
                            usage.output,
                            usage.cache_read,
                            usage.cache_write,
                            estimate_session_usage_cost(&session, &usage),
                        ));
                    if let Some(part) = assistant.parts.last_mut() {
                        part.data["acp_prompt_tokens"] = serde_json::json!(prompt_tokens);
                    }
                    if usage.output > 0 {
                        assistant.output_tokens = Some(
                            assistant
                                .output_tokens
                                .unwrap_or(0)
                                .saturating_add(usage.output as usize),
                        );
                    }
                    send_live_usage(
                        &connection,
                        &session_id,
                        &session,
                        base_context_tokens,
                        base_cost,
                        &assistant,
                        &live_usage,
                    )?;
                }
                crate::llm::ChunkMessage::Cancelled => cancelled = true,
                crate::llm::ChunkMessage::Failed(error) => failed = Some(error),
                crate::llm::ChunkMessage::TurnStopReason(reason) => turn_stop_reason = Some(reason),
                crate::llm::ChunkMessage::PermissionRequest(prompt) => {
                    let response = request_permission(&connection, &session_id, &prompt).await;
                    let _ = prompt.response_tx.send(response);
                }
                crate::llm::ChunkMessage::QuestionRequest {
                    tool_call_id,
                    questions,
                    response_tx,
                } => {
                    let response = if self.supports_form_elicitation() {
                        request_questions(
                            &connection,
                            &session_id,
                            tool_call_id.as_deref(),
                            &questions,
                            &cancellation,
                        )
                        .await
                    } else {
                        skipped_question_answers(&questions)
                    };
                    let _ = response_tx.send(response);
                }
                crate::llm::ChunkMessage::TerminalSessionRequest(request) => {
                    if self.supports_terminals() {
                        bridge_terminal_session(
                            &connection,
                            &session_id,
                            &session.cwd,
                            request,
                            &cancellation,
                        )
                        .await;
                    } else {
                        let _ = request
                            .control_tx
                            .send(crate::tools::TerminalSessionControl::Stop);
                    }
                }
                crate::llm::ChunkMessage::End => break,
                _ => {}
            }
        }

        assistant.is_complete = true;
        assistant.was_interrupted = cancelled || cancellation.is_cancelled();
        {
            let mut manager = self.session_manager.lock().map_err(|_| internal_error())?;
            manager
                .add_message_to_session(&session_id, &assistant)
                .map_err(|_| internal_error())?;
            let status = if assistant.was_interrupted {
                crate::session::types::SessionStatus::Interrupted
            } else if failed.is_some() {
                crate::session::types::SessionStatus::Failed
            } else {
                crate::session::types::SessionStatus::Idle
            };
            manager
                .set_session_status(&session_id, status, failed.as_deref())
                .map_err(|_| internal_error())?;
        }
        if assistant.was_interrupted {
            if let Some(current) = self.sessions.lock().await.get_mut(&session_id) {
                current.cancellation = None;
            }
            return Ok(PromptResponse::new(StopReason::Cancelled));
        }
        if let Some(error) = failed {
            if let Some(current) = self.sessions.lock().await.get_mut(&session_id) {
                current.cancellation = None;
            }
            return Err(internal_error_with(&error));
        }
        let _ = self
            .maybe_auto_compact_session(
                &session_id,
                &session,
                &connection,
                cancellation.clone(),
                prompt_tokens,
                0,
            )
            .await?;
        if let Some(current) = self.sessions.lock().await.get_mut(&session_id) {
            current.cancellation = None;
        }
        Ok(PromptResponse::new(acp_stop_reason(turn_stop_reason)))
    }
}
