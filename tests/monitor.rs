#![cfg(unix)]
use std::{
    fs,
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

struct Fixture {
    _temp: tempfile::TempDir,
    _tcp: TcpListener,
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir(root.join("admin")).unwrap();
        fs::create_dir(root.join("logs")).unwrap();
        fs::write(root.join("admin/listener.ora"), "# preserve me\n").unwrap();
        fs::write(root.join("logs/listener.log"), "some old listener data\n").unwrap();
        let script = r##"#!/bin/sh
case "$1" in
services)
  if [ -f "$ORACLE_HOME/down" ]; then echo 'TNS-12541: no listener'; exit 0; fi
  echo 'Service "orcl" has 1 instance(s).'
  echo '  Instance "orcl", status READY, has 1 handler(s) for this service...'
  ;;
start)
  echo start >> "$ORACLE_HOME/calls"
  if [ -f "$ORACLE_HOME/needs-restart" ]; then echo 'TNS-12542: address in use'; exit 0; fi
  rm -f "$ORACLE_HOME/down"
  ;;
stop) echo stop >> "$ORACLE_HOME/calls"; rm -f "$ORACLE_HOME/needs-restart" ;;
*)
  if [ -f "$ORACLE_HOME/down" ]; then echo 'TNS-12541: no listener'; exit 0; fi
  script=$(cat)
  echo "$script" >> "$ORACLE_HOME/calls"
  case "$script" in
  *'set log_status off'*)
    echo OFF > "$ORACLE_HOME/log-status"
    if [ -f "$ORACLE_HOME/fail-off" ]; then echo 'TNS-12560: error'; exit 0; fi
    ;;
  *'set log_status on'*) echo ON > "$ORACLE_HOME/log-status" ;;
  esac
  status=ON
  if [ -f "$ORACLE_HOME/log-status" ]; then status=$(cat "$ORACLE_HOME/log-status"); fi
  echo "LISTENER parameter \"log_status\" set to $status"
  ;;
esac
"##;
        let tool = root.join("bin/lsnrctl");
        fs::write(&tool, script).unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
        let config = format!("oracle_home = '{r}'\ntns_admin = '{r}/admin'\nlistener = 'LISTENER'\nendpoint = '{}'\nexpected_services = ['orcl']\nlistener_log = '{r}/logs/listener.log'\nstate_dir = '{r}/state'\nmax_log_bytes = 10\nretention_days = 1\nrecovery_checks = 1\ncommand_timeout_seconds = 1\n", tcp.local_addr().unwrap(), r=root.display());
        fs::write(root.join("config.toml"), config).unwrap();
        Self {
            _temp: temp,
            _tcp: tcp,
            root,
        }
    }
    fn run(&self, dry: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bh_oracle_util"));
        command.arg("--config").arg(self.root.join("config.toml"));
        if dry {
            command.arg("--dry-run");
        }
        command.output().unwrap()
    }
    fn archives(&self) -> Vec<PathBuf> {
        fs::read_dir(self.root.join("logs"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|s| s == "bak"))
            .collect()
    }
}
#[test]
fn rotate_preserve_config_cleanup_only_expired_owned_archives() {
    let f = Fixture::new();
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let old = f.root.join(format!(
        "logs/listener.log.bh-oracle-{}-1.bak",
        epoch - 172800
    ));
    let recent = f
        .root
        .join(format!("logs/listener.log.bh-oracle-{epoch}-2.bak"));
    fs::write(&old, "expired").unwrap();
    fs::write(&recent, "recent").unwrap();
    fs::write(
        f.root.join("logs/listener.log.manual.bak"),
        "operator backup",
    )
    .unwrap();
    let out = f.run(false);
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!old.exists());
    assert!(recent.exists());
    assert_eq!(f.archives().len(), 3);
    assert_eq!(
        fs::read_to_string(f.root.join("admin/listener.ora")).unwrap(),
        "# preserve me\n"
    );
    assert_eq!(
        fs::read_to_string(f.root.join("log-status"))
            .unwrap()
            .trim(),
        "ON"
    );
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(f.root.join("state/state.json")).unwrap()).unwrap();
    assert_eq!(state["logging_restore_pending"], false);
    // A second scheduled execution checks health but skips the daily log check.
    assert!(f.run(false).status.success());
    assert_eq!(f.archives().len(), 3);
}
#[test]
fn dry_run_does_not_rotate_or_repair() {
    let f = Fixture::new();
    fs::write(f.root.join("down"), "").unwrap();
    let out = f.run(true);
    assert!(!out.status.success());
    assert!(f.root.join("down").exists());
    assert!(f.root.join("logs/listener.log").exists());
    assert!(f.archives().is_empty());
    assert!(!f.root.join("log-status").exists());
}
#[test]
fn oracle_zero_exit_failure_restores_logging_and_preserves_log() {
    let f = Fixture::new();
    fs::write(f.root.join("fail-off"), "").unwrap();
    assert!(!f.run(false).status.success());
    assert!(f.root.join("logs/listener.log").exists());
    assert!(f.archives().is_empty());
    assert_eq!(
        fs::read_to_string(f.root.join("log-status"))
            .unwrap()
            .trim(),
        "ON"
    );
}
#[test]
fn down_listener_started_and_verified() {
    let f = Fixture::new();
    fs::write(f.root.join("down"), "").unwrap();
    // Listener recovery retries failed maintenance in the same execution.
    assert!(f.run(false).status.success());
    assert!(!f.root.join("down").exists());
    assert!(fs::read_to_string(f.root.join("calls"))
        .unwrap()
        .contains("start"));
    assert!(f.run(false).status.success());
}
#[test]
fn timeout_does_not_leave_monitor_hanging() {
    let f = Fixture::new();
    fs::write(f.root.join("bin/lsnrctl"), "#!/bin/sh\nexec sleep 30\n").unwrap();
    let start = std::time::Instant::now();
    assert!(!f.run(true).status.success());
    assert!(start.elapsed().as_secs() < 5);
}

