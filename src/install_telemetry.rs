use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const TELEMETRY_ENDPOINT: &str = "https://license.rooagi.com/v1/telemetry/install";
const SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct InstallReceipt {
    schema_version: u8,
    installer: String,
    version: String,
    consented: bool,
    #[serde(default)]
    install_id: Option<String>,
    #[serde(default)]
    event_id: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    installed_at_unix: Option<i64>,
    #[serde(default)]
    sent: bool,
}

/// Handle the private install hook and first interactive CLI use. A true
/// result means the invocation was the private hook and normal CLI dispatch
/// should stop.
pub(crate) fn handle_invocation(binary_name: &str, args: &[String]) -> bool {
    if binary_name != "membrick" {
        return false;
    }

    if args.get(1).is_some_and(|arg| arg == "__record-install-telemetry") {
        let installer = args.get(2).map(String::as_str).unwrap_or_default();
        if matches!(installer, "install_sh" | "install_ps1") {
            let agent = args
                .iter()
                .skip(3)
                .find(|arg| matches!(arg.as_str(), "codex" | "claude" | "gemini" | "agy"))
                .map(String::as_str)
                .unwrap_or("none");
            let share = args.iter().any(|arg| arg == "--share-telemetry");
            record_install(installer, agent, share);
        }
        return true;
    }

    if !is_first_interactive_use(args) {
        return false;
    }
    if let Some(installer) = detect_installer() {
        record_install(installer, "none", false);
    }
    false
}

fn is_first_interactive_use(args: &[String]) -> bool {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "-V" | "--version"))
    {
        return false;
    }
    if crate::logging::Role::from_args(args) != crate::logging::Role::Cli {
        return false;
    }
    has_interactive_terminal()
}

fn detect_installer() -> Option<&'static str> {
    let executable = std::env::current_exe().ok()?;
    let parent = executable.parent()?;
    if let Ok(marker) = fs::read_to_string(parent.join("membrick-install-method")) {
        match marker.trim() {
            "pip" => return Some("pip"),
            "npm" => return Some("npm"),
            "standalone" | "install_ps1" => return None,
            _ => {}
        }
    }

    let components = executable
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>();
    if components
        .windows(2)
        .any(|pair| pair[0].eq_ignore_ascii_case("target") && pair[1].eq_ignore_ascii_case("debug"))
    {
        return None;
    }
    if components
        .windows(2)
        .any(|pair| pair[0].eq_ignore_ascii_case("target") && pair[1].eq_ignore_ascii_case("release"))
    {
        return Some("cargo_build");
    }
    if is_cargo_install(&executable) {
        return Some("cargo");
    }
    Some("other")
}

fn is_cargo_install(executable: &Path) -> bool {
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|home| PathBuf::from(home).join(".cargo"))
        });
    cargo_home.is_some_and(|home| executable.starts_with(home.join("bin")))
}

fn has_interactive_terminal() -> bool {
    #[cfg(unix)]
    {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .is_ok()
    }
    #[cfg(not(unix))]
    {
        io::stdin().is_terminal() && io::stderr().is_terminal()
    }
}

fn ask_to_share() -> Option<bool> {
    let prompt = concat!(
        "Share pseudonymous install details with RooAGI? This sends Membrick version, OS, CPU architecture, ",
        "installer source, selected agent, and a random install ID. It excludes names, email, project paths, ",
        "and memory data. The service can see your network address. [y/N] "
    );
    #[cfg(unix)]
    {
        let mut tty = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()?;
        tty.write_all(prompt.as_bytes()).ok()?;
        tty.flush().ok()?;
        let mut answer = String::new();
        BufReader::new(tty).read_line(&mut answer).ok()?;
        Some(matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
    }
    #[cfg(not(unix))]
    {
        if !has_interactive_terminal() {
            return None;
        }
        eprint!("{prompt}");
        io::stderr().flush().ok()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer).ok()?;
        Some(matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
    }
}

fn telemetry_dir() -> Option<PathBuf> {
    let root = if cfg!(windows) {
        std::env::var_os("APPDATA")
            .or_else(|| std::env::var_os("HOME"))
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)?
    } else if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else {
        PathBuf::from(std::env::var_os("HOME")?).join(".config")
    };
    Some(root.join("membrick").join("install-telemetry"))
}

