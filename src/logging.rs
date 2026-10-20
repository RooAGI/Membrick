//! Process-wide diagnostic logging.
//!
//! Every Membrick process writes one line per event to stderr:
//!
//! ```text
//! 2026-10-18T21:05:12.345Z  INFO resident[73620] membrick::local_ipc: resident IPC listening endpoint=/path/mb.sock
//! ```
//!
//! The fields are the UTC time, the level, the process role and id, the
//! module that logged, then the message. A resident's stderr is
//! `.membrick/runtime/resident.log`, so its lines interleave there with any
//! starter that lost the owner lock, and the role and id tell them apart.
//!
//! The level comes from `.membrick/server.toml` in the working directory:
//!
//! ```toml
//! [logging]
//! level = "debug"
//! ```
//!
//! Background roles default to `info`; interactive CLI commands default to
//! `warn` so their own output stays readable. Other crates log at `warn` and
//! above only.

use std::fmt;
use std::path::Path;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

/// What kind of Membrick process is logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// The long-lived workspace owner.
    Resident,
    /// A provider-launched stdio MCP adapter.
    Mcp,
    /// A short-lived provider hook.
    Hook,
    /// The standalone HTTP server.
    Server,
    /// An interactive command.
    Cli,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Self::Resident => "resident",
            Self::Mcp => "mcp",
            Self::Hook => "hook",
            Self::Server => "server",
            Self::Cli => "cli",
        }
    }

    fn default_level(self) -> LevelFilter {
        match self {
            Self::Cli => LevelFilter::WARN,
            _ => LevelFilter::INFO,
        }
    }

    /// The role implied by a Membrick command line.
    pub(crate) fn from_args<S: AsRef<str>>(args: &[S]) -> Self {
        let args = args.iter().map(AsRef::as_ref).collect::<Vec<_>>();
        if args.contains(&"--resident-server") {
            return Self::Resident;
        }
        let is_serve = |arg: &str| arg == "serve" || arg.ends_with("-serve");
        let is_hook = |arg: &str| arg == "hook" || arg.ends_with("-hook");
        // Only the subcommand position counts, so a path argument named
        // "serve" or "hook" does not change the role.
        match args.get(1).copied() {
            Some(arg) if is_serve(arg) => Self::Mcp,
            Some(arg) if is_hook(arg) => Self::Hook,
            _ => Self::Cli,
        }
    }
}

/// The level the process logger uses and where it came from: `server.toml`
/// or the role's default.
static ACTIVE_LEVEL: std::sync::OnceLock<(LevelFilter, &'static str)> = std::sync::OnceLock::new();

/// The level chosen by [`init`], so startup can report what is in effect.
pub(crate) fn active_level() -> Option<(LevelFilter, &'static str)> {
    ACTIVE_LEVEL.get().copied()
}

/// The level a process of `role` started in `project_root` logs at, and where
/// it came from.
fn effective_level(role: Role, project_root: &Path) -> (LevelFilter, &'static str) {
    match configured_level(project_root) {
        Some(level) => (level, "server.toml"),
        None => (role.default_level(), "default"),
    }
}

/// Install the process-wide logger. Later calls keep the first logger.
pub(crate) fn init(role: Role, project_root: &Path) {
    let (level, source) = effective_level(role, project_root);
    let _ = ACTIVE_LEVEL.set((level, source));
    let filter = Targets::new()
        .with_target("membrick", level)
        .with_default(LevelFilter::WARN);
    let layer = tracing_subscriber::fmt::layer()
        .event_format(LineFormat {
            role: role.name(),
            pid: std::process::id(),
        })
        .with_writer(std::io::stderr)
        .with_filter(filter);
    let _ = tracing_subscriber::registry().with(layer).try_init();
}

/// `[logging] level` from `<root>/.membrick/server.toml`, if set and valid.
fn configured_level(project_root: &Path) -> Option<LevelFilter> {
    let path = project_root.join(".membrick").join("server.toml");
    let settings = std::fs::read_to_string(path).ok()?;
    let level = settings
        .parse::<toml::Value>()
        .ok()?
        .get("logging")?
        .get("level")?
        .as_str()?
        .to_owned();
    parse_level(&level)
}

fn parse_level(level: &str) -> Option<LevelFilter> {
    match level.trim().to_ascii_lowercase().as_str() {
        "off" => Some(LevelFilter::OFF),
        "error" => Some(LevelFilter::ERROR),
        "warn" | "warning" => Some(LevelFilter::WARN),
        "info" => Some(LevelFilter::INFO),
        "debug" => Some(LevelFilter::DEBUG),
        "trace" => Some(LevelFilter::TRACE),
        _ => None,
    }
}

struct LineFormat {
    role: &'static str,
    pid: u32,
}

impl<S, N> FormatEvent<S, N> for LineFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let metadata = event.metadata();
        write!(
            writer,
            "{} {:>5} {}[{}] {}: ",
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ"),
            level_name(*metadata.level()),
            self.role,
            self.pid,
            metadata.target()
        )?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::ERROR => "ERROR",
        Level::WARN => "WARN",
        Level::INFO => "INFO",
        Level::DEBUG => "DEBUG",
        Level::TRACE => "TRACE",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_follow_the_subcommand_position() {
        let role = |args: &[&str]| Role::from_args(args);
        assert_eq!(
            role(&["membrick", "--resident-server", "--managed-resident"]),
            Role::Resident
        );
        assert_eq!(role(&["membrick", "--codex-serve", "."]), Role::Mcp);
        assert_eq!(role(&["membrick", "serve", "codex", "."]), Role::Mcp);
        assert_eq!(
            role(&["membrick", "--claude-code-hook", "stop"]),
            Role::Hook
        );
        assert_eq!(role(&["membrick", "hook", "claude", "stop"]), Role::Hook);
        assert_eq!(role(&["membrick", "status", "serve"]), Role::Cli);
    }

    #[test]
    fn level_comes_from_the_logging_table_in_server_settings() {
        let root = std::env::temp_dir().join(format!("membrick-logging-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".membrick")).unwrap();
        assert_eq!(configured_level(&root), None);
        std::fs::write(
            root.join(".membrick/server.toml"),
            "bind = \"127.0.0.1:8080\"\n[logging]\nlevel = \"Debug\"\n",
        )
        .unwrap();
        assert_eq!(configured_level(&root), Some(LevelFilter::DEBUG));
        std::fs::write(
            root.join(".membrick/server.toml"),
            "[logging]\nlevel = \"loud\"\n",
        )
        .unwrap();
        assert_eq!(
            configured_level(&root),
            None,
            "an unknown level falls back to the default"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_resident_uses_the_level_install_wrote() {
        let root =
            std::env::temp_dir().join(format!("membrick-logging-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(
            effective_level(Role::Resident, &root),
            (LevelFilter::INFO, "default")
        );
        crate::state_paths::initialize_project_state(&root).unwrap();
        assert_eq!(
            effective_level(Role::Resident, &root),
            (LevelFilter::INFO, "server.toml")
        );
        // A level the user sets afterwards is what the next resident uses.
        let settings = root.join(".membrick/server.toml");
        let text = std::fs::read_to_string(&settings)
            .unwrap()
            .replace("level = \"info\"", "level = \"debug\"");
        std::fs::write(&settings, text).unwrap();
        assert_eq!(
            effective_level(Role::Resident, &root),
            (LevelFilter::DEBUG, "server.toml")
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
