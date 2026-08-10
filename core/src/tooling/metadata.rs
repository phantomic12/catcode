/// Risk class retained on the wire for backward-compatible approval behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolKind {
    ReadOnly,
    Destructive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolAccess {
    Read,
    Write,
    External,
    Interactive,
    Control,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParallelSafety {
    Safe,
    Sequential,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalPolicy {
    Inherit,
    Never,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancellationBehavior {
    Immediate,
    Cooperative,
    KillSubprocess,
}

/// Policy metadata is deliberately independent from JSON schema and execution.
/// Adding a built-in tool without an entry here fails an invariant test.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolMetadata {
    pub name: &'static str,
    pub kind: ToolKind,
    pub access: ToolAccess,
    pub parallel: ParallelSafety,
    pub approval: ApprovalPolicy,
    pub cancellation: CancellationBehavior,
    pub required_capabilities: &'static [&'static str],
    pub redacted_arguments: &'static [&'static str],
}

const READ: &[&str] = &["workspace.read"];
const WRITE: &[&str] = &["workspace.write"];
const READ_WRITE: &[&str] = &["workspace.read", "workspace.write"];
const PROCESS: &[&str] = &["subprocess.execute"];
const NETWORK: &[&str] = &["network.access"];
const CHILD: &[&str] = &["subagent.spawn"];
const INTERACTIVE: &[&str] = &["user.interaction"];
const NONE: &[&str] = &[];
const SECRET_ARGS: &[&str] = &["password", "api_key", "authorization", "token"];

const fn entry(
    name: &'static str,
    kind: ToolKind,
    access: ToolAccess,
    parallel: ParallelSafety,
    approval: ApprovalPolicy,
    cancellation: CancellationBehavior,
    required_capabilities: &'static [&'static str],
) -> ToolMetadata {
    ToolMetadata {
        name,
        kind,
        access,
        parallel,
        approval,
        cancellation,
        required_capabilities,
        redacted_arguments: SECRET_ARGS,
    }
}

