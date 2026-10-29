//! Antigravity CLI integration. MCP transport is shared with Gemini CLI, while
//! lifecycle hooks use AGY's independent hooks.json protocol.

pub mod hooks;

use crate::integrations::gemini_cli::{self, GeminiCliServerOptions};
use crate::integrations::session_recording::RecordingProvider;
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const SERVER_NAME: &str = "membrick";
const HOOK_MARKER: &str = "--agy-hook";
const UPDATE_PERMISSION: &str = "mcp(membrick/update_membrick)";
const LEGACY_MANAGER_SERVER_NAME: &str = "membrick_manager";
const MANAGER_ARG: &str = "--manager-serve";

/// Register the user-global "membrick" entry as the setup-only manager.
///
/// AGY reads MCP servers only from the user-global mcp_config.json and from
/// plugins, and plugin servers are renamed `<plugin>_<server>` rather than
/// overriding a global entry. So there is no project-level entry that could
/// shadow the manager inside a project, as Claude Code and Codex have. The
/// manager and a project's memory server share the one global "membrick"
/// name. An existing entry that runs anything other than the manager (an
/// installed project's --agy-serve, or a user's own server) is left alone;
/// installing a project is what replaces the manager entry.
pub fn install_manager_config(config_path: Option<&Path>) -> Result<PathBuf> {
    let path = config_path
        .map(Path::to_path_buf)
        .unwrap_or(default_config_path()?);
    let executable = env::current_exe()?;
    let mut settings = read_json_object(&path)?;
    let servers = settings.entry("mcpServers").or_insert_with(|| json!({}));
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("AGY mcpServers must be an object"))?;
    servers.remove(LEGACY_MANAGER_SERVER_NAME);
    match servers.get(SERVER_NAME) {
        Some(entry) if !entry_runs_manager(entry) => {
            let served = entry_project_root(entry)
                .map(|root| format!(" It serves the AGY project {}.", root.display()))
                .unwrap_or_default();
            eprintln!(
                "Left the AGY MCP server '{SERVER_NAME}' in {} unchanged because it is not a \
                 Membrick manager entry.{served} AGY has one global '{SERVER_NAME}' entry, so \
                 it has no manager tools until you run `membrick uninstall agy <PATH>` for that \
                 project.",
                path.display()
            );
        }
        _ => {
            servers.insert(
                SERVER_NAME.into(),
                json!({"command":executable,"args":[MANAGER_ARG]}),
            );
        }
    }
    write_json_object(&path, &settings)?;
    Ok(path)
}

/// The project the global "membrick" entry serves, if it is an AGY project
/// memory server.
pub fn installed_project_root(config_path: Option<&Path>) -> Result<Option<PathBuf>> {
    let path = config_path
        .map(Path::to_path_buf)
        .unwrap_or(default_config_path()?);
    let settings = read_json_object(&path)?;
    Ok(settings
        .get("mcpServers")
        .and_then(|servers| servers.get(SERVER_NAME))
        .and_then(entry_project_root))
}

fn entry_project_root(entry: &Value) -> Option<PathBuf> {
    let args = entry.get("args").and_then(Value::as_array)?;
    if args.first().and_then(Value::as_str) != Some("--agy-serve") {
        return None;
    }
    args.get(1).and_then(Value::as_str).map(PathBuf::from)
}

/// Record an AGY install in the Membrick project registry, as Claude Code and
/// Codex installs do. AGY's global "membrick" entry serves one project at a
/// time, so a different project the install displaced (`previous`) no longer
/// has AGY memory tools and loses its registration. A displaced folder that no
/// longer exists is unregistered by the canonical path stored in the entry.
/// Returns the displaced project when there was one.
pub(crate) fn register_install(root: &Path, previous: Option<&Path>) -> Result<Option<PathBuf>> {
    register_install_at(
        &crate::integrations::project_registry::registry_path()?,
        root,
        previous,
    )
}

fn register_install_at(
    registry: &Path,
    root: &Path,
    previous: Option<&Path>,
) -> Result<Option<PathBuf>> {
    use crate::integrations::project_registry::{register_at, unregister_stored_at};
    let root = root.canonicalize()?;
    // The entry stores the canonical root, so a missing folder is matched by
    // that string; an existing one is canonicalized again to catch symlinks.
    let displaced = previous
        .map(|previous| previous.canonicalize().unwrap_or_else(|_| previous.into()))
        .filter(|previous| *previous != root);
    if let Some(displaced) = &displaced {
        unregister_stored_at(registry, &displaced.to_string_lossy(), "agy")?;
    }
    register_at(registry, &root, "agy")?;
    Ok(displaced)
}

