//! Membrick-owned Linux sandbox profile.
//!
//! Bubblewrap constructs namespaces and a private filesystem. The trusted
//! worker installs Membrick's fixed seccomp deny-list after `exec`, before it
//! reads requests. No launcher flags come from the caller or extension.

use crate::{RuntimeError, RuntimeLimits};
use std::ffi::{CString, OsStr};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const BWRAP_PATH: &str = "/usr/bin/bwrap";
const TEMPFS_BYTES: &str = "16777216";
const ADDRESS_SPACE_OVERHEAD: u64 = 256 * 1024 * 1024;

#[derive(Debug)]
pub(super) struct BubblewrapSandbox {
    worker: PathBuf,
    bwrap: PathBuf,
    runtime_files: Vec<RuntimeFile>,
}

#[derive(Debug)]
struct RuntimeFile {
    source: PathBuf,
    target: PathBuf,
}

impl BubblewrapSandbox {
    pub(super) fn new(worker: PathBuf, limits: &RuntimeLimits) -> Result<Self, RuntimeError> {
        let worker = fs::canonicalize(worker).map_err(|_| RuntimeError::WorkerUnavailable)?;
        if !fs::metadata(&worker)
            .map_err(|_| RuntimeError::WorkerUnavailable)?
            .is_file()
        {
            return Err(RuntimeError::WorkerUnavailable);
        }

        let bwrap = fs::canonicalize(BWRAP_PATH).map_err(|_| RuntimeError::SandboxUnavailable)?;
        let metadata = fs::metadata(&bwrap).map_err(|_| RuntimeError::SandboxUnavailable)?;
        let mode = metadata.mode();
        if !metadata.is_file() || metadata.uid() != 0 || mode & 0o022 != 0 {
            return Err(RuntimeError::SandboxUnavailable);
        }

        let runtime_files = resolve_worker_runtime_files(&worker)?;

        let sandbox = Self {
            worker,
            bwrap,
            runtime_files,
        };
        sandbox.run_probe(limits)?;
        Ok(sandbox)
    }

    pub(super) fn worker_path(&self) -> &Path {
        &self.worker
    }

    pub(super) fn command(&self, limits: &RuntimeLimits) -> Result<Command, RuntimeError> {
        let mut command = self.base_command();
        command.arg("--membrick-wasm-worker");
        install_process_limits(&mut command, limits)?;
        Ok(command)
    }

    fn base_command(&self) -> Command {
        let mut command = Command::new(&self.bwrap);
        command
            .arg("--die-with-parent")
            .arg("--new-session")
            .arg("--unshare-user")
            .arg("--unshare-pid")
            .arg("--unshare-ipc")
            .arg("--unshare-uts")
            .arg("--unshare-net")
            .arg("--disable-userns")
            .arg("--cap-drop")
            .arg("ALL")
            .arg("--clearenv")
            .arg("--proc")
            .arg("/proc")
            .arg("--dev")
            .arg("/dev");

        let mut parent_dirs = self
            .runtime_files
            .iter()
            .flat_map(|file| file.target.ancestors().skip(1).map(Path::to_owned))
            .collect::<Vec<_>>();
        parent_dirs.sort_by_key(|path| (path.components().count(), path.clone()));
        parent_dirs.dedup();
        for dir in parent_dirs {
            if dir != Path::new("/") {
                command.arg("--dir").arg(dir);
            }
        }
        for file in &self.runtime_files {
            command.arg("--ro-bind").arg(&file.source).arg(&file.target);
        }

        command
            .arg("--size")
            .arg(TEMPFS_BYTES)
            .arg("--tmpfs")
            .arg("/tmp")
            .arg("--setenv")
            .arg("HOME")
            .arg("/nonexistent")
            .arg("--chdir")
            .arg("/tmp")
            .arg("--remount-ro")
            .arg("/")
            .arg("--")
            .arg(&self.worker);

        command
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command
    }

