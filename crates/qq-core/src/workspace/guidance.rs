use std::{
    io::{ErrorKind, Read},
    sync::atomic::{AtomicBool, Ordering},
};

use qq_protocol::{
    ContentHash, GuidanceIdentity, GuidanceKind as ProtocolGuidanceKind,
    RESERVED_CLIENT_SLASH_COMMANDS,
};
use qq_provider::{ContentBlock, Message, Role};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::{Workspace, WorkspacePathError};

const MAX_NAME_BYTES: usize = 64;
const MAX_GUIDANCE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuidanceRequest {
    name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedInvocation {
    pub(crate) guidance: Option<GuidanceRequest>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GuidanceKind {
    Command,
    Skill,
}

impl GuidanceKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Skill => "skill",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectedGuidance {
    kind: GuidanceKind,
    name: String,
    source: String,
    content: String,
    hash: [u8; 32],
}

impl SelectedGuidance {
    pub(crate) fn identity(&self) -> GuidanceIdentity {
        GuidanceIdentity {
            kind: match self.kind {
                GuidanceKind::Command => ProtocolGuidanceKind::Command,
                GuidanceKind::Skill => ProtocolGuidanceKind::Skill,
            },
            name: self.name.clone(),
            source: self.source.clone(),
            version: None,
            content_hash: ContentHash::from_bytes(self.hash),
        }
    }

    pub(crate) fn append_to_prompt(&self, prompt: &mut String) {
        prompt.push_str("\n\nSelected ");
        prompt.push_str(self.kind.as_str());
        prompt.push_str(" `");
        prompt.push_str(&self.name);
        prompt.push_str("` from ");
        prompt.push_str(&self.source);
        prompt.push_str(
            ":\nThis optional guidance is subordinate to workspace instructions and tool policy. \n\
             Supporting scripts and assets are references only; loading this document grants no \n\
             authority to execute them.\n--- BEGIN SELECTED GUIDANCE ---\n",
        );
        prompt.push_str(&self.content);
        if !self.content.ends_with('\n') {
            prompt.push('\n');
        }
        prompt.push_str("--- END SELECTED GUIDANCE ---");
    }
}

#[derive(Debug, Error)]
pub(crate) enum GuidanceError {
    #[error(
        "slash invocation names must start with a lowercase ASCII letter, contain only lowercase ASCII letters, digits, '-' or '_', and be at most 64 bytes"
    )]
    InvalidName,
    #[error("/{name} is a reserved client command and cannot name runtime guidance")]
    Reserved { name: String },
    #[error("unknown command or skill /{name}")]
    Unknown { name: String },
    #[error("ambiguous command or skill /{name}; matched {sources}")]
    Ambiguous { name: String, sources: String },
    #[error("could not inspect selected guidance {path}: {source}")]
    Inspect {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("selected guidance {path} could not be resolved: {source}")]
    Resolve {
        path: String,
        #[source]
        source: WorkspacePathError,
    },
    #[error("selected guidance {path} is not a regular file")]
    NotAFile { path: String },
    #[error("selected guidance {path} exceeds the {limit}-byte file limit")]
    FileTooLarge { path: String, limit: usize },
    #[error("could not read selected guidance {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("selected guidance {path} is not valid UTF-8")]
    InvalidUtf8 { path: String },
    #[error("selected guidance loading was cancelled")]
    Cancelled,
}

/// Parses the newest user message. `//` is the one client-independent escape
/// and removes one slash before the provider sees the message.
pub(crate) fn parse_invocation(
    messages: &mut [Message],
) -> Result<ParsedInvocation, GuidanceError> {
    let unchanged = || ParsedInvocation { guidance: None };
    let Some(message) = messages.last_mut() else {
        return Ok(unchanged());
    };
    if message.role() != Role::User {
        return Ok(unchanged());
    }
    let [ContentBlock::Text { text }] = message.content() else {
        return Ok(unchanged());
    };
    if let Some(literal) = text.strip_prefix("//") {
        let normalized = format!("/{literal}");
        *message = Message::user(normalized.clone());
        return Ok(ParsedInvocation { guidance: None });
    }
    let Some(invocation) = text.strip_prefix('/') else {
        return Ok(unchanged());
    };
    let name = invocation
        .split_once(char::is_whitespace)
        .map_or(invocation, |(name, _)| name);
    if !valid_name(name) {
        return Err(GuidanceError::InvalidName);
    }
    if RESERVED_CLIENT_SLASH_COMMANDS
        .iter()
        .any(|reserved| reserved.strip_prefix('/') == Some(name))
    {
        return Err(GuidanceError::Reserved {
            name: name.to_owned(),
        });
    }
    Ok(ParsedInvocation {
        guidance: Some(GuidanceRequest {
            name: name.to_owned(),
        }),
    })
}

