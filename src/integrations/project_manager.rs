//! User-global MCP entrypoint for installing and removing project-scoped
//! Membrick integrations. It has no access to project memory tools.

use crate::integrations::mcp_transport::{
    self, JsonRpcError, JsonRpcRequest, JsonRpcResponse, ToolDefinition,
};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

const SERVER_NAME: &str = "membrick";

pub(crate) fn setup_global() -> Result<()> {
    let mut installed = Vec::new();
    #[cfg(feature = "codex")]
    {
        let path = crate::integrations::codex::install_manager_config(None)?;
        installed.push(("Codex", path));
    }
    #[cfg(feature = "claude-code")]
    {
        let path = crate::integrations::claude_code::install_manager_config(None)?;
        installed.push(("Claude Code", path));
    }
    #[cfg(feature = "agy")]
    {
        let path = crate::integrations::agy::install_manager_config(None)?;
        installed.push(("AGY", path));
    }
    if installed.is_empty() {
        anyhow::bail!("this Membrick build has no Codex, Claude Code, or AGY integration enabled");
    }
    println!("Installed the Membrick project manager in:");
    for (client, path) in installed {
        println!("  {client}: {}", path.display());
    }
    println!("Each agent now has one `membrick` MCP server. Outside installed projects it");
    println!("shows the project manager tools. Inside a project installed with");
    println!("`membrick install <provider> .` the same server shows that project's memory tools.");
    #[cfg(feature = "agy")]
    {
        println!("AGY has no project-level MCP config, so its one global `membrick` server");
        println!("runs the manager until you install an AGY project. After that it serves that");
        println!("project's memory tools in every folder, until `membrick uninstall agy <PATH>`.");
    }
    println!("Restart those agents to load the change.");
    Ok(())
}

pub(crate) fn run_server() -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = io::BufReader::new(stdin.lock());
    let mut writer = stdout.lock();
    mcp_transport::serve_requests(
        &mut reader,
        &mut writer,
        handle_request,
        |_| {},
        |method, error| tracing::warn!(method, "project manager MCP request failed: {error:#}"),
        || {},
    )
}

fn handle_request(request: JsonRpcRequest) -> Result<JsonRpcResponse> {
    let id = request.id;
    let (result, error) = match request.method.as_str() {
        "initialize" => (
            Some(json!({
                "protocolVersion": "2024-11-05",
                "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {"tools": {"listChanged": false}}
            })),
            None,
        ),
        "notifications/initialized" => (None, None),
        "tools/list" => (Some(json!({"tools": tool_definitions()})), None),
        "tools/call" => match dispatch_tool(&request.params.unwrap_or_else(|| json!({}))) {
            Ok(value) => (Some(value), None),
            Err(error) => (
                None,
                Some(JsonRpcError {
                    code: -32602,
                    message: format!("{error:#}"),
                }),
            ),
        },
        method => (
            None,
            Some(JsonRpcError {
                code: -32601,
                message: format!("unknown method: {method}"),
            }),
        ),
    };
    if id.is_none() {
        return Ok(JsonRpcResponse {
            jsonrpc: "2.0",
            id: None,
            result: None,
            error: None,
        });
    }
    Ok(JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result,
        error,
    })
}

fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "membrick_list_projects".into(),
            description: "List projects registered with Membrick on this computer, the providers configured for each, and whether each project is ready.".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        },
        ToolDefinition {
            name: "membrick_project_status".into(),
            description: "Check Membrick registration and project state for one explicit project path and provider (codex, claude or agy).This manager does not read project memories.".into(),
            input_schema: json!({"type":"object","properties":{"project_path":{"type":"string"},"provider":{"type":"string","enum":["codex","claude","agy"]}},"required":["project_path","provider"],"additionalProperties":false}),
        },
        ToolDefinition {
            name: "membrick_install_project".into(),
            description: "Install Membrick's project-scoped MCP, hooks, and guidance for one explicit project path and provider (codex, claude or agy).Writes provider configuration; call only when the user asks to install or enable Membrick for that project. For agy, AGY has one global membrick server, so the install replaces this manager and any other AGY project in every folder until the project is removed.".into(),
            input_schema: json!({"type":"object","properties":{"project_path":{"type":"string"},"provider":{"type":"string","enum":["codex","claude","agy"]}},"required":["project_path","provider"],"additionalProperties":false}),
        },
        ToolDefinition {
            name: "membrick_remove_project".into(),
            description: "Remove Membrick's provider configuration for one explicit project path. Keeps the project's .membrick data and the global project manager. Call only when the user asks to remove Membrick from that project.".into(),
            input_schema: json!({"type":"object","properties":{"project_path":{"type":"string"},"provider":{"type":"string","enum":["codex","claude","agy"]}},"required":["project_path","provider"],"additionalProperties":false}),
        },
    ]
}