    fn run_probe(&self, limits: &RuntimeLimits) -> Result<(), RuntimeError> {
        let host_secret = create_probe_sentinel()?;

        let mut command = self.base_command();
        command
            .arg("--membrick-sandbox-probe")
            .arg(host_secret.path().as_os_str())
            .arg(&self.worker)
            .arg(
                self.runtime_files
                    .iter()
                    .find(|file| file.target != self.worker)
                    .map(|file| &file.target)
                    .unwrap_or(&self.worker),
            );
        install_process_limits(&mut command, limits)?;
        let output = command
            .output()
            .map_err(|_| RuntimeError::SandboxUnavailable)?;
        if !output.status.success() {
            return Err(RuntimeError::SandboxUnavailable);
        }

        let report: SandboxProbe =
            serde_json::from_slice(&output.stdout).map_err(|_| RuntimeError::SandboxProbeFailed)?;
        if report.verify(limits).is_err() {
            eprintln!("Bubblewrap sandbox probe failed verification: {report:?}");
            return Err(RuntimeError::SandboxProbeFailed);
        }
        Ok(())
    }
}

#[derive(Debug, serde::Deserialize)]
struct SandboxProbe {
    namespaces: Namespaces,
    host_secret_visible: bool,
    host_passwd_visible: bool,
    tmp_writable: bool,
    socket_denied: bool,
    fork_denied: bool,
    exec_denied: bool,
    address_space_growth_denied: bool,
    root_read_only: bool,
    library_read_only: bool,
    extra_fds_closed: bool,
    cpu_soft_seconds: u64,
    cpu_hard_seconds: u64,
    address_space_soft_bytes: u64,
    address_space_hard_bytes: u64,
}

#[derive(Debug, serde::Deserialize)]
struct Namespaces {
    user: String,
    mount: String,
    pid: String,
    ipc: String,
    uts: String,
    net: String,
}

impl SandboxProbe {
    fn verify(self, limits: &RuntimeLimits) -> Result<(), ()> {
        let host = namespace_ids().map_err(|_| ())?;
        let guest = [
            self.namespaces.user,
            self.namespaces.mount,
            self.namespaces.pid,
            self.namespaces.ipc,
            self.namespaces.uts,
            self.namespaces.net,
        ];
        if guest
            .iter()
            .zip(host)
            .any(|(inside, outside)| inside == &outside)
        {
            return Err(());
        }
        if self.host_secret_visible
            || self.host_passwd_visible
            || !self.tmp_writable
            || !self.socket_denied
            || !self.fork_denied
            || !self.exec_denied
            || !self.address_space_growth_denied
            || !self.root_read_only
            || !self.library_read_only
            || !self.extra_fds_closed
        {
            return Err(());
        }

        let cpu = cpu_limits(limits.timeout);
        let address_space = address_space_limit(limits).map_err(|_| ())?;
        if self.cpu_soft_seconds != cpu.0
            || self.cpu_hard_seconds != cpu.1
            || self.address_space_soft_bytes != address_space
            || self.address_space_hard_bytes != address_space
        {
            return Err(());
        }
        Ok(())
    }
}