fn record_install(installer: &str, agent: &str, explicit_share: bool) {
    let Some(directory) = telemetry_dir() else {
        return;
    };
    if fs::create_dir_all(&directory).is_err() {
        return;
    }
    set_private_directory_permissions(&directory);

    let version = env!("CARGO_PKG_VERSION");
    let Some(receipt_path) = receipt_path(&directory, installer, version) else {
        return;
    };
    let mut receipt = match fs::read(&receipt_path) {
        Ok(bytes) => match serde_json::from_slice::<InstallReceipt>(&bytes) {
            Ok(receipt) => receipt,
            Err(_) => return,
        },
        Err(_) => {
            let consented = if explicit_share {
                true
            } else {
                match ask_to_share() {
                    Some(consented) => consented,
                    None => return,
                }
            };
            InstallReceipt {
                schema_version: SCHEMA_VERSION,
                installer: installer.to_owned(),
                version: version.to_owned(),
                consented,
                install_id: None,
                event_id: None,
                agent: Some(agent.to_owned()),
                installed_at_unix: None,
                sent: false,
            }
        }
    };

    if !receipt.consented || receipt.sent {
        return;
    }
    if receipt.install_id.is_none() {
        receipt.install_id = Some(read_or_create_install_id(&directory));
        receipt.event_id = Some(Uuid::new_v4().simple().to_string());
        receipt.installed_at_unix = Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs() as i64)
                .unwrap_or_default(),
        );
    }
    if write_receipt(&receipt_path, &receipt).is_err() {
        return;
    }
    if send_event(&receipt) {
        receipt.sent = true;
        let _ = write_receipt(&receipt_path, &receipt);
    }
}

fn receipt_path(directory: &Path, installer: &str, version: &str) -> Option<PathBuf> {
    if !matches!(
        installer,
        "install_sh" | "install_ps1" | "cargo" | "cargo_build" | "pip" | "npm" | "other"
    )
        || version.is_empty()
        || version.len() > 32
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_'))
    {
        return None;
    }
    Some(directory.join(format!("receipt-{installer}-{version}.json")))
}

fn read_or_create_install_id(directory: &Path) -> String {
    let path = directory.join("install-id");
    if let Ok(value) = fs::read_to_string(&path) {
        let value = value.trim();
        if is_valid_id(value) {
            return value.to_owned();
        }
    }
    if let Some(legacy_path) = directory.parent().map(|parent| parent.join("install-id")) {
        if let Ok(value) = fs::read_to_string(legacy_path) {
            let value = value.trim();
            if is_valid_id(value) {
                let _ = write_private_file(&path, value.as_bytes());
                return value.to_owned();
            }
        }
    }
    let value = Uuid::new_v4().simple().to_string();
    if write_private_file(&path, value.as_bytes()).is_ok() {
        value
    } else {
        String::new()
    }
}

fn is_valid_id(value: &str) -> bool {
    (value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        || (value.len() == 36
            && value.bytes().enumerate().all(|(index, byte)| match index {
                8 | 13 | 18 | 23 => byte == b'-',
                _ => byte.is_ascii_hexdigit(),
            }))
}

fn send_event(receipt: &InstallReceipt) -> bool {
    let (Some(install_id), Some(event_id), Some(installed_at_unix)) = (
        receipt.install_id.as_deref(),
        receipt.event_id.as_deref(),
        receipt.installed_at_unix,
    ) else {
        return false;
    };
    if install_id.is_empty() {
        return false;
    }
    let payload = json!({
        "schema_version": SCHEMA_VERSION,
        "event_id": event_id,
        "install_id": install_id,
        "installed_at_unix": installed_at_unix,
        "version": receipt.version,
        "os": operating_system(),
        "architecture": architecture(),
        "installer": receipt.installer,
        "agent": receipt.agent.as_deref().unwrap_or("none"),
        "outcome": "success",
        "failure_stage": "none",
    });
    let Ok(client) = Client::builder().timeout(Duration::from_secs(4)).build() else {
        return false;
    };
    client
        .post(TELEMETRY_ENDPOINT)
        .header(
            reqwest::header::USER_AGENT,
            format!("Membrick-Installer/{}", receipt.version),
        )
        .json(&payload)
        .send()
        .is_ok_and(|response| response.status().is_success())
}

fn operating_system() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macos",
        "linux" => "linux",
        "windows" => "windows",
        _ => "other",
    }
}

fn architecture() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        _ => "other",
    }
}

fn write_receipt(path: &Path, receipt: &InstallReceipt) -> io::Result<()> {
    let bytes = serde_json::to_vec(receipt)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    write_private_file(path, &bytes)
}

fn write_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(contents)?;
    file.sync_all()?;
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temporary, path)
}

fn set_private_directory_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = path;
}
