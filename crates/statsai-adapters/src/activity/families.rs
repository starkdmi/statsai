//! Family alias tables. Unknown names stay `other`. MCP rows always use family `mcp`.

use statsai_core::ActivityFamily;

#[derive(Clone, Copy)]
pub(crate) struct FamilyAlias {
    pub name: &'static str,
    pub family: ActivityFamily,
}

pub(crate) const CODEX_LEGACY_FAMILY_ALIASES: &[FamilyAlias] = &[
    FamilyAlias {
        name: "exec",
        family: ActivityFamily::Shell,
    },
    FamilyAlias {
        name: "run",
        family: ActivityFamily::Shell,
    },
    FamilyAlias {
        name: "exec_command",
        family: ActivityFamily::Shell,
    },
    FamilyAlias {
        name: "shell",
        family: ActivityFamily::Shell,
    },
    FamilyAlias {
        name: "shell_command",
        family: ActivityFamily::Shell,
    },
    FamilyAlias {
        name: "write_stdin",
        family: ActivityFamily::Shell,
    },
    FamilyAlias {
        name: "apply_patch",
        family: ActivityFamily::FileWrite,
    },
    FamilyAlias {
        name: "view_image",
        family: ActivityFamily::Media,
    },
    FamilyAlias {
        name: "update_plan",
        family: ActivityFamily::Planning,
    },
    FamilyAlias {
        name: "spawn_agent",
        family: ActivityFamily::Agent,
    },
    FamilyAlias {
        name: "wait_agent",
        family: ActivityFamily::Agent,
    },
    FamilyAlias {
        name: "send_message",
        family: ActivityFamily::Agent,
    },
    FamilyAlias {
        name: "close_agent",
        family: ActivityFamily::Agent,
    },
    FamilyAlias {
        name: "list_agents",
        family: ActivityFamily::Agent,
    },
    FamilyAlias {
        name: "web_search_call",
        family: ActivityFamily::Web,
    },
];

pub(crate) const CLAUDE_FAMILY_ALIASES: &[FamilyAlias] = &[
    FamilyAlias {
        name: "Read",
        family: ActivityFamily::FileRead,
    },
    FamilyAlias {
        name: "NotebookRead",
        family: ActivityFamily::FileRead,
    },
    FamilyAlias {
        name: "Edit",
        family: ActivityFamily::FileWrite,
    },
    FamilyAlias {
        name: "Write",
        family: ActivityFamily::FileWrite,
    },
    FamilyAlias {
        name: "NotebookEdit",
        family: ActivityFamily::FileWrite,
    },
    FamilyAlias {
        name: "MultiEdit",
        family: ActivityFamily::FileWrite,
    },
    FamilyAlias {
        name: "Grep",
        family: ActivityFamily::CodeSearch,
    },
    FamilyAlias {
        name: "Glob",
        family: ActivityFamily::CodeSearch,
    },
    FamilyAlias {
        name: "LS",
        family: ActivityFamily::CodeSearch,
    },
    FamilyAlias {
        name: "ToolSearch",
        family: ActivityFamily::CodeSearch,
    },
    FamilyAlias {
        name: "Bash",
        family: ActivityFamily::Shell,
    },
    FamilyAlias {
        name: "WebFetch",
        family: ActivityFamily::Web,
    },
    FamilyAlias {
        name: "WebSearch",
        family: ActivityFamily::Web,
    },
    FamilyAlias {
        name: "Agent",
        family: ActivityFamily::Agent,
    },
    FamilyAlias {
        name: "Task",
        family: ActivityFamily::Agent,
    },
    FamilyAlias {
        name: "TodoWrite",
        family: ActivityFamily::Planning,
    },
    FamilyAlias {
        name: "TodoRead",
        family: ActivityFamily::Planning,
    },
    FamilyAlias {
        name: "TaskCreate",
        family: ActivityFamily::Planning,
    },
    FamilyAlias {
        name: "TaskUpdate",
        family: ActivityFamily::Planning,
    },
    FamilyAlias {
        name: "TaskList",
        family: ActivityFamily::Planning,
    },
    FamilyAlias {
        name: "TaskGet",
        family: ActivityFamily::Planning,
    },
    FamilyAlias {
        name: "TaskOutput",
        family: ActivityFamily::Planning,
    },
    FamilyAlias {
        name: "TaskStop",
        family: ActivityFamily::Planning,
    },
    FamilyAlias {
        name: "EnterPlanMode",
        family: ActivityFamily::Planning,
    },
    FamilyAlias {
        name: "ExitPlanMode",
        family: ActivityFamily::Planning,
    },
    FamilyAlias {
        name: "AskUserQuestion",
        family: ActivityFamily::Other,
    },
    FamilyAlias {
        name: "CronCreate",
        family: ActivityFamily::Other,
    },
    FamilyAlias {
        name: "CronDelete",
        family: ActivityFamily::Other,
    },
    FamilyAlias {
        name: "CronList",
        family: ActivityFamily::Other,
    },
];

