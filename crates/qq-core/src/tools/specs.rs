use std::sync::OnceLock;

use qq_provider::ToolSpec;
use serde::Deserialize;
use serde_json::json;

use super::{list::MAX_DIRECTORY_ENTRIES, read::MAX_READ_LINES, shell::MAX_SHELL_TIMEOUT_SECS};

/// The sub-agent tool. Not a [`BuiltInTool`]: it is declared only for runs
/// that may spawn (never for child sessions), and it dispatches to the
/// session layer rather than to a workspace execution.
pub(crate) const SPAWN_AGENT_TOOL: &str = "spawn_agent";

#[derive(Clone, Copy)]
pub(super) enum BuiltInTool {
    ReadFile,
    ListDir,
    Search,
    EditFile,
    WriteFile,
    Shell,
    #[cfg(test)]
    TestDelay,
    #[cfg(test)]
    TestMutate,
    #[cfg(test)]
    TestShell,
}

impl BuiltInTool {
    const ALL: [Self; 6] = [
        Self::ReadFile,
        Self::ListDir,
        Self::Search,
        Self::EditFile,
        Self::WriteFile,
        Self::Shell,
    ];

    pub(super) fn from_name(name: &str) -> Option<Self> {
        match name {
            "read_file" => Some(Self::ReadFile),
            "list_dir" => Some(Self::ListDir),
            "search" => Some(Self::Search),
            "edit_file" => Some(Self::EditFile),
            "write_file" => Some(Self::WriteFile),
            "shell" => Some(Self::Shell),
            #[cfg(test)]
            "__test_delay" => Some(Self::TestDelay),
            #[cfg(test)]
            "__test_mutate" => Some(Self::TestMutate),
            #[cfg(test)]
            "__test_shell" => Some(Self::TestShell),
            _ => None,
        }
    }

    fn spec(self) -> ToolSpec {
        match self {
            Self::ReadFile => ToolSpec::new(
                "read_file",
                "Read a UTF-8 file in the workspace by line, with a 1-based offset and bounded line count.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "offset": { "type": "integer", "minimum": 1 },
                        "limit": { "type": "integer", "minimum": 1, "maximum": MAX_READ_LINES }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            ),
            Self::ListDir => ToolSpec::new(
                "list_dir",
                "List one workspace directory in deterministic name order.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 1, "maximum": MAX_DIRECTORY_ENTRIES }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            ),
            Self::Search => ToolSpec::new(
                "search",
                "Search workspace file names and UTF-8 file contents for a literal string.",
                json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "minLength": 1 },
                        "path": { "type": "string" }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }),
            ),
            Self::EditFile => ToolSpec::new(
                "edit_file",
                "Replace an exact string in a workspace file that was read earlier in this session. Fails if old_string is missing or ambiguous; set replace_all to replace every occurrence.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "old_string": { "type": "string", "minLength": 1 },
                        "new_string": { "type": "string" },
                        "replace_all": { "type": "boolean" }
                    },
                    "required": ["path", "old_string", "new_string"],
                    "additionalProperties": false
                }),
            ),
            Self::WriteFile => ToolSpec::new(
                "write_file",
                "Create a workspace file, or fully overwrite one that was read earlier in this session.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"],
                    "additionalProperties": false
                }),
            ),
            Self::Shell => ToolSpec::new(
                "shell",
                "Run one shell command in the workspace via `sh -c`, capturing combined stdout and stderr. The command runs with a timeout (120 s by default) and its whole process group is killed when the timeout expires or the run is cancelled.",
                json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "minLength": 1 },
                        "cwd": {
                            "type": "string",
                            "description": "Working directory relative to the workspace root; defaults to the root."
                        },
                        "timeout_seconds": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": MAX_SHELL_TIMEOUT_SECS
                        }
                    },
                    "required": ["command"],
                    "additionalProperties": false
                }),
            ),
            #[cfg(test)]
            Self::TestDelay | Self::TestMutate | Self::TestShell => {
                unreachable!("test tools are not advertised")
            }
        }
    }
}

/// The declaration for [`SPAWN_AGENT_TOOL`]. Kept out of [`specs`] because it
/// joins the tool list only when the run may spawn: child sessions and
/// session-less runs never see it.
pub(crate) fn spawn_agent_spec(model_routes: &[String]) -> ToolSpec {
    let mut properties = serde_json::Map::from_iter([(
        "task".to_owned(),
        json!({
            "type": "string",
            "minLength": 1,
            "description": "A complete, self-contained brief for the sub-agent."
        }),
    )]);
    if !model_routes.is_empty() {
        properties.insert(
            "model".to_owned(),
            json!({
                "type": "string",
                "enum": model_routes,
                "description": "Exact authenticated provider/model override. Omit by default to use QQ's configured worker model or this session's selected model. Set only when the user explicitly requests one of these exact routes; never guess or translate providers."
            }),
        );
    }
    ToolSpec::new(
        SPAWN_AGENT_TOOL,
        "Delegate one self-contained task to a read-only sub-agent in this workspace and receive \
         only its final answer. Worth it when the raw evidence would dwarf the distilled answer \
         and you will not need that evidence verbatim later; several independent questions can be \
         delegated in parallel. Single reads, searches, and quick lookups are cheaper inline. The \
         task brief must carry everything the sub-agent needs: it starts with no other context. \
         Omit model by default so QQ uses its configured worker model or the current session's \
         selected model. Set model only when the user explicitly requests an exact provider/model \
         route listed by this tool; never guess, translate, or invent a route.",
        serde_json::Value::Object(serde_json::Map::from_iter([
            ("type".to_owned(), json!("object")),
            (
                "properties".to_owned(),
                serde_json::Value::Object(properties),
            ),
            ("required".to_owned(), json!(["task"])),
            ("additionalProperties".to_owned(), json!(false)),
        ])),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SpawnAgentArgs {
    pub(crate) task: String,
    #[serde(default)]
    pub(crate) model: Option<String>,
}

pub(crate) fn specs() -> Vec<ToolSpec> {
    static SPECS: OnceLock<Vec<ToolSpec>> = OnceLock::new();
    SPECS
        .get_or_init(|| {
            BuiltInTool::ALL
                .into_iter()
                .map(BuiltInTool::spec)
                .collect()
        })
        .clone()
}
