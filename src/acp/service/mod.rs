mod attachment_storage;
mod attachments;
mod command_expansion;
mod commands;
mod compaction;
mod compaction_summary;
mod config;
mod context;
mod mcp_config;
mod models;
mod modes;
mod notifications;
mod permission_preview;
mod permissions;
mod prompt;
mod questions;
mod replay;
mod sessions;
mod stream;
mod terminals;
mod tool_content;
mod tool_locations;
mod transcript;
mod usage;

#[cfg(test)]
mod test_support;

use crate::config::configuration::LoadedConfig;
use crate::session::manager::SessionManager;
use agent_client_protocol::Error;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct AcpService {
    sessions: Arc<AsyncMutex<HashMap<String, AcpSession>>>,
    session_manager: Arc<Mutex<SessionManager>>,
    client_capabilities: Arc<Mutex<agent_client_protocol::schema::v1::ClientCapabilities>>,
}

#[derive(Clone)]
struct AcpSession {
    cwd: PathBuf,
    config: LoadedConfig,
    tool_permissions: crate::tools::ToolPermissions,
    skills: crate::skill::SkillStore,
    models: Vec<crate::model::types::Model>,
    provider: String,
    model: String,
    agent: String,
    reasoning_selection: crate::model::reasoning::ReasoningEffort,
    reasoning: Option<crate::model::reasoning::ReasoningEffort>,
    context_window: Option<u32>,
    cancellation: Option<CancellationToken>,
}

fn internal_error() -> Error {
    Error::internal_error().data("Crabcode ACP operation failed")
}

fn internal_error_with(error: &str) -> Error {
    Error::internal_error().data(format!("Crabcode ACP operation failed: {error}"))
}

impl AcpService {
    pub fn new(initial_workspace: &Path) -> Result<Self, Error> {
        let session_manager = SessionManager::new()
            .with_history_for_workspace(initial_workspace)
            .map_err(|_| internal_error())?;
        Ok(Self {
            sessions: Arc::new(AsyncMutex::new(HashMap::new())),
            session_manager: Arc::new(Mutex::new(session_manager)),
            client_capabilities: Arc::new(Mutex::new(Default::default())),
        })
    }

    pub fn set_client_capabilities(
        &self,
        capabilities: agent_client_protocol::schema::v1::ClientCapabilities,
    ) {
        if let Ok(mut current) = self.client_capabilities.lock() {
            *current = capabilities;
        }
    }

    fn supports_form_elicitation(&self) -> bool {
        self.client_capabilities
            .lock()
            .ok()
            .and_then(|capabilities| capabilities.elicitation.clone())
            .and_then(|elicitation| elicitation.form)
            .is_some()
    }

    fn supports_terminals(&self) -> bool {
        self.client_capabilities
            .lock()
            .ok()
            .is_some_and(|capabilities| capabilities.terminal)
    }
}