pub(crate) const OPENCODE_FAMILY_ALIASES: &[FamilyAlias] = &[
    FamilyAlias {
        name: "read",
        family: ActivityFamily::FileRead,
    },
    FamilyAlias {
        name: "edit",
        family: ActivityFamily::FileWrite,
    },
    FamilyAlias {
        name: "write",
        family: ActivityFamily::FileWrite,
    },
    FamilyAlias {
        name: "apply_patch",
        family: ActivityFamily::FileWrite,
    },
    FamilyAlias {
        name: "multiedit",
        family: ActivityFamily::FileWrite,
    },
    FamilyAlias {
        name: "grep",
        family: ActivityFamily::CodeSearch,
    },
    FamilyAlias {
        name: "glob",
        family: ActivityFamily::CodeSearch,
    },
    FamilyAlias {
        name: "list",
        family: ActivityFamily::CodeSearch,
    },
    FamilyAlias {
        name: "lsp_diagnostics",
        family: ActivityFamily::CodeSearch,
    },
    FamilyAlias {
        name: "bash",
        family: ActivityFamily::Shell,
    },
    FamilyAlias {
        name: "webfetch",
        family: ActivityFamily::Web,
    },
    FamilyAlias {
        name: "websearch",
        family: ActivityFamily::Web,
    },
    FamilyAlias {
        name: "google_search",
        family: ActivityFamily::Web,
    },
    FamilyAlias {
        name: "task",
        family: ActivityFamily::Agent,
    },
    FamilyAlias {
        name: "call_omo_agent",
        family: ActivityFamily::Agent,
    },
    FamilyAlias {
        name: "background_output",
        family: ActivityFamily::Agent,
    },
    FamilyAlias {
        name: "todowrite",
        family: ActivityFamily::Planning,
    },
    FamilyAlias {
        name: "todoread",
        family: ActivityFamily::Planning,
    },
    FamilyAlias {
        name: "question",
        family: ActivityFamily::Planning,
    },
    FamilyAlias {
        name: "skill",
        family: ActivityFamily::Other,
    },
];

pub(crate) const GROK_FAMILY_ALIASES: &[FamilyAlias] = &[
    FamilyAlias {
        name: "read_file",
        family: ActivityFamily::FileRead,
    },
    FamilyAlias {
        name: "list_dir",
        family: ActivityFamily::FileRead,
    },
    FamilyAlias {
        name: "search_replace",
        family: ActivityFamily::FileWrite,
    },
    FamilyAlias {
        name: "write",
        family: ActivityFamily::FileWrite,
    },
    FamilyAlias {
        name: "grep",
        family: ActivityFamily::CodeSearch,
    },
    FamilyAlias {
        name: "run_terminal_command",
        family: ActivityFamily::Shell,
    },
    FamilyAlias {
        name: "get_command_or_subagent_output",
        family: ActivityFamily::Shell,
    },
    FamilyAlias {
        name: "kill_command_or_subagent",
        family: ActivityFamily::Shell,
    },
    FamilyAlias {
        name: "web_fetch",
        family: ActivityFamily::Web,
    },
    FamilyAlias {
        name: "image_gen",
        family: ActivityFamily::Media,
    },
    FamilyAlias {
        name: "image_edit",
        family: ActivityFamily::Media,
    },
    FamilyAlias {
        name: "image_to_video",
        family: ActivityFamily::Media,
    },
    FamilyAlias {
        name: "reference_to_video",
        family: ActivityFamily::Media,
    },
    FamilyAlias {
        name: "spawn_subagent",
        family: ActivityFamily::Agent,
    },
    FamilyAlias {
        name: "todo_write",
        family: ActivityFamily::Planning,
    },
];

#[must_use]
pub(crate) fn family_for_name(aliases: &[FamilyAlias], name: &str) -> ActivityFamily {
    aliases
        .iter()
        .find(|alias| alias.name == name)
        .map(|alias| alias.family)
        .unwrap_or(ActivityFamily::Other)
}