fn entry_runs_manager(entry: &Value) -> bool {
    entry
        .get("args")
        .and_then(Value::as_array)
        .is_some_and(|args| args.iter().any(|arg| arg.as_str() == Some(MANAGER_ARG)))
}

pub type AgyServerOptions<'a> = GeminiCliServerOptions<'a>;

pub fn install_memory_skill(root: &Path, force: bool) -> Result<PathBuf> {
    let root = root.canonicalize()?;
    let skill_path = root.join(".agents/skills/membrick-memory/SKILL.md");
    if let Some(parent) = skill_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let generated = include_str!("skill.md");
    if let Ok(existing) = fs::read_to_string(&skill_path) {
        if !force && existing != generated {
            anyhow::bail!(
                "refusing to overwrite existing AGY skill {}; it appears user-modified; use --agy-force-skill to replace it",
                skill_path.display()
            );
        }
        if existing == generated {
            return Ok(skill_path);
        }
    }
    fs::write(&skill_path, generated)?;
    Ok(skill_path)
}

pub fn install_user_config(root: &Path, config_path: Option<&Path>) -> Result<PathBuf> {
    let path = config_path
        .map(Path::to_path_buf)
        .unwrap_or(default_config_path()?);
    let root = root.canonicalize()?;
    let executable = env::current_exe()?;
    let mut settings = read_json_object(&path)?;
    let servers = settings.entry("mcpServers").or_insert_with(|| json!({}));
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("AGY mcpServers must be an object"))?;
    servers.remove("lint-ai");
    servers.remove(LEGACY_MANAGER_SERVER_NAME);
    // Replaces the setup-only manager entry, if present, with this project's
    // memory server under the same name.
    servers.insert(
        SERVER_NAME.into(),
        json!({
            "command": executable,
            "args": ["--agy-serve", root.to_string_lossy()]
        }),
    );
    write_json_object(&path, &settings)?;
    Ok(path)
}

pub fn install_hook_settings(root: &Path, settings_path: Option<&Path>) -> Result<PathBuf> {
    let path = settings_path
        .map(Path::to_path_buf)
        .unwrap_or(default_settings_path()?);
    let root = root.canonicalize()?;
    let executable = env::current_exe()?;
    let mut settings = read_json_object(&path)?;
    let hooks = settings.entry("membrick").or_insert_with(|| json!({}));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("AGY hooks must be an object"))?;
    for (event, name) in [
        ("PreToolUse", "pre-tool-use"),
        ("PostToolUse", "post-tool-use"),
        ("PreInvocation", "pre-invocation"),
        ("PostInvocation", "post-invocation"),
        ("Stop", "stop"),
    ] {
        let entries = hooks.entry(event).or_insert_with(|| json!([]));
        let entries = entries
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("AGY hook {event} must be an array"))?;
        entries.retain(|entry| {
            !entry.as_object().is_some_and(Map::is_empty)
                && !serde_json::to_string(entry)
                    .unwrap_or_default()
                    .contains(HOOK_MARKER)
        });
        let command = json!({"type":"command", "command": format!("{} {HOOK_MARKER} {name} {}", shell_quote(&executable.to_string_lossy()), shell_quote(&root.to_string_lossy()))});
        if matches!(event, "PreToolUse" | "PostToolUse") {
            entries.push(json!({"matcher":".*", "hooks":[command]}));
        } else {
            entries.push(command);
        }
    }
    write_json_object(&path, &settings)?;
    Ok(path)
}

/// Require explicit AGY approval for the self-update MCP action.
pub fn install_update_permission(settings_path: Option<&Path>) -> Result<PathBuf> {
    let path = settings_path
        .map(Path::to_path_buf)
        .unwrap_or(default_permission_settings_path()?);
    let mut settings = read_json_object(&path)?;
    let permissions = settings.entry("permissions").or_insert_with(|| json!({}));
    let permissions = permissions
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("AGY permissions must be an object"))?;
    let ask = permissions.entry("ask").or_insert_with(|| json!([]));
    let ask = ask
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("AGY permissions.ask must be an array"))?;
    if !ask
        .iter()
        .any(|rule| rule.as_str() == Some(UPDATE_PERMISSION))
    {
        ask.push(Value::String(UPDATE_PERMISSION.to_string()));
    }
    write_json_object(&path, &settings)?;
    Ok(path)
}