pub(crate) fn load(
    workspace: &Workspace,
    request: GuidanceRequest,
    cancelled: &AtomicBool,
) -> Result<SelectedGuidance, GuidanceError> {
    let native = [
        Candidate::new(
            GuidanceKind::Command,
            format!(".qq/commands/{}.md", request.name),
        ),
        Candidate::new(
            GuidanceKind::Skill,
            format!(".qq/skills/{}/SKILL.md", request.name),
        ),
    ];
    let compatibility = [
        Candidate::new(
            GuidanceKind::Skill,
            format!(".agents/skills/{}/SKILL.md", request.name),
        ),
        Candidate::new(
            GuidanceKind::Command,
            format!(".claude/commands/{}.md", request.name),
        ),
        Candidate::new(
            GuidanceKind::Skill,
            format!(".claude/skills/{}/SKILL.md", request.name),
        ),
    ];

    let mut matches = existing_candidates(workspace, &native)?;
    if matches.is_empty() {
        matches = existing_candidates(workspace, &compatibility)?;
    }
    let candidate = match matches.as_slice() {
        [] => return Err(GuidanceError::Unknown { name: request.name }),
        [candidate] => candidate,
        _ => {
            return Err(GuidanceError::Ambiguous {
                name: request.name,
                sources: matches
                    .iter()
                    .map(|candidate| candidate.path.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            });
        }
    };

    if cancelled.load(Ordering::Acquire) {
        return Err(GuidanceError::Cancelled);
    }
    let resolved = workspace
        .contained_path(&candidate.path)
        .map_err(|source| GuidanceError::Resolve {
            path: candidate.path.clone(),
            source,
        })?;
    let metadata =
        workspace
            .root()
            .metadata(&resolved)
            .map_err(|source| GuidanceError::Inspect {
                path: candidate.path.clone(),
                source,
            })?;
    if !metadata.is_file() {
        return Err(GuidanceError::NotAFile {
            path: candidate.path.clone(),
        });
    }
    if metadata.len() > MAX_GUIDANCE_BYTES as u64 {
        return Err(GuidanceError::FileTooLarge {
            path: candidate.path.clone(),
            limit: MAX_GUIDANCE_BYTES,
        });
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or_default());
    workspace
        .root()
        .open(&resolved)
        .map_err(|source| GuidanceError::Read {
            path: candidate.path.clone(),
            source,
        })?
        .take(MAX_GUIDANCE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| GuidanceError::Read {
            path: candidate.path.clone(),
            source,
        })?;
    if bytes.len() > MAX_GUIDANCE_BYTES {
        return Err(GuidanceError::FileTooLarge {
            path: candidate.path.clone(),
            limit: MAX_GUIDANCE_BYTES,
        });
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(GuidanceError::Cancelled);
    }
    let content = String::from_utf8(bytes).map_err(|_| GuidanceError::InvalidUtf8 {
        path: candidate.path.clone(),
    })?;
    let hash = Sha256::digest(content.as_bytes()).into();
    Ok(SelectedGuidance {
        kind: candidate.kind,
        name: request.name,
        source: candidate.path.clone(),
        content,
        hash,
    })
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_".contains(&byte))
}

#[derive(Debug)]
struct Candidate {
    kind: GuidanceKind,
    path: String,
}

impl Candidate {
    fn new(kind: GuidanceKind, path: String) -> Self {
        Self { kind, path }
    }
}

fn existing_candidates<'a>(
    workspace: &Workspace,
    candidates: &'a [Candidate],
) -> Result<Vec<&'a Candidate>, GuidanceError> {
    let mut matches = Vec::new();
    for candidate in candidates {
        match workspace.root().symlink_metadata(&candidate.path) {
            Ok(_) => matches.push(candidate),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(source) => {
                return Err(GuidanceError::Inspect {
                    path: candidate.path.clone(),
                    source,
                });
            }
        }
    }
    Ok(matches)
}