fn install_process_limits(
    command: &mut Command,
    limits: &RuntimeLimits,
) -> Result<(), RuntimeError> {
    let (cpu_soft, cpu_hard) = cpu_limits(limits.timeout);
    let address_space = address_space_limit(limits)?;
    // SAFETY: only async-signal-safe libc calls are made in the pre-exec hook.
    unsafe {
        command.pre_exec(move || {
            let cpu = libc::rlimit {
                rlim_cur: cpu_soft as libc::rlim_t,
                rlim_max: cpu_hard as libc::rlim_t,
            };
            let memory = libc::rlimit {
                rlim_cur: address_space as libc::rlim_t,
                rlim_max: address_space as libc::rlim_t,
            };
            if libc::setrlimit(libc::RLIMIT_CPU, &cpu) != 0
                || libc::setrlimit(libc::RLIMIT_AS, &memory) != 0
                || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

fn cpu_limits(timeout: Duration) -> (u64, u64) {
    let soft = timeout
        .as_secs()
        .saturating_add(u64::from(timeout.subsec_nanos() != 0))
        .max(1);
    (soft, soft.saturating_add(1))
}

fn address_space_limit(limits: &RuntimeLimits) -> Result<u64, RuntimeError> {
    (limits.memory_bytes as u64)
        .checked_add(ADDRESS_SPACE_OVERHEAD)
        .filter(|limit| *limit <= MAX_ADDRESS_SPACE_BYTES)
        .ok_or(RuntimeError::InvalidLimits)
}

struct ProbeSentinel(PathBuf);

impl ProbeSentinel {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ProbeSentinel {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn create_probe_sentinel() -> Result<ProbeSentinel, RuntimeError> {
    let dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0..128u32 {
        let path = dir.join(format!(
            "membrick-sandbox-probe-{}-{nonce}-{attempt}",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut file) => {
                if file.write_all(b"must not be visible to sandbox").is_err() {
                    let _ = fs::remove_file(&path);
                    return Err(RuntimeError::SandboxProbeFailed);
                }
                return Ok(ProbeSentinel(path));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(RuntimeError::SandboxProbeFailed),
        }
    }
    Err(RuntimeError::SandboxProbeFailed)
}

fn resolve_worker_runtime_files(worker: &Path) -> Result<Vec<RuntimeFile>, RuntimeError> {
    let ldd = Path::new("/usr/bin/ldd");
    if !ldd.is_file() {
        return Err(RuntimeError::SandboxUnavailable);
    }
    let output = Command::new(ldd)
        .env_clear()
        .arg(worker)
        .output()
        .map_err(|_| RuntimeError::SandboxUnavailable)?;
    if !output.status.success() {
        return Err(RuntimeError::SandboxUnavailable);
    }
    let text = String::from_utf8(output.stdout).map_err(|_| RuntimeError::SandboxUnavailable)?;
    let mut files = vec![RuntimeFile {
        source: worker.to_owned(),
        target: worker.to_owned(),
    }];
    for line in text.lines() {
        let candidate = line
            .split_once("=>")
            .map(|(_, rhs)| rhs.split_whitespace().next().unwrap_or_default())
            .filter(|path| path.starts_with('/'))
            .or_else(|| {
                let first = line.split_whitespace().next().unwrap_or_default();
                first.starts_with('/').then_some(first)
            });
        if let Some(candidate) = candidate {
            let source =
                fs::canonicalize(candidate).map_err(|_| RuntimeError::SandboxUnavailable)?;
            if !source.is_file() {
                return Err(RuntimeError::SandboxUnavailable);
            }
            files.push(RuntimeFile {
                source,
                target: PathBuf::from(candidate),
            });
        }
    }
    files.sort_by(|a, b| a.target.cmp(&b.target));
    files.dedup_by(|a, b| a.target == b.target);
    Ok(files)
}

const MAX_ADDRESS_SPACE_BYTES: u64 = 1024 * 1024 * 1024;

fn namespace_ids() -> Result<[String; 6], ()> {
    ["user", "mnt", "pid", "ipc", "uts", "net"]
        .map(|name| fs::read_link(format!("/proc/self/ns/{name}")))
        .map(|path| {
            path.map(|value| value.to_string_lossy().into_owned())
                .map_err(|_| ())
        })
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?
        .try_into()
        .map_err(|_| ())
}

pub(super) fn worker_sandbox_probe(
    secret: &OsStr,
    worker: &OsStr,
    library: &OsStr,
) -> Result<Vec<u8>, RuntimeError> {
    let secret_visible = Path::new(secret).exists();
    let passwd_visible = Path::new("/etc/passwd").exists();
    let temporary_probe = Path::new("/tmp/membrick-sandbox-write-probe");
    let tmp_writable = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temporary_probe)
        .is_ok();
    let _ = fs::remove_file(temporary_probe);
    let root_probe = PathBuf::from(format!(
        "/membrick-sandbox-root-write-probe-{}",
        std::process::id()
    ));
    let root_read_only = readonly_creation_denied(&root_probe);
    let library_read_only = readonly_open_denied(Path::new(library));
    let extra_fds_closed = fs::read_dir("/proc/self/fd")
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|entry| entry.file_name().to_string_lossy().parse::<i32>().ok())
                .all(|fd| fd <= 3)
        })
        .unwrap_or(false);

    // SAFETY: this probe uses only direct syscalls and exits any unexpected
    // forked child immediately; the required seccomp filter should deny it.
    let fork_denied = unsafe {
        let child = libc::fork();
        if child == 0 {
            libc::_exit(0);
        }
        child == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    };
    // SAFETY: the selected absolute path is intentionally absent in the
    // sandbox. Without the execve filter, the result is ENOENT rather than
    // EPERM, so the probe distinguishes filtering from filesystem hiding.
    let exec_denied = unsafe {
        let path = CString::new("/membrick-probe-exec-denied").expect("static path");
        let result = libc::syscall(
            libc::SYS_execve,
            path.as_ptr(),
            std::ptr::null::<*const i8>(),
            std::ptr::null::<*const i8>(),
        );
        result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    };
    // SAFETY: direct socket syscall is denied by the fixed worker filter.
    let socket_denied = unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        if fd >= 0 {
            libc::close(fd);
        }
        fd == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    };

    let mut cpu = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let mut address_space = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: both pointers refer to initialized writable rlimit structs.
    let limits_ok = unsafe {
        libc::getrlimit(libc::RLIMIT_CPU, &mut cpu) == 0
            && libc::getrlimit(libc::RLIMIT_AS, &mut address_space) == 0
    };
    let resources_limited = limits_ok
        && address_space.rlim_cur <= MAX_ADDRESS_SPACE_BYTES as libc::rlim_t
        && address_space.rlim_max <= MAX_ADDRESS_SPACE_BYTES as libc::rlim_t
        && address_space.rlim_cur == address_space.rlim_max
        && cpu.rlim_cur > 0
        && cpu.rlim_cur <= 301;
    // SAFETY: mmap is called with a private anonymous mapping and no file
    // descriptor. A mapping one page larger than RLIMIT_AS must be rejected.
    let address_space_growth_denied = unsafe {
        let page_size = libc::sysconf(libc::_SC_PAGESIZE).max(1) as usize;
        let requested = (address_space.rlim_cur as usize).saturating_add(page_size);
        let mapping = libc::mmap(
            std::ptr::null_mut(),
            requested,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        );
        if mapping == libc::MAP_FAILED {
            std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOMEM)
        } else {
            let _ = libc::munmap(mapping, requested);
            false
        }
    };

    let report = serde_json::json!({
        "namespaces": {
            "user": namespace_link("user")?,
            "mount": namespace_link("mnt")?,
            "pid": namespace_link("pid")?,
            "ipc": namespace_link("ipc")?,
            "uts": namespace_link("uts")?,
            "net": namespace_link("net")?,
        },
        "host_secret_visible": secret_visible,
        "host_passwd_visible": passwd_visible,
        "tmp_writable": tmp_writable,
        "socket_denied": socket_denied,
        "fork_denied": fork_denied,
        "exec_denied": exec_denied,
        "address_space_growth_denied": address_space_growth_denied,
        "root_read_only": root_read_only,
        "library_read_only": library_read_only,
        "extra_fds_closed": extra_fds_closed,
        "cpu_soft_seconds": cpu.rlim_cur,
        "cpu_hard_seconds": cpu.rlim_max,
        "address_space_soft_bytes": address_space.rlim_cur,
        "address_space_hard_bytes": address_space.rlim_max,
        "resource_bounds_present": resources_limited,
        "worker_path": worker.to_string_lossy(),
    });
    if !resources_limited {
        return Err(RuntimeError::SandboxProbeFailed);
    }
    serde_json::to_vec(&report).map_err(|_| RuntimeError::SandboxProbeFailed)
}

fn readonly_creation_denied(path: &Path) -> bool {
    matches!(
        OpenOptions::new().write(true).create_new(true).open(path),
        Err(error) if error.raw_os_error() == Some(libc::EROFS)
    )
}

fn readonly_open_denied(path: &Path) -> bool {
    matches!(
        OpenOptions::new().write(true).open(path),
        Err(error) if error.raw_os_error() == Some(libc::EROFS)
    )
}

fn namespace_link(name: &str) -> Result<String, RuntimeError> {
    fs::read_link(format!("/proc/self/ns/{name}"))
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|_| RuntimeError::SandboxProbeFailed)
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub(super) fn install_worker_seccomp() -> Result<(), RuntimeError> {
    let mut filter = build_seccomp_filter();
    let program = libc::sock_fprog {
        len: filter.len() as libc::c_ushort,
        filter: filter.as_mut_ptr(),
    };
    // SAFETY: BPF instructions are a fixed deny-list with bounds checked
    // above. Installing this filter is intentionally irreversible per worker.
    let installed = unsafe {
        libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0
            && libc::prctl(
                libc::PR_SET_SECCOMP,
                libc::SECCOMP_MODE_FILTER,
                &program as *const libc::sock_fprog,
            ) == 0
    };
    if installed {
        Ok(())
    } else {
        Err(RuntimeError::SandboxUnavailable)
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn build_seccomp_filter() -> Vec<libc::sock_filter> {
    use libc::sock_filter;

    let mut filter = Vec::<sock_filter>::new();
    let arch = if cfg!(target_arch = "x86_64") {
        0xc000_003e // AUDIT_ARCH_X86_64
    } else {
        0xc000_00b7 // AUDIT_ARCH_AARCH64
    };
    filter.push(stmt((libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16, 4));
    filter.push(jump(
        (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
        arch,
        1,
        0,
    ));
    filter.push(stmt(
        (libc::BPF_RET | libc::BPF_K) as u16,
        libc::SECCOMP_RET_KILL_PROCESS,
    ));
    filter.push(stmt((libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16, 0));

    #[cfg(target_arch = "x86_64")]
    {
        // x32 syscall numbers set this bit and would otherwise miss the
        // native syscall-number deny list. Kill those requests before matching.
        filter.push(jump(
            (libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K) as u16,
            0x4000_0000,
            0,
            1,
        ));
        filter.push(stmt(
            (libc::BPF_RET | libc::BPF_K) as u16,
            libc::SECCOMP_RET_KILL_PROCESS,
        ));
    }

    for syscall in denied_syscalls() {
        filter.push(jump(
            (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            syscall as u32,
            0,
            1,
        ));
        filter.push(stmt(
            (libc::BPF_RET | libc::BPF_K) as u16,
            libc::SECCOMP_RET_ERRNO | libc::EPERM as u32,
        ));
    }
    filter.push(stmt(
        (libc::BPF_RET | libc::BPF_K) as u16,
        libc::SECCOMP_RET_ALLOW,
    ));

    filter
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub(super) fn install_worker_seccomp() -> Result<(), RuntimeError> {
    Err(RuntimeError::UnsupportedPlatform)
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn denied_syscalls() -> Vec<libc::c_long> {
    let syscalls = vec![
        libc::SYS_clone,
        libc::SYS_clone3,
        libc::SYS_execve,
        libc::SYS_execveat,
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_connect,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_accept,
        libc::SYS_accept4,
        libc::SYS_sendto,
        libc::SYS_recvfrom,
        libc::SYS_sendmsg,
        libc::SYS_recvmsg,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_ptrace,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_keyctl,
        libc::SYS_userfaultfd,
        libc::SYS_io_uring_setup,
    ];
    #[cfg(target_arch = "x86_64")]
    {
        let mut syscalls = syscalls;
        syscalls.extend([libc::SYS_fork, libc::SYS_vfork]);
        syscalls
    }
    #[cfg(not(target_arch = "x86_64"))]
    syscalls
}

fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod seccomp_tests {
    use super::*;

    fn evaluate(filter: &[libc::sock_filter], arch: u32, syscall: u32) -> u32 {
        let mut accumulator = 0u32;
        let mut pc = 0usize;
        while pc < filter.len() {
            let instruction = filter[pc];
            match instruction.code & 0x07 {
                code if code == libc::BPF_LD as u16 => {
                    accumulator = match instruction.k {
                        0 => syscall,
                        4 => arch,
                        _ => panic!("unexpected seccomp load offset"),
                    };
                    pc += 1;
                }
                code if code == libc::BPF_JMP as u16 => {
                    let operation = instruction.code & 0xf0;
                    let matched = match operation {
                        code if code == libc::BPF_JEQ as u16 => accumulator == instruction.k,
                        code if code == libc::BPF_JSET as u16 => accumulator & instruction.k != 0,
                        _ => panic!("unexpected seccomp jump"),
                    };
                    pc += 1 + if matched {
                        instruction.jt as usize
                    } else {
                        instruction.jf as usize
                    };
                }
                code if code == libc::BPF_RET as u16 => return instruction.k,
                _ => panic!("unexpected seccomp instruction"),
            }
        }
        panic!("seccomp filter fell through")
    }

    #[test]
    fn x32_syscall_numbers_are_killed_before_native_deny_matching() {
        let filter = build_seccomp_filter();
        assert_eq!(
            evaluate(&filter, 0xc000_003e, 0x4000_0000 | libc::SYS_clone as u32),
            libc::SECCOMP_RET_KILL_PROCESS
        );
        assert_eq!(
            evaluate(&filter, 0xc000_003e, libc::SYS_clone as u32),
            libc::SECCOMP_RET_ERRNO | libc::EPERM as u32
        );
    }
}
