use crate::AskRequest;
use thiserror::Error;

pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub const MAX_EVENT_BYTES: usize = 1024 * 1024;

const MAX_PROMPT_BYTES: usize = 512 * 1024;
pub const MAX_WORKSPACE_BYTES: usize = 4096;
pub const MAX_MODEL_BYTES: usize = 512;
pub const MAX_ORGANIZATION_BYTES: usize = 512;
const SESSION_ID_HEX_BYTES: usize = 32;

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum AskValidationError {
    #[error("prompt must not be empty")]
    EmptyPrompt,
    #[error("prompt is too large")]
    PromptTooLarge,
    #[error("session ID is invalid")]
    InvalidSessionId,
    #[error("workspace path must be valid UTF-8")]
    WorkspaceNotUtf8,
    #[error("workspace path must not be empty")]
    EmptyWorkspace,
    #[error("workspace path is too large")]
    WorkspaceTooLarge,
    #[error("model is too large")]
    ModelTooLarge,
    #[error("organization is too large")]
    OrganizationTooLarge,
}

/// Validates the shared semantic limits for the legacy one-shot request.
pub fn validate_ask_request(request: &AskRequest) -> Result<(), AskValidationError> {
    if request.prompt.trim().is_empty() {
        return Err(AskValidationError::EmptyPrompt);
    }
    if request.prompt.len() > MAX_PROMPT_BYTES {
        return Err(AskValidationError::PromptTooLarge);
    }
    if request.session_id.as_ref().is_some_and(|session_id| {
        session_id.len() != SESSION_ID_HEX_BYTES
            || !session_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }) {
        return Err(AskValidationError::InvalidSessionId);
    }
    let workspace = request
        .workspace
        .to_str()
        .ok_or(AskValidationError::WorkspaceNotUtf8)?;
    if workspace.is_empty() {
        return Err(AskValidationError::EmptyWorkspace);
    }
    if workspace.len() > MAX_WORKSPACE_BYTES {
        return Err(AskValidationError::WorkspaceTooLarge);
    }
    if request
        .model
        .as_ref()
        .is_some_and(|model| model.len() > MAX_MODEL_BYTES)
    {
        return Err(AskValidationError::ModelTooLarge);
    }
    if request
        .organization
        .as_ref()
        .is_some_and(|organization| organization.len() > MAX_ORGANIZATION_BYTES)
    {
        return Err(AskValidationError::OrganizationTooLarge);
    }
    Ok(())
}
