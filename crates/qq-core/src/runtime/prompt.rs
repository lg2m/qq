use std::path::Path;

use qq_protocol::{ContentHash, PromptVersion};
use qq_provider::ToolSpec;

use crate::{
    hosts::{EMBEDDED_TOOL_PREFIX, MCP_TOOL_PREFIX},
    tools::SPAWN_AGENT_TOOL,
    workspace::{SelectedGuidance, WorkspaceInstructions},
};

pub(crate) const AGENT_PROMPT_VERSION: PromptVersion = match PromptVersion::new(9) {
    Some(version) => version,
    None => panic!("agent prompt version must be nonzero"),
};

/// Version 9 of the base agent prompt. The text is versioned in code, not
/// configuration: bump this note and review the diff whenever it changes.
///
/// `tool_index` is the progressive-exposure index of external tools not yet
/// callable; `skill_index` lists disclosed skills the model may load.
pub(crate) fn agent_system_prompt(
    workspace: &Path,
    specs: &[ToolSpec],
    tool_index: Option<&str>,
    skill_index: Option<&str>,
    workspace_instructions: &WorkspaceInstructions,
    persona: Option<&crate::plan::Persona>,
    selected_guidance: Option<&SelectedGuidance>,
) -> String {
    let mut tool_names = String::new();
    let mut has_external = tool_index.is_some();
    let mut has_spawn = false;
    for spec in specs {
        if !tool_names.is_empty() {
            tool_names.push_str(", ");
        }
        tool_names.push_str(spec.name());
        has_external |= spec.name().starts_with(MCP_TOOL_PREFIX)
            || spec.name().starts_with(EMBEDDED_TOOL_PREFIX);
        has_spawn |= spec.name() == SPAWN_AGENT_TOOL;
    }
    let mcp_note = if has_external {
        " Tools named mcp__<server>__<tool> or ext__<host>__<tool> call external tool hosts, \
         execute outside the workspace, and may require user approval."
    } else {
        ""
    };
    let spawn_section = if has_spawn {
        "\n\nDelegation:\n\
         - spawn_agent runs a one-shot read-only sub-agent in this workspace from a \
         self-contained task brief and returns only its final answer.\n\
         - Omit spawn_agent's model argument by default. QQ then uses the configured worker \
         model or this session's persisted selected model, including its authenticated provider. \
         Set model only when the user explicitly requests an exact provider/model route; never \
         guess, translate, or invent one.\n\
         - Delegate when all three hold: the raw evidence would dwarf the distilled answer, \
         you will not need that evidence verbatim later, and the task needs no mid-flight \
         steering.\n\
         - Default to working inline: single reads, searches, and quick lookups are never \
         worth a sub-agent.\n\
         - Exception: several independent questions are worth delegating even when each is \
         small, because sub-agents run concurrently."
    } else {
        ""
    };
    let mut prompt = format!(
        "You are QQ, a coding agent operating in the workspace rooted at {root}.\n\
         \n\
         Available tools: {tool_names}. read_file, list_dir, search, and search_history are read-only; \
         edit_file and write_file modify workspace files and may require user approval; \
         shell runs one command in the workspace with a bounded timeout and may require user approval.{mcp_note}\n\
         \n\
         Working conventions:\n\
         - Determine observable completion criteria from the user's request before acting.\n\
         - Read a file with read_file before editing or overwriting it; edits without a prior read in this session are rejected.\n\
         - Inspect existing state before changing it and preserve unrelated work.\n\
         - Prefer search over guessing file paths.\n\
         - Give every tool path relative to the workspace root; absolute paths are rejected.\n\
         - Before changing files below a subdirectory, inspect each directory from the workspace root to the target for AGENTS.md; when AGENTS.md is absent at one scope, check CLAUDE.md. Apply selected instructions root-to-leaf, with more-specific instructions taking precedence.\n\
         - Implement requested changes rather than stopping at analysis unless the user requested analysis-only work.\n\
         - Treat failed tools and tests as evidence: diagnose them and continue when a safe path remains.\n\
         - Run the narrowest relevant verification before broader checks.\n\
         - Do not claim success without evidence from the resulting state.\n\
         - Report remaining failures and uncertainty honestly.\n\
         - Respect explicit time, token, cost, and safety budgets.\n\
         - Prefer edit_file and write_file over shell for changing files.{spawn_section}",
        root = workspace.display(),
    );
    if let Some(index) = tool_index {
        prompt.push_str("\n\n");
        prompt.push_str(index.trim_end());
    }
    if let Some(index) = skill_index {
        prompt.push_str("\n\n");
        prompt.push_str(index.trim_end());
    }
    workspace_instructions.append_to_prompt(&mut prompt);
    if let Some(persona) = persona {
        persona.append_to_prompt(&mut prompt);
    }
    if let Some(guidance) = selected_guidance {
        guidance.append_to_prompt(&mut prompt);
    }
    prompt
}

pub(crate) struct ToolSchemaMeasurement {
    pub(crate) hash: ContentHash,
    pub(crate) bytes: u64,
}

pub(crate) fn tool_schema_measurement(specs: &[ToolSpec]) -> ToolSchemaMeasurement {
    use sha2::{Digest, Sha256};

    let mut digest = Sha256::new();
    let mut measured_bytes = 0_u64;
    for spec in specs {
        for bytes in [spec.name().as_bytes(), spec.description().as_bytes()] {
            digest.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
            digest.update(bytes);
            measured_bytes =
                measured_bytes.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        }
        let schema = spec.input_schema().to_string();
        digest.update(
            u64::try_from(schema.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        digest.update(schema.as_bytes());
        measured_bytes = measured_bytes
            .saturating_add(u64::try_from(schema.len()).unwrap_or(u64::MAX))
            .saturating_add(32);
    }
    ToolSchemaMeasurement {
        hash: ContentHash::from_bytes(digest.finalize().into()),
        bytes: measured_bytes,
    }
}