fn dispatch_tool(params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match name {
        "membrick_list_projects" => {
            let projects = crate::integrations::project_registry::list()?;
            Ok(tool_result(json!({"projects": projects})))
        }
        "membrick_project_status" => {
            let root = required_project_path(&args)?;
            let provider = required_provider(&args)?;
            let status = crate::integrations::project_registry::status(&root, provider)?;
            Ok(tool_result(serde_json::to_value(status)?))
        }
        "membrick_install_project" | "membrick_remove_project" => {
            let root = required_project_path(&args)?;
            let provider = required_provider(&args)?;
            let action = if name == "membrick_install_project" {
                "install"
            } else {
                "uninstall"
            };
            let output = run_project_command(action, provider, &root)?;
            Ok(tool_result(json!({
                "project_path": root,
                "provider": provider,
                "action": action,
                "output": output,
                "reload_required": true,
                "message": "Restart or reload the agent so it picks up the updated project configuration."
            })))
        }
        _ => anyhow::bail!("unknown project manager tool '{name}'"),
    }
}

fn required_project_path(args: &Value) -> Result<PathBuf> {
    let raw = args
        .get("project_path")
        .and_then(Value::as_str)
        .context("project_path is required")?;
    Path::new(raw)
        .canonicalize()
        .with_context(|| format!("project path {raw} does not exist"))
}

fn required_provider(args: &Value) -> Result<&str> {
    let provider = args
        .get("provider")
        .and_then(Value::as_str)
        .context("provider is required")?;
    anyhow::ensure!(
        matches!(provider, "codex" | "claude" | "agy"),
        "provider must be codex, claude or agy"
    );
    Ok(provider)
}

fn run_project_command(action: &str, provider: &str, root: &Path) -> Result<String> {
    let executable = std::env::current_exe().context("cannot locate Membrick executable")?;
    let output = Command::new(executable)
        .arg(action)
        .arg(provider)
        .arg(root)
        .output()
        .with_context(|| format!("failed to run `membrick {action} {provider}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!(
            "Membrick {action} failed: {}{}",
            stdout.trim(),
            stderr.trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn tool_result(value: Value) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    json!({"content":[{"type":"text","text":text}],"structuredContent":value,"isError":false})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_manager_exposes_only_project_management_tools() {
        let names = tool_definitions()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "membrick_list_projects",
                "membrick_project_status",
                "membrick_install_project",
                "membrick_remove_project",
            ]
        );
        assert!(!names
            .iter()
            .any(|name| name == "search" || name == "board_open"));
    }

    #[test]
    fn project_mutation_tools_require_an_explicit_existing_path_and_provider() {
        let error = dispatch_tool(&json!({
            "name":"membrick_install_project",
            "arguments":{"provider":"codex"}
        }))
        .unwrap_err();
        assert!(error.to_string().contains("project_path is required"));

        let error = dispatch_tool(&json!({
            "name":"membrick_remove_project",
            "arguments":{"project_path":".","provider":"muse"}
        }))
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("provider must be codex, claude or agy"));
    }

    #[test]
    fn protocol_lists_tools_and_rejects_unknown_tool() {
        let response = handle_request(JsonRpcRequest {
            id: Some(json!(1)),
            method: "tools/list".into(),
            params: None,
        })
        .unwrap();
        assert_eq!(
            response.result.unwrap()["tools"].as_array().unwrap().len(),
            4
        );

        let response = handle_request(JsonRpcRequest {
            id: Some(json!(2)),
            method: "tools/call".into(),
            params: Some(json!({"name":"search","arguments":{"query":"secret"}})),
        })
        .unwrap();
        assert_eq!(response.error.unwrap().code, -32602);
    }
}