/// Return metadata only for registered built-ins. Plugin tools carry their own
/// manifest-derived policy and are intentionally not guessed here.
pub fn metadata(name: &str) -> Option<ToolMetadata> {
    use ApprovalPolicy::{Inherit, Never};
    use CancellationBehavior::{Cooperative, Immediate, KillSubprocess};
    use ParallelSafety::{Safe, Sequential};
    use ToolAccess::{Control, External, Interactive, Read, Write};
    use ToolKind::{Destructive, ReadOnly};

    let value = match name {
        "read_file" => entry(
            name_static("read_file"),
            ReadOnly,
            Read,
            Safe,
            Never,
            Immediate,
            READ,
        ),
        "list_dir" => entry(
            name_static("list_dir"),
            ReadOnly,
            Read,
            Safe,
            Never,
            Immediate,
            READ,
        ),
        "grep" => entry(
            name_static("grep"),
            ReadOnly,
            Read,
            Safe,
            Never,
            KillSubprocess,
            READ,
        ),
        "glob" => entry(
            name_static("glob"),
            ReadOnly,
            Read,
            Safe,
            Never,
            Immediate,
            READ,
        ),
        "bulk_read" => entry(
            name_static("bulk_read"),
            ReadOnly,
            Read,
            Safe,
            Never,
            Cooperative,
            READ,
        ),
        "todo_read" => entry(
            name_static("todo_read"),
            ReadOnly,
            Read,
            Safe,
            Never,
            Immediate,
            READ,
        ),
        "diagnostics" => entry(
            name_static("diagnostics"),
            ReadOnly,
            External,
            Safe,
            Never,
            KillSubprocess,
            PROCESS,
        ),
        "git_status" => entry(
            name_static("git_status"),
            ReadOnly,
            Read,
            Safe,
            Never,
            KillSubprocess,
            READ,
        ),
        "git_diff" => entry(
            name_static("git_diff"),
            ReadOnly,
            Read,
            Safe,
            Never,
            KillSubprocess,
            READ,
        ),
        "git_log" => entry(
            name_static("git_log"),
            ReadOnly,
            Read,
            Safe,
            Never,
            KillSubprocess,
            READ,
        ),
        "git_show" => entry(
            name_static("git_show"),
            ReadOnly,
            Read,
            Safe,
            Never,
            KillSubprocess,
            READ,
        ),
        "workspace_activity" => entry(
            name_static("workspace_activity"),
            ReadOnly,
            Read,
            Safe,
            Never,
            Immediate,
            READ,
        ),
        "fetch" => entry(
            name_static("fetch"),
            Destructive,
            External,
            Safe,
            Inherit,
            Cooperative,
            NETWORK,
        ),
        // Align with fetch: network egress should not be auto-approved under
        // default Destructive mode while fetch is gated (CORE_REVIEW).
        "web_search" => entry(
            name_static("web_search"),
            Destructive,
            External,
            Safe,
            Inherit,
            Cooperative,
            NETWORK,
        ),
        "mcp" => entry(
            "mcp",
            Destructive,
            External,
            Sequential,
            Inherit,
            Cooperative,
            NETWORK,
        ),
        "finish" | "load_tools" | "goal_write_plan" => entry(
            match name {
                "finish" => "finish",
                "load_tools" => "load_tools",
                _ => "goal_write_plan",
            },
            ReadOnly,
            Control,
            Sequential,
            Never,
            Immediate,
            NONE,
        ),
        // memory/collections mutate durable state (save/forget/index/remove).
        // Classify Destructive + Inherit so default Destructive mode prompts;
        // knowledge is read-only ranking over existing stores (CORE_REVIEW).
        "memory" => entry(
            "memory",
            Destructive,
            Control,
            Sequential,
            Inherit,
            Cooperative,
            WRITE,
        ),
        "knowledge" => entry(
            "knowledge",
            ReadOnly,
            Control,
            Sequential,
            Never,
            Cooperative,
            READ,
        ),
        "collections" => entry(
            name_static("collections"),
            Destructive,
            Control,
            Sequential,
            Inherit,
            Cooperative,
            READ_WRITE,
        ),
        "ask" | "contact_supervisor" | "intercom" => entry(
            match name {
                "ask" => "ask",
                "contact_supervisor" => "contact_supervisor",
                _ => "intercom",
            },
            ReadOnly,
            Interactive,
            Sequential,
            Never,
            Cooperative,
            INTERACTIVE,
        ),
        "eval" => entry(
            "eval",
            ReadOnly,
            External,
            Sequential,
            Never,
            KillSubprocess,
            PROCESS,
        ),
        "read" => entry("read", ReadOnly, External, Safe, Never, Cooperative, READ),
        "subagent" | "spawn" => entry(
            if name == "subagent" {
                "subagent"
            } else {
                "spawn"
            },
            Destructive,
            Control,
            Sequential,
            Inherit,
            Cooperative,
            CHILD,
        ),
        "bash" | "test_env" => entry(
            if name == "bash" { "bash" } else { "test_env" },
            Destructive,
            External,
            Sequential,
            Inherit,
            KillSubprocess,
            PROCESS,
        ),
        "process" => entry(
            "process",
            // The approval loop refines this by action: status/logs bypass the
            // gate while start/stop/restart retain this fail-closed default.
            Destructive,
            External,
            Sequential,
            Inherit,
            KillSubprocess,
            PROCESS,
        ),
        "snapshot_edit" | "ast_edit" | "lsp" | "debug" => entry(
            static_name_ide(name)?,
            // lsp spawns a host language-server process — treat as Destructive
            // so it is not auto-approved under default Destructive mode
            // (CORE_REVIEW C4). Command is also allowlisted in ide.rs.
            ToolKind::Destructive,
            if name == "lsp" || name == "debug" {
                External
            } else {
                Write
            },
            Sequential,
            Inherit,
            KillSubprocess,
            if name == "lsp" || name == "debug" {
                PROCESS
            } else {
                WRITE
            },
        ),
        "write_file" | "edit" | "patch" | "delete" | "rename" | "mkdir" | "bulk_write"
        | "bulk_edit" | "todo_write" | "git_add" | "git_commit" | "git_push" | "git_pull"
        | "git_branch" | "bulk" => entry(
            static_name(name)?,
            Destructive,
            Write,
            Sequential,
            Inherit,
            Cooperative,
            WRITE,
        ),
        name if crate::browser::is_browser_tool(name) => entry(
            browser_name(name)?,
            if crate::browser::is_browser_readonly(name) {
                ReadOnly
            } else {
                Destructive
            },
            External,
            Sequential,
            Inherit,
            Cooperative,
            NETWORK,
        ),
        _ => return None,
    };
    Some(value)
}

const fn name_static(name: &'static str) -> &'static str {
    name
}

fn static_name(name: &str) -> Option<&'static str> {
    Some(match name {
        "write_file" => "write_file",
        "edit" => "edit",
        "patch" => "patch",
        "delete" => "delete",
        "rename" => "rename",
        "mkdir" => "mkdir",
        "bulk_write" => "bulk_write",
        "bulk_edit" => "bulk_edit",
        "todo_write" => "todo_write",
        "git_add" => "git_add",
        "git_commit" => "git_commit",
        "git_push" => "git_push",
        "git_pull" => "git_pull",
        "git_branch" => "git_branch",
        "bulk" => "bulk",
        _ => return None,
    })
}
fn static_name_ide(name: &str) -> Option<&'static str> {
    Some(match name {
        "snapshot_edit" => "snapshot_edit",
        "ast_edit" => "ast_edit",
        "lsp" => "lsp",
        "debug" => "debug",
        _ => return None,
    })
}

fn browser_name(name: &str) -> Option<&'static str> {
    Some(match name {
        "browser_create" => "browser_create",
        "browser_close" => "browser_close",
        "browser_list_sessions" => "browser_list_sessions",
        "browser_navigate" => "browser_navigate",
        "browser_back" => "browser_back",
        "browser_reload" => "browser_reload",
        "browser_snapshot" => "browser_snapshot",
        "browser_find" => "browser_find",
        "browser_click" => "browser_click",
        "browser_fill" => "browser_fill",
        "browser_type" => "browser_type",
        "browser_press" => "browser_press",
        "browser_scroll" => "browser_scroll",
        "browser_wait" => "browser_wait",
        "browser_evaluate" => "browser_evaluate",
        "browser_screenshot" => "browser_screenshot",
        "browser_show" => "browser_show",
        "browser_hide" => "browser_hide",
        _ => return None,
    })
}
