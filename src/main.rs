use anyhow::{bail, Context, Result};
use clap::Parser;
use fs2::FileExt;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Parser)]
#[command(version, about = "Oracle listener monitoring and recovery")]
struct Args {
    #[arg(long)]
    config: PathBuf,
    /// Run checks continuously; otherwise execute once (for Task Scheduler/cron).
    #[arg(long)]
    watch: bool,
    /// Read-only health and log inspection. No repair, rotation or cleanup.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    oracle_home: PathBuf,
    tns_admin: PathBuf,
    listener: String,
    endpoint: SocketAddr,
    expected_services: Vec<String>,
    listener_log: PathBuf,
    state_dir: PathBuf,
    #[serde(default = "default_size")]
    max_log_bytes: u64,
    #[serde(default = "default_retention")]
    retention_days: u64,
    #[serde(default = "default_daily")]
    log_check_seconds: u64,
    #[serde(default = "default_health")]
    health_check_seconds: u64,
    #[serde(default = "default_timeout")]
    command_timeout_seconds: u64,
    #[serde(default = "default_cooldown")]
    repair_cooldown_seconds: u64,
    #[serde(default = "default_attempts")]
    recovery_checks: u32,
    #[serde(default = "yes")]
    auto_repair: bool,
    #[serde(default)]
    allow_restart: bool,
    /// Optional Oracle external-password-store alias, for an actual remote SQL probe.
    #[serde(default)]
    sqlplus_wallet_alias: Option<String>,
}
fn default_size() -> u64 {
    1024 * 1024 * 1024
}
fn default_retention() -> u64 {
    30
}
fn default_daily() -> u64 {
    86400
}
fn default_health() -> u64 {
    60
}
fn default_timeout() -> u64 {
    30
}
fn default_cooldown() -> u64 {
    1800
}
fn default_attempts() -> u32 {
    6
}
fn yes() -> bool {
    true
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    last_log_check: Option<u64>,
    last_repair: Option<u64>,
    logging_restore_pending: bool,
    listener_identity: String,
}
#[derive(Debug, Serialize)]
struct Health {
    control_ok: bool,
    tcp_ok: bool,
    missing_services: Vec<String>,
    sql_ok: Option<bool>,
}
impl Health {
    fn healthy(&self) -> bool {
        self.control_ok
            && self.tcp_ok
            && self.missing_services.is_empty()
            && self.sql_ok != Some(false)
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
static AUDIT: OnceLock<PathBuf> = OnceLock::new();
fn event(kind: &str, detail: impl Serialize) {
    let line = serde_json::json!({"time_unix":now(), "event":kind, "detail":detail}).to_string();
    println!("{line}");
    if let Some(path) = AUDIT.get() {
        let written = (|| -> Result<()> {
            if fs::metadata(path).is_ok_and(|m| m.len() >= 10 * 1024 * 1024) {
                let oldest = path.with_extension("jsonl.3");
                if oldest.exists() {
                    fs::remove_file(oldest)?;
                }
                for n in (1..=2).rev() {
                    let old = path.with_extension(format!("jsonl.{n}"));
                    if old.exists() {
                        fs::rename(old, path.with_extension(format!("jsonl.{}", n + 1)))?;
                    }
                }
                fs::rename(path, path.with_extension("jsonl.1"))?;
            }
            writeln!(
                OpenOptions::new().create(true).append(true).open(path)?,
                "{line}"
            )?;
            Ok(())
        })();
        if let Err(e) = written {
            eprintln!("audit write failed: {e:#}");
        }
    }
}
fn safe_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}
impl Config {
    fn load(path: &Path) -> Result<Self> {
        let c: Self = toml::from_str(&fs::read_to_string(path).context("read config")?)?;
        c.validate()?;
        Ok(c)
    }
    fn validate(&self) -> Result<()> {
        for p in [
            &self.oracle_home,
            &self.tns_admin,
            &self.listener_log,
            &self.state_dir,
        ] {
            if !p.is_absolute() {
                bail!("all paths must be absolute: {}", p.display());
            }
        }
        if !safe_name(&self.listener)
            || self.expected_services.is_empty()
            || self.expected_services.iter().any(|s| !safe_name(s))
        {
            bail!("listener and expected_services must contain only letters, digits, _, . or -");
        }
        if !self
            .listener_log
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("listener.log"))
        {
            bail!("only listener.log may be rotated; listener.ora is configuration");
        }
        if self.max_log_bytes == 0
            || !(1..=3650).contains(&self.retention_days)
            || self.log_check_seconds == 0
            || self.health_check_seconds == 0
            || !(1..=300).contains(&self.command_timeout_seconds)
            || self.repair_cooldown_seconds < 60
            || !(1..=60).contains(&self.recovery_checks)
        {
            bail!("invalid threshold, interval, retention, timeout or recovery bounds");
        }
        if self.endpoint.ip().is_unspecified() || self.endpoint.port() == 0 {
            bail!("endpoint must be a concrete local-server IP and nonzero port");
        }
        if self
            .sqlplus_wallet_alias
            .as_ref()
            .is_some_and(|s| !safe_name(s))
        {
            bail!("invalid wallet alias");
        }
        if !self.tns_admin.join("listener.ora").is_file() {
            bail!("listener.ora not found in tns_admin");
        }
        if !self.tool("lsnrctl").is_file() {
            bail!("lsnrctl not found in oracle_home/bin");
        }
        if self.sqlplus_wallet_alias.is_some() && !self.tool("sqlplus").is_file() {
            bail!("sqlplus not found");
        }
        Ok(())
    }
    fn tool(&self, name: &str) -> PathBuf {
        self.oracle_home.join("bin").join(if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.into()
        })
    }
    fn identity(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.oracle_home.display(),
            self.tns_admin.display(),
            self.listener,
            self.listener_log.display()
        )
    }
}