/// Remove the AGY configuration Membrick installed for `root`. Project memory
/// data, the executable and the shared update approval are left in place.
///
/// AGY has one user-global MCP config, so a "membrick" entry that serves this
/// root is replaced with the setup-only manager entry, keeping the project
/// manager tools available. Entries for other roots are left alone.
pub fn uninstall(
    root: &Path,
    config_path: Option<&Path>,
    settings_path: Option<&Path>,
) -> Result<()> {
    let root = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", root.display()))?;
    let root_arg = root.to_string_lossy().into_owned();

    let config_path = config_path
        .map(Path::to_path_buf)
        .unwrap_or(default_config_path()?);
    if config_path.exists() {
        let mut settings = read_json_object(&config_path)?;
        if remove_project_servers(&mut settings, &root_arg)? {
            write_json_object(&config_path, &settings)?;
        }
    }

    let settings_path = settings_path
        .map(Path::to_path_buf)
        .unwrap_or(default_settings_path()?);
    if settings_path.exists() {
        let mut settings = read_json_object(&settings_path)?;
        if remove_project_hooks(&mut settings, &shell_quote(&root_arg)) {
            write_json_object(&settings_path, &settings)?;
        }
    }

    let skill_dir = root.join(".agents/skills/membrick-memory");
    let skill_path = skill_dir.join("SKILL.md");
    if skill_path.exists() {
        let contents = fs::read_to_string(&skill_path)
            .with_context(|| format!("failed to read {}", skill_path.display()))?;
        if contents == include_str!("skill.md") {
            fs::remove_file(&skill_path)
                .with_context(|| format!("failed to remove {}", skill_path.display()))?;
            remove_dir_if_empty(&skill_dir)?;
        } else {
            eprintln!(
                "leaving {} in place because it differs from the skill Membrick installed",
                skill_path.display()
            );
        }
    }
    Ok(())
}

/// Swap a "membrick" entry serving `root` for the manager entry and drop a
/// legacy "lint-ai" entry serving `root`. Returns whether anything changed.
fn remove_project_servers(settings: &mut Map<String, Value>, root_arg: &str) -> Result<bool> {
    let Some(servers) = settings.get_mut("mcpServers") else {
        return Ok(false);
    };
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("AGY mcpServers must be an object"))?;
    let mut changed = false;
    if servers
        .get("lint-ai")
        .is_some_and(|entry| entry_args_contain(entry, root_arg))
    {
        servers.remove("lint-ai");
        changed = true;
    }
    if servers
        .get(SERVER_NAME)
        .is_some_and(|entry| entry_serves_root(entry, root_arg))
    {
        let executable = env::current_exe()?;
        servers.insert(
            SERVER_NAME.into(),
            json!({"command":executable,"args":[MANAGER_ARG]}),
        );
        changed = true;
    }
    Ok(changed)
}

fn entry_serves_root(entry: &Value, root_arg: &str) -> bool {
    let Some(args) = entry.get("args").and_then(Value::as_array) else {
        return false;
    };
    args.first().and_then(Value::as_str) == Some("--agy-serve")
        && args.get(1).and_then(Value::as_str) == Some(root_arg)
}

fn entry_args_contain(entry: &Value, root_arg: &str) -> bool {
    entry
        .get("args")
        .and_then(Value::as_array)
        .is_some_and(|args| args.iter().any(|arg| arg.as_str() == Some(root_arg)))
}