#[test]
fn interrupted_rotation_restores_logging_before_next_check() {
    let f = Fixture::new();
    // Generate a valid listener-bound state before simulating an interruption.
    fs::write(f.root.join("logs/listener.log"), "tiny").unwrap();
    assert!(f.run(false).status.success());
    let path = f.root.join("state/state.json");
    let mut state: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    state["logging_restore_pending"] = true.into();
    fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    fs::write(f.root.join("log-status"), "OFF\n").unwrap();
    fs::write(f.root.join("down"), "").unwrap();
    let out = f.run(false);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(f.root.join("log-status"))
            .unwrap()
            .trim(),
        "ON"
    );
    let restored: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(restored["logging_restore_pending"], false);
}

#[test]
fn repair_cooldown_survives_separate_processes() {
    let f = Fixture::new();
    fs::write(f.root.join("logs/listener.log"), "tiny").unwrap();
    fs::write(f.root.join("down"), "").unwrap();
    assert!(f.run(false).status.success());
    fs::write(f.root.join("down"), "").unwrap();
    assert!(!f.run(false).status.success());
    assert!(f.root.join("down").exists());
    assert_eq!(
        fs::read_to_string(f.root.join("calls"))
            .unwrap()
            .lines()
            .filter(|l| *l == "start")
            .count(),
        1
    );
}

#[test]
fn concurrent_monitor_refused_by_lock() {
    use fs2::FileExt;
    let f = Fixture::new();
    fs::create_dir(f.root.join("state")).unwrap();
    let lock = fs::File::create(f.root.join("state/monitor.lock")).unwrap();
    lock.try_lock_exclusive().unwrap();
    let out = f.run(false);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("another monitor"));
    assert!(f.archives().is_empty());
}

#[test]
fn bounded_restart_after_start_fails() {
    let f = Fixture::new();
    fs::write(f.root.join("down"), "").unwrap();
    fs::write(f.root.join("needs-restart"), "").unwrap();
    let path = f.root.join("config.toml");
    let mut config = fs::read_to_string(&path).unwrap();
    config.push_str("allow_restart = true\n");
    fs::write(path, config).unwrap();
    let out = f.run(false);
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let calls = fs::read_to_string(f.root.join("calls")).unwrap();
    assert_eq!(calls.lines().filter(|l| *l == "start").count(), 2);
    assert_eq!(calls.lines().filter(|l| *l == "stop").count(), 1);
    assert!(!f.root.join("down").exists());
}

#[test]
fn refuses_configuration_file_as_log() {
    let f = Fixture::new();
    let path = f.root.join("config.toml");
    let config = fs::read_to_string(&path)
        .unwrap()
        .replace("/logs/listener.log'", "/admin/listener.ora'");
    fs::write(path, config).unwrap();
    let out = f.run(false);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("listener.ora is configuration"));
    assert!(!f.root.join("calls").exists());
}