// Output goes to anonymous temporary files rather than pipes: large output cannot
// deadlock a child. Commands and arguments never pass through a shell.
fn run(c: &Config, tool: &str, args: &[&str], input: Option<&str>) -> Result<String> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let out_path = c
        .state_dir
        .join(format!("command-{}-{nonce}.tmp", std::process::id()));
    let out = OpenOptions::new()
        .write(true)
        .read(true)
        .create_new(true)
        .open(&out_path)?;
    let result = (|| -> Result<String> {
        let mut command = Command::new(c.tool(tool));
        let mut search_path = vec![c.oracle_home.join("bin")];
        search_path.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command.env("PATH", std::env::join_paths(search_path)?);
        #[cfg(target_os = "linux")]
        {
            let mut libraries = vec![c.oracle_home.join("lib")];
            libraries.extend(std::env::split_paths(
                &std::env::var_os("LD_LIBRARY_PATH").unwrap_or_default(),
            ));
            command.env("LD_LIBRARY_PATH", std::env::join_paths(libraries)?);
        }
        command
            .args(args)
            .env("ORACLE_HOME", &c.oracle_home)
            .env("TNS_ADMIN", &c.tns_admin)
            .env("NLS_LANG", "AMERICAN_AMERICA.AL32UTF8")
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(out.try_clone()?)
            .stderr(out.try_clone()?);
        let mut child = command.spawn().with_context(|| format!("spawn {tool}"))?;
        if let Some(text) = input {
            if let Err(e) = child
                .stdin
                .take()
                .context("child stdin")?
                .write_all(text.as_bytes())
            {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e.into());
            }
        }
        let deadline = Instant::now() + Duration::from_secs(c.command_timeout_seconds);
        let status = loop {
            if out.metadata()?.len() > 1024 * 1024 {
                let _ = child.kill();
                let _ = child.wait();
                bail!("{tool} output exceeded 1 MiB");
            }
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("{tool} timed out");
            }
            thread::sleep(Duration::from_millis(50));
        };
        if out.metadata()?.len() > 1024 * 1024 {
            bail!("{tool} output exceeded 1 MiB");
        }
        let output = String::from_utf8_lossy(&fs::read(&out_path)?).into_owned();
        if !status.success() || oracle_error(&output) {
            bail!(
                "{tool} failed (exit {:?}; Oracle error={})",
                status.code(),
                oracle_error(&output)
            );
        }
        Ok(output)
    })();
    drop(out);
    let _ = fs::remove_file(out_path);
    result
}
fn oracle_error(output: &str) -> bool {
    Regex::new(r"(?i)\b(?:TNS|ORA|NL|DIA|LRM|SP2)-\d+")
        .unwrap()
        .is_match(output)
}
fn ctl(c: &Config, op: &str) -> Result<String> {
    run(c, "lsnrctl", &[op, &c.listener], None)
}
fn logging(c: &Config, value: Option<bool>) -> Result<bool> {
    let operation = match value {
        Some(true) => "set log_status on",
        Some(false) => "set log_status off",
        None => "show log_status",
    };
    let script = format!(
        "set current_listener {}\n{operation}\nshow log_status\nexit\n",
        c.listener
    );
    let text = run(c, "lsnrctl", &[], Some(&script))?;
    // Oracle's English output: parameter "log_status" set to ON/OFF.
    let re = Regex::new(r#"(?i)"log_status"\s+(?:set to|is)\s+(ON|OFF)\b"#)?;
    let actual = re
        .captures_iter(&text)
        .last()
        .context("cannot confirm log_status")?[1]
        .eq_ignore_ascii_case("ON");
    if value.is_some_and(|v| v != actual) {
        bail!("log_status did not change to requested value");
    }
    Ok(actual)
}
fn missing_services(output: &str, expected: &[String]) -> Vec<String> {
    let header = Regex::new(r#"(?i)Service\s+"([^"]+)"\s+has\s+\d+\s+instance"#).unwrap();
    let usable = Regex::new(r"(?i)status\s+(READY|UNKNOWN)\b").unwrap();
    let mut found = Vec::new();
    let mut service: Option<String> = None;
    for line in output.lines() {
        if let Some(cap) = header.captures(line) {
            service = Some(cap[1].to_ascii_lowercase());
        } else if usable.is_match(line) {
            if let Some(s) = &service {
                found.push(s.clone());
            }
        }
    }
    expected
        .iter()
        .filter(|s| !found.contains(&s.to_ascii_lowercase()))
        .cloned()
        .collect()
}
fn check(c: &Config) -> Health {
    let response = ctl(c, "services");
    if let Err(e) = &response {
        event("control_error", e.to_string());
    }
    let missing = response
        .as_ref()
        .map(|s| missing_services(s, &c.expected_services))
        .unwrap_or_else(|_| c.expected_services.clone());
    let tcp_ok = TcpStream::connect_timeout(
        &c.endpoint,
        Duration::from_secs(c.command_timeout_seconds.min(5)),
    )
    .is_ok();
    let sql_ok = c.sqlplus_wallet_alias.as_ref().map(|alias| {
        let connect = format!("/@{alias}");
        let probe = "whenever oserror exit failure\nwhenever sqlerror exit failure\nset heading off feedback off echo off\nselect 'BH_ORACLE_HEALTH_' || 'OK' from dual;\nexit success\n";
        run(c, "sqlplus", &["-L", "-S", &connect], Some(probe)).is_ok_and(|s| s.lines().any(|l| l.trim() == "BH_ORACLE_HEALTH_OK"))
    });
    Health {
        control_ok: response.is_ok(),
        tcp_ok,
        missing_services: missing,
        sql_ok,
    }
}
fn save(c: &Config, state: &State) -> Result<()> {
    // Keep the locked inode intact; state is a separate atomically replaced file.
    let tmp = c.state_dir.join("state.tmp");
    let mut f = File::create(&tmp)?;
    f.write_all(&serde_json::to_vec_pretty(state)?)?;
    f.sync_all()?;
    drop(f);
    // Windows std::fs::rename does not replace existing destinations. Use the
    // platform MoveFileExW replacement API to avoid a delete-before-write gap.
    replace_state(&tmp, &c.state_dir.join("state.json"))
}
#[cfg(not(windows))]
fn replace_state(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to)?;
    Ok(())
}
#[cfg(windows)]
fn replace_state(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }
    let a: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let b: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { MoveFileExW(a.as_ptr(), b.as_ptr(), 1 | 8) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
fn due(previous: Option<u64>, interval: u64, time: u64) -> bool {
    previous.is_none_or(|p| time >= p && time - p >= interval)
}
fn recover(c: &Config, state: &mut State, h: Health, dry: bool) -> Result<Health> {
    if h.healthy() || dry || !c.auto_repair {
        return Ok(h);
    }
    if !due(state.last_repair, c.repair_cooldown_seconds, now()) {
        event("repair_cooldown", "repair skipped");
        return Ok(h);
    }
    // Missing DB registration/SQL failure alone is not a listener crash.
    if h.control_ok && h.tcp_ok {
        event("database_or_registration_failure", &h);
        return Ok(h);
    }
    state.last_repair = Some(now());
    save(c, state)?;
    // Start first. Restart is only allowed explicitly and after start failed
    // to restore health, including the case of a hung existing listener.
    if let Err(e) = ctl(c, "start") {
        event("start_error", e.to_string());
    }
    let mut health = wait_health(c);
    if !health.healthy() && (!health.control_ok || !health.tcp_ok) && c.allow_restart {
        event("restart", "bounded listener stop/start");
        ctl(c, "stop").context("stop failed; refusing to claim restart success")?;
        ctl(c, "start")?;
        health = wait_health(c);
    }
    event("repair_result", &health);
    Ok(health)
}
fn wait_health(c: &Config) -> Health {
    let mut h = check(c);
    for _ in 1..c.recovery_checks {
        if h.healthy() {
            break;
        }
        thread::sleep(Duration::from_secs(2));
        h = check(c);
    }
    h
}
fn regular_file(path: &Path) -> Result<bool> {
    // Reject symlinks in all existing components, including the log directory.
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(m) if m.file_type().is_symlink() => {
                bail!("symlink path refused: {}", ancestor.display())
            }
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    match fs::symlink_metadata(path) {
        Ok(m) => Ok(m.is_file()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
fn archive_timestamp(name: &str) -> Option<u64> {
    let suffix = name
        .strip_prefix("listener.log.bh-oracle-")?
        .strip_suffix(".bak")?;
    let (seconds, nanos) = suffix.split_once('-')?;
    if !nanos.bytes().all(|b| b.is_ascii_digit()) || nanos.is_empty() {
        return None;
    }
    seconds.parse().ok()
}
fn cleanup(c: &Config, dry: bool) -> Result<()> {
    let dir = c.listener_log.parent().context("log parent")?;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if let Some(timestamp) = archive_timestamp(&entry.file_name().to_string_lossy()) {
            if due(Some(timestamp), c.retention_days * 86400, now()) && regular_file(&entry.path())?
            {
                event(
                    if dry {
                        "would_delete_archive"
                    } else {
                        "delete_archive"
                    },
                    entry.path().display().to_string(),
                );
                if !dry {
                    fs::remove_file(entry.path())?;
                }
            }
        }
    }
    Ok(())
}
fn logs(c: &Config, state: &mut State, dry: bool) -> Result<()> {
    // Retention runs even below the size threshold to prevent stale archives.
    cleanup(c, dry)?;
    if !regular_file(&c.listener_log)? {
        bail!("listener.log missing or not a regular file");
    }
    let size = fs::metadata(&c.listener_log)?.len();
    event(
        "log_size",
        serde_json::json!({"bytes":size,"threshold":c.max_log_bytes}),
    );
    if size >= c.max_log_bytes {
        if dry {
            event("would_rotate", c.listener_log.display().to_string());
            return Ok(());
        }
        if !logging(c, None)? {
            bail!("logging already OFF; preserving operator setting, rotation skipped");
        }
        // Persist intent before switching OFF so the next run can restore ON
        // after interruption, including an off command that times out.
        state.logging_restore_pending = true;
        save(c, state)?;
        let rotation = (|| -> Result<()> {
            logging(c, Some(false))?;
            if !regular_file(&c.listener_log)? {
                bail!("log changed before rotation");
            }
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.subsec_nanos();
            let dest = c
                .listener_log
                .with_file_name(format!("listener.log.bh-oracle-{}-{nanos}.bak", now()));
            if dest.exists() {
                bail!("archive collision");
            }
            fs::rename(&c.listener_log, &dest)
                .context("archive log; listener may still hold file lock")?;
            event("rotated", dest.display().to_string());
            Ok(())
        })();
        let restored = logging(c, Some(true));
        if restored.is_ok() {
            state.logging_restore_pending = false;
            save(c, state)?;
        }
        if let Err(e) = rotation {
            event("rotation_error", e.to_string());
            restored.context("also failed to restore logging; next run retries")?;
            return Err(e);
        }
        restored.context("failed to restore logging; next run retries")?;
        // Verify control health after a successful rotation; log is regenerated
        // by Oracle on subsequent writes, so never create/truncate it ourselves.
        let h = check(c);
        event("post_rotation_health", &h);
        if !h.healthy() {
            bail!("listener unhealthy after rotation");
        }
    }
    if !dry {
        state.last_log_check = Some(now());
        save(c, state)?;
    }
    Ok(())
}
fn cycle(c: &Config, state: &mut State, dry: bool) -> Result<()> {
    if state.logging_restore_pending {
        if dry {
            bail!("pending logging restoration; run without --dry-run to recover");
        }
        let h = check(c);
        if !h.control_ok || !h.tcp_ok {
            recover(c, state, h, false)?;
        }
        logging(c, Some(true)).context("recover interrupted rotation")?;
        state.logging_restore_pending = false;
        save(c, state)?;
        event("logging_restored", "recovered interrupted rotation");
    }
    let initial = check(c);
    let initial_control_failure = !initial.control_ok || !initial.tcp_ok;
    event("health", &initial);
    // Logs are independent of health failure and can be the root cause.
    let mut log_result = if dry || due(state.last_log_check, c.log_check_seconds, now()) {
        logs(c, state, dry)
    } else {
        Ok(())
    };
    if let Err(e) = &log_result {
        event("log_error", e.to_string());
    }
    let health = if initial.healthy() { initial } else { check(c) };
    let final_health = recover(c, state, health, dry)?;
    event("final_health", &final_health);
    if initial_control_failure && final_health.healthy() && log_result.is_err() && !dry {
        event(
            "retry_log_check",
            "listener recovered; retry interrupted log maintenance once",
        );
        log_result = logs(c, state, false);
    }
    log_result?;
    if !final_health.healthy() {
        bail!("listener health checks failed");
    }
    Ok(())
}
fn main() -> Result<()> {
    let args = Args::parse();
    let c = Config::load(&args.config)?;
    fs::create_dir_all(&c.state_dir)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(c.state_dir.join("monitor.lock"))?;
    lock.try_lock_exclusive()
        .context("another monitor holds the state lock")?;
    if !args.dry_run {
        let _ = AUDIT.set(c.state_dir.join("audit.jsonl"));
    }
    let path = c.state_dir.join("state.json");
    let mut state: State = if path.exists() {
        serde_json::from_slice(&fs::read(path)?).context("corrupt state; refusing unsafe reset")?
    } else {
        State::default()
    };
    if !state.listener_identity.is_empty() && state.listener_identity != c.identity() {
        bail!("state belongs to another listener; use a separate state_dir");
    }
    state.listener_identity = c.identity();
    loop {
        let result = cycle(&c, &mut state, args.dry_run);
        if !args.watch {
            return result;
        }
        if let Err(e) = result {
            event("cycle_error", e.to_string());
        }
        thread::sleep(Duration::from_secs(c.health_check_seconds));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn services_require_exact_name_and_usable_instance() {
        let output = "Service \"orcl.evil\" has 1 instance(s).\n Instance \"orcl\", status READY\nService \"orcl\" has 1 instance(s).\n Instance \"orcl\", status BLOCKED\nService \"static\" has 1 instance(s).\n Instance \"static\", status UNKNOWN\n";
        assert_eq!(
            missing_services(output, &["orcl".into(), "static".into()]),
            vec!["orcl"]
        );
    }
    #[test]
    fn oracle_error_even_with_success_exit() {
        assert!(oracle_error("TNS-12541: no listener"));
        assert!(oracle_error("ORA-12514"));
        assert!(!oracle_error("The command completed successfully"));
    }
    #[test]
    fn retention_uses_archive_creation_not_old_log_mtime() {
        assert_eq!(
            archive_timestamp("listener.log.bh-oracle-100-42.bak"),
            Some(100)
        );
        for name in [
            "listener.ora",
            "listener.log",
            "listener.log.bh-oracle-100.bak",
            "listener.log.bh-oracle-100-xx.bak",
        ] {
            assert_eq!(archive_timestamp(name), None);
        }
        assert!(!due(Some(100), 30, 120));
        assert!(due(Some(100), 30, 130));
        assert!(!due(Some(100), 30, 90));
    }
    #[cfg(unix)]
    #[test]
    fn symlink_parent_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("real")).unwrap();
        std::os::unix::fs::symlink(temp.path().join("real"), temp.path().join("link")).unwrap();
        assert!(regular_file(&temp.path().join("link/listener.log")).is_err());
    }
}