/// Remove the hook commands install_hook_settings wrote for this root, which
/// end in `--agy-hook <event> <quoted root>`. Other hooks are kept.
fn remove_project_hooks(settings: &mut Map<String, Value>, quoted_root: &str) -> bool {
    let Some(hooks) = settings.get_mut("membrick").and_then(Value::as_object_mut) else {
        return false;
    };
    let is_project_command = |hook: &Value| {
        hook.get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| {
                command.contains(HOOK_MARKER) && command.ends_with(&format!(" {quoted_root}"))
            })
    };
    let mut changed = false;
    let events = hooks.keys().cloned().collect::<Vec<_>>();
    for event in events {
        let Some(entries) = hooks.get_mut(&event).and_then(Value::as_array_mut) else {
            continue;
        };
        let mut event_changed = false;
        entries.retain_mut(|entry| {
            if is_project_command(entry) {
                event_changed = true;
                return false;
            }
            if let Some(commands) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
                let count = commands.len();
                commands.retain(|hook| !is_project_command(hook));
                if commands.len() != count {
                    event_changed = true;
                    return !commands.is_empty();
                }
            }
            true
        });
        if event_changed && entries.is_empty() {
            hooks.remove(&event);
        }
        changed |= event_changed;
    }
    if changed && hooks.is_empty() {
        settings.remove("membrick");
    }
    changed
}

fn remove_dir_if_empty(path: &Path) -> Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

pub fn run_server(root: &Path, options: AgyServerOptions<'_>) -> Result<()> {
    gemini_cli::run_server_for(root, RecordingProvider::Agy, "agy", "Gemini", options)
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME or USERPROFILE is not set"))
}
fn default_config_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".gemini/config/mcp_config.json"))
}
fn default_settings_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".gemini/config/hooks.json"))
}
fn default_permission_settings_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".gemini/antigravity-cli/settings.json"))
}
fn read_json_object(path: &Path) -> Result<Map<String, Value>> {
    if !path.exists() {
        return Ok(Map::new());
    }
    let contents = fs::read_to_string(path)?;
    if contents.trim().is_empty() {
        return Ok(Map::new());
    }
    Ok(serde_json::from_str(&contents)?)
}
fn write_json_object(path: &Path, value: &Map<String, Value>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(value)? + "\n")?;
    Ok(())
}
fn shell_quote(value: &str) -> String {
    if value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/_-.".contains(&b))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    /// A fresh directory per call. The process id and a counter keep parallel
    /// tests apart, which a coarse clock alone does not.
    fn temp_root() -> PathBuf {
        let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("membrick-agy-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn uses_agy_configuration_paths() {
        let root = PathBuf::from("/tmp/project");
        assert!(default_config_path()
            .unwrap()
            .to_string_lossy()
            .contains("mcp_config.json"));
        assert_eq!(HOOK_MARKER, "--agy-hook");
        let _ = root;
    }

    fn read_config(path: &Path) -> Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn global_manager_config_preserves_existing_servers() {
        let path = temp_root().join("mcp_config.json");
        fs::write(&path, r#"{"mcpServers":{"other":{"command":"other"}}}"#).unwrap();
        install_manager_config(Some(&path)).unwrap();
        let value = read_config(&path);
        assert_eq!(
            value["mcpServers"][SERVER_NAME]["args"][0],
            "--manager-serve"
        );
        assert_eq!(value["mcpServers"]["other"]["command"], "other");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn global_manager_config_writes_membrick_on_empty_config() {
        let path = temp_root().join("mcp_config.json");
        install_manager_config(Some(&path)).unwrap();
        install_manager_config(Some(&path)).unwrap();
        let value = read_config(&path);
        let servers = value["mcpServers"].as_object().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[SERVER_NAME]["args"], json!(["--manager-serve"]));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn global_manager_config_removes_legacy_manager_entry() {
        let path = temp_root().join("mcp_config.json");
        fs::write(
            &path,
            r#"{"mcpServers":{"membrick_manager":{"command":"old","args":["--manager-serve"]}}}"#,
        )
        .unwrap();
        install_manager_config(Some(&path)).unwrap();
        let value = read_config(&path);
        assert!(value["mcpServers"]
            .get(LEGACY_MANAGER_SERVER_NAME)
            .is_none());
        assert_eq!(
            value["mcpServers"][SERVER_NAME]["args"][0],
            "--manager-serve"
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn global_manager_config_leaves_installed_project_entry() {
        let path = temp_root().join("mcp_config.json");
        fs::write(
            &path,
            r#"{"mcpServers":{"membrick":{"command":"membrick","args":["--agy-serve","/work/project"]},"membrick_manager":{"command":"old","args":["--manager-serve"]}}}"#,
        )
        .unwrap();
        install_manager_config(Some(&path)).unwrap();
        let value = read_config(&path);
        assert_eq!(
            value["mcpServers"][SERVER_NAME]["args"],
            json!(["--agy-serve", "/work/project"])
        );
        assert_eq!(value["mcpServers"][SERVER_NAME]["command"], "membrick");
        assert!(value["mcpServers"]
            .get(LEGACY_MANAGER_SERVER_NAME)
            .is_none());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn project_install_replaces_manager_entry_and_removes_legacy() {
        let root = temp_root();
        let path = root.join("mcp_config.json");
        fs::write(
            &path,
            r#"{"mcpServers":{"membrick_manager":{"command":"old","args":["--manager-serve"]}}}"#,
        )
        .unwrap();
        install_manager_config(Some(&path)).unwrap();
        install_user_config(&root, Some(&path)).unwrap();
        let value = read_config(&path);
        let canonical = root.canonicalize().unwrap();
        assert_eq!(
            value["mcpServers"][SERVER_NAME]["args"],
            json!(["--agy-serve", canonical.to_string_lossy()])
        );
        assert!(value["mcpServers"]
            .get(LEGACY_MANAGER_SERVER_NAME)
            .is_none());

        // Re-running global setup afterwards must not clobber the project.
        install_manager_config(Some(&path)).unwrap();
        let value = read_config(&path);
        assert_eq!(value["mcpServers"][SERVER_NAME]["args"][0], "--agy-serve");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn memory_skill_preserves_user_edits_without_force() {
        let root = temp_root();
        let path = root.join(".agents/skills/membrick-memory/SKILL.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "custom").unwrap();

        let error = install_memory_skill(&root, false).unwrap_err().to_string();
        assert!(error.contains("--agy-force-skill"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "custom");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn memory_skill_force_replaces_user_edits_and_is_idempotent() {
        let root = temp_root();

        install_memory_skill(&root, true).unwrap();
        let path = root.join(".agents/skills/membrick-memory/SKILL.md");
        let installed = fs::read_to_string(&path).unwrap();
        assert!(installed.contains("<!-- membrick-managed-skill -->"));
        install_memory_skill(&root, false).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), installed);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn installation_is_idempotent_and_preserves_existing_configuration() {
        let root = temp_root();
        let config = root.join("mcp-config.json");
        let settings = root.join("settings.json");
        fs::write(&config, r#"{"mcpServers":{"other":{"command":"other"}}}"#).unwrap();
        fs::write(
            &settings,
            r#"{"theme":"dark","hooks":{"BeforeAgent":[{}, {"hooks":[{"type":"command","command":"user-hook"}]}]}}"#,
        )
        .unwrap();

        install_user_config(&root, Some(&config)).unwrap();
        install_user_config(&root, Some(&config)).unwrap();
        install_hook_settings(&root, Some(&settings)).unwrap();
        install_hook_settings(&root, Some(&settings)).unwrap();

        let config: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(config["mcpServers"]["other"]["command"], "other");
        assert_eq!(config["mcpServers"]["membrick"]["args"][0], "--agy-serve");
        assert_eq!(
            config["mcpServers"]["membrick"]["args"][1],
            root.to_string_lossy().as_ref()
        );
        let settings: Value =
            serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(settings["theme"], "dark");
        assert_eq!(settings["membrick"]["Stop"].as_array().unwrap().len(), 1);
        assert_eq!(
            settings["membrick"]["PreToolUse"].as_array().unwrap().len(),
            1
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn uninstall_restores_manager_and_removes_only_this_project() {
        let root = temp_root();
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        let canonical = project
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let config = root.join("mcp_config.json");
        let hooks = root.join("hooks.json");
        fs::write(
            &config,
            json!({"mcpServers": {"other": {"command": "other"}}}).to_string(),
        )
        .unwrap();
        fs::write(
            &hooks,
            r#"{"theme":"dark","membrick":{"Stop":[{"type":"command","command":"user-hook"}],"PreToolUse":[{"matcher":"x","hooks":[{"type":"command","command":"user-tool-hook"}]}]}}"#,
        )
        .unwrap();
        install_memory_skill(&project, false).unwrap();
        install_manager_config(Some(&config)).unwrap();
        install_user_config(&project, Some(&config)).unwrap();
        install_hook_settings(&project, Some(&hooks)).unwrap();
        // A legacy entry for this root, which uninstall also removes.
        let mut value = read_config(&config);
        value["mcpServers"]["lint-ai"] =
            json!({"command": "lint-ai", "args": ["--agy-serve", canonical]});
        fs::write(&config, value.to_string()).unwrap();

        uninstall(&project, Some(&config), Some(&hooks)).unwrap();
        let value = read_config(&config);
        assert_eq!(
            value["mcpServers"][SERVER_NAME]["args"],
            json!([MANAGER_ARG])
        );
        assert_eq!(value["mcpServers"]["other"]["command"], "other");
        assert!(value["mcpServers"].get("lint-ai").is_none());
        let settings = read_config(&hooks);
        assert_eq!(settings["theme"], "dark");
        assert_eq!(
            settings["membrick"]["Stop"],
            json!([{"type":"command","command":"user-hook"}])
        );
        assert_eq!(
            settings["membrick"]["PreToolUse"],
            json!([{"matcher":"x","hooks":[{"type":"command","command":"user-tool-hook"}]}])
        );
        assert!(settings["membrick"].get("PostInvocation").is_none());
        assert!(!serde_json::to_string(&settings)
            .unwrap()
            .contains(HOOK_MARKER));
        assert!(!project.join(".agents/skills/membrick-memory").exists());

        // A second run changes nothing.
        let config_before = fs::read(&config).unwrap();
        let hooks_before = fs::read(&hooks).unwrap();
        uninstall(&project, Some(&config), Some(&hooks)).unwrap();
        assert_eq!(fs::read(&config).unwrap(), config_before);
        assert_eq!(fs::read(&hooks).unwrap(), hooks_before);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn uninstall_leaves_another_roots_entry_and_hooks() {
        let root = temp_root();
        let project = root.join("project");
        let other = root.join("other");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&other).unwrap();
        let config = root.join("mcp_config.json");
        let hooks = root.join("hooks.json");
        install_user_config(&other, Some(&config)).unwrap();
        install_hook_settings(&other, Some(&hooks)).unwrap();
        let config_before = fs::read(&config).unwrap();
        let hooks_before = fs::read(&hooks).unwrap();

        uninstall(&project, Some(&config), Some(&hooks)).unwrap();
        assert_eq!(fs::read(&config).unwrap(), config_before);
        assert_eq!(fs::read(&hooks).unwrap(), hooks_before);
        let value = read_config(&config);
        assert_eq!(value["mcpServers"][SERVER_NAME]["args"][0], "--agy-serve");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn uninstall_keeps_customised_skill_and_tolerates_missing_files() {
        let root = temp_root();
        let skill = root.join(".agents/skills/membrick-memory/SKILL.md");
        fs::create_dir_all(skill.parent().unwrap()).unwrap();
        fs::write(&skill, "custom").unwrap();
        let config = root.join("missing/mcp_config.json");
        let hooks = root.join("missing/hooks.json");

        uninstall(&root, Some(&config), Some(&hooks)).unwrap();
        assert_eq!(fs::read_to_string(&skill).unwrap(), "custom");
        assert!(!config.exists());
        assert!(!hooks.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn installed_project_root_reads_only_project_entries() {
        let root = temp_root();
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        let config = root.join("mcp_config.json");

        assert_eq!(installed_project_root(Some(&config)).unwrap(), None);
        install_manager_config(Some(&config)).unwrap();
        assert_eq!(installed_project_root(Some(&config)).unwrap(), None);
        install_user_config(&project, Some(&config)).unwrap();
        assert_eq!(
            installed_project_root(Some(&config)).unwrap(),
            Some(project.canonicalize().unwrap())
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn global_manager_config_after_install_keeps_project_and_registration() {
        let root = temp_root();
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        let config = root.join("mcp_config.json");
        let registry = root.join("installed-projects.json");
        install_manager_config(Some(&config)).unwrap();
        let previous = installed_project_root(Some(&config)).unwrap();
        install_user_config(&project, Some(&config)).unwrap();
        register_install_at(&registry, &project, previous.as_deref()).unwrap();

        install_manager_config(Some(&config)).unwrap();
        assert_eq!(
            installed_project_root(Some(&config)).unwrap(),
            Some(project.canonicalize().unwrap())
        );
        let status =
            crate::integrations::project_registry::status_at(&registry, &project, "agy").unwrap();
        assert!(status.installed);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_registers_agy_and_uninstall_unregisters() {
        use crate::integrations::project_registry::{
            list_at, register_at, status_at, unregister_at,
        };
        let root = temp_root();
        let first = root.join("first");
        let second = root.join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let config = root.join("mcp_config.json");
        let hooks = root.join("hooks.json");
        let registry = root.join("installed-projects.json");
        install_manager_config(Some(&config)).unwrap();
        register_at(&registry, &first, "claude").unwrap();

        // First install: registered for agy, shown by status and list.
        let previous = installed_project_root(Some(&config)).unwrap();
        install_user_config(&first, Some(&config)).unwrap();
        install_hook_settings(&first, Some(&hooks)).unwrap();
        assert_eq!(
            register_install_at(&registry, &first, previous.as_deref()).unwrap(),
            None
        );
        let status = status_at(&registry, &first, "agy").unwrap();
        assert!(status.installed);
        assert_eq!(status.providers, ["agy", "claude"]);
        let first_path = first.canonicalize().unwrap().to_string_lossy().into_owned();
        let listed = list_at(&registry).unwrap();
        assert!(
            listed
                .iter()
                .any(|project| project.path == first_path
                    && project.providers.contains(&"agy".into()))
        );

        // Reinstalling the same project displaces nothing.
        let previous = installed_project_root(Some(&config)).unwrap();
        install_user_config(&first, Some(&config)).unwrap();
        assert_eq!(
            register_install_at(&registry, &first, previous.as_deref()).unwrap(),
            None
        );

        // A second install takes the one global entry, so the first project
        // loses its agy registration but keeps its other providers.
        let previous = installed_project_root(Some(&config)).unwrap();
        install_user_config(&second, Some(&config)).unwrap();
        install_hook_settings(&second, Some(&hooks)).unwrap();
        assert_eq!(
            register_install_at(&registry, &second, previous.as_deref()).unwrap(),
            Some(first.canonicalize().unwrap())
        );
        let status = status_at(&registry, &first, "agy").unwrap();
        assert!(!status.installed);
        assert_eq!(status.providers, ["claude"]);
        assert!(status_at(&registry, &second, "agy").unwrap().installed);

        // Uninstall, as `membrick uninstall agy` runs it, restores the
        // manager and removes the registration.
        uninstall(&second, Some(&config), Some(&hooks)).unwrap();
        unregister_at(&registry, &second, "agy").unwrap();
        assert_eq!(installed_project_root(Some(&config)).unwrap(), None);
        assert_eq!(
            read_config(&config)["mcpServers"][SERVER_NAME]["args"],
            json!([MANAGER_ARG])
        );
        assert!(!status_at(&registry, &second, "agy").unwrap().installed);
        let second_path = second
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(!list_at(&registry)
            .unwrap()
            .iter()
            .any(|project| project.path == second_path));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_unregisters_a_displaced_project_that_no_longer_exists() {
        use crate::integrations::project_registry::{list_at, register_at};
        let root = temp_root();
        let gone = root.join("gone");
        let current = root.join("current");
        fs::create_dir_all(&gone).unwrap();
        fs::create_dir_all(&current).unwrap();
        let config = root.join("mcp_config.json");
        let registry = root.join("installed-projects.json");
        install_user_config(&gone, Some(&config)).unwrap();
        register_install_at(&registry, &gone, None).unwrap();
        register_at(&registry, &gone, "claude").unwrap();
        let gone_path = gone.canonicalize().unwrap();
        fs::remove_dir_all(&gone).unwrap();

        let previous = installed_project_root(Some(&config)).unwrap();
        assert_eq!(previous.as_deref(), Some(gone_path.as_path()));
        install_user_config(&current, Some(&config)).unwrap();
        assert_eq!(
            register_install_at(&registry, &current, previous.as_deref()).unwrap(),
            Some(gone_path.clone())
        );
        let listed = list_at(&registry).unwrap();
        let gone_entry = listed
            .iter()
            .find(|project| project.path == gone_path.to_string_lossy())
            .unwrap();
        assert_eq!(gone_entry.providers, ["claude"]);
        let current_path = current.canonicalize().unwrap();
        assert!(listed
            .iter()
            .any(|project| project.path == current_path.to_string_lossy()
                && project.providers == ["agy"]));
        fs::remove_dir_all(root).unwrap();
    }
}
