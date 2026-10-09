//! Read-only first-install discovery; generated configuration stays usable offline.
use super::*;
use std::net::{IpAddr, TcpListener, ToSocketAddrs};

#[derive(Default, Deserialize)]
#[serde(default)]
struct Inventory {
    homes: Vec<Home>,
    services: Vec<String>,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct Home {
    home: PathBuf,
    tns_admin: Option<PathBuf>,
}

#[cfg(windows)]
fn inventory() -> Result<Inventory> {
    // Read both registry views and listener service ImagePath. PowerShell emits
    // UTF-8 JSON explicitly, avoiding reg.exe's localized labels and OEM encoding.
    let script = r#"
$ErrorActionPreference='Stop'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)
$homes = @()
foreach ($root in @('HKLM:\SOFTWARE\ORACLE', 'HKLM:\SOFTWARE\WOW6432Node\ORACLE')) {
  if (Test-Path -LiteralPath $root) {
    $keys = @((Get-Item -LiteralPath $root)) + @(Get-ChildItem -LiteralPath $root -ErrorAction SilentlyContinue)
    foreach ($key in $keys) {
      $p = Get-ItemProperty -LiteralPath $key.PSPath
      if ($p.ORACLE_HOME) { $homes += @{home=[string]$p.ORACLE_HOME; tns_admin=$p.TNS_ADMIN} }
    }
  }
}
$services = @(Get-ChildItem 'HKLM:\SYSTEM\CurrentControlSet\Services' | ForEach-Object {
  $p = Get-ItemProperty -LiteralPath $_.PSPath -ErrorAction SilentlyContinue
  if ($p.ImagePath -match '(?i)tnslsnr\.exe') { [Environment]::ExpandEnvironmentVariables([string]$p.ImagePath) }
})
@{homes=@($homes); services=@($services)} | ConvertTo-Json -Depth 5 -Compress
"#;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("inventory.json");
    let output = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)?;
    let system_root = std::env::var_os("SystemRoot").context("SystemRoot not defined")?;
    let executable =
        PathBuf::from(system_root).join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let mut child = Command::new(executable)
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if output.metadata()?.len() > 1024 * 1024 {
            let _ = child.kill();
            let _ = child.wait();
            bail!("registry inventory output too large");
        }
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                bail!("cannot read Oracle registry/service inventory; use --oracle-home and --tns-admin");
            }
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("registry inventory timed out");
        }
        thread::sleep(Duration::from_millis(50));
    }
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).context("parse UTF-8 Oracle registry inventory")
}
#[cfg(not(windows))]
fn inventory() -> Result<Inventory> {
    let mut result = Inventory::default();
    if let Ok(text) = fs::read_to_string("/etc/oratab") {
        for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
            if let Some(home) = line.split(':').nth(1).filter(|s| !s.is_empty()) {
                result.homes.push(Home {
                    home: home.into(),
                    tns_admin: None,
                });
            }
        }
    }
    Ok(result)
}
fn home_from_service(image: &str) -> Option<PathBuf> {
    let re = Regex::new(r#"(?i)^\s*"?(.+?[\\/]bin[\\/]tnslsnr\.exe)(?:"|\s|$)"#).unwrap();
    let executable = re.captures(image)?.get(1)?.as_str();
    // Windows separators must also be parsed in cross-platform parser tests.
    let lower = executable.to_ascii_lowercase();
    let offset = lower.rfind("\\bin\\").or_else(|| lower.rfind("/bin/"))?;
    Some(executable[..offset].into())
}
fn tool_exists(home: &Path) -> bool {
    home.join("bin")
        .join(if cfg!(windows) {
            "lsnrctl.exe"
        } else {
            "lsnrctl"
        })
        .is_file()
}
fn unique_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut result: Vec<PathBuf> = Vec::new();
    for path in paths {
        if !result.iter().any(|p| {
            if cfg!(windows) {
                p.to_string_lossy()
                    .eq_ignore_ascii_case(&path.to_string_lossy())
            } else {
                p == &path
                    || p.canonicalize()
                        .ok()
                        .zip(path.canonicalize().ok())
                        .is_some_and(|(a, b)| a == b)
            }
        }) {
            result.push(path);
        }
    }
    result
}
fn choose_home(hint: Option<PathBuf>, inv: &Inventory) -> Result<PathBuf> {
    if let Some(home) = hint {
        if !home.is_absolute() || !tool_exists(&home) {
            bail!("--oracle-home must be an absolute directory containing bin/lsnrctl");
        }
        return Ok(home);
    }
    // Actual installed listener services take precedence over client-only homes.
    let mut homes: Vec<PathBuf> = inv
        .services
        .iter()
        .filter_map(|s| home_from_service(s))
        .filter(|p| tool_exists(p))
        .collect();
    if homes.is_empty() {
        homes.extend(inv.homes.iter().map(|h| h.home.clone()));
        if let Some(home) = std::env::var_os("ORACLE_HOME") {
            homes.push(home.into());
        }
        for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
            if dir
                .file_name()
                .is_some_and(|s| s.eq_ignore_ascii_case("bin"))
            {
                if let Some(home) = dir.parent().filter(|p| tool_exists(p)) {
                    homes.push(home.into());
                }
            }
        }
        homes.retain(|p| p.is_absolute() && tool_exists(p));
    }
    let homes = unique_paths(homes);
    if homes.len() != 1 {
        bail!(
            "expected one Oracle Home; found {}: {:?}. Select with --oracle-home",
            homes.len(),
            homes
        );
    }
    Ok(homes[0].clone())
}
fn choose_admin(home: &Path, hint: Option<PathBuf>, inv: &Inventory) -> Result<PathBuf> {
    let candidates = if let Some(hint) = hint {
        vec![hint]
    } else if let Some(env) = std::env::var_os("TNS_ADMIN") {
        vec![env.into()]
    } else {
        let mut paths: Vec<PathBuf> = inv
            .homes
            .iter()
            .filter(|h| {
                h.home
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&home.to_string_lossy())
            })
            .filter_map(|h| h.tns_admin.clone())
            .collect();
        if paths.is_empty() {
            paths.extend([home.join("network/admin"), home.join("NETWORK/ADMIN")]);
        }
        paths
    };
    let paths = unique_paths(
        candidates
            .into_iter()
            .filter(|p| p.is_absolute() && p.join("listener.ora").is_file())
            .collect(),
    );
    if paths.len() != 1 {
        bail!(
            "cannot select listener.ora directory: {:?}. Use --tns-admin",
            paths
        );
    }
    Ok(paths[0].clone())
}
// Keep only genuine top-level listener definitions, not SID_LIST/ADR parameters.
// Walk balanced parentheses so nested SERVICE/ADDRESS lines are not candidates.
fn listeners(text: &str) -> Result<Vec<String>> {
    let text = text
        .lines()
        .map(|line| line.split('#').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    let bytes = text.as_bytes();
    let mut start = 0;
    let mut depth = 0_i32;
    let mut quoted = false;
    let mut names = Vec::new();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'"' {
            quoted = !quoted;
        }
        if quoted {
            continue;
        }
        if b == b'(' {
            depth += 1;
        }
        if b == b')' {
            depth -= 1;
            if depth < 0 {
                bail!("unbalanced listener.ora");
            }
            if depth == 0 {
                let entry = text[start..=i].trim();
                if let Some((key, body)) = entry.split_once('=') {
                    let key = key.trim();
                    if safe_name(key)
                        && body.to_ascii_uppercase().contains("(ADDRESS")
                        && !key.to_ascii_uppercase().starts_with("SID_LIST_")
                    {
                        names.push(key.to_string());
                    }
                }
                start = i + 1;
            }
        }
        // Scalar top-level parameters end at newline; they must not be joined
        // onto the next listener definition.
        if b == b'\n' && depth == 0 {
            if let Some((_, value)) = text[start..i].split_once('=') {
                if !value.trim().is_empty() {
                    start = i + 1;
                }
            }
        }
    }
    if depth != 0 || quoted {
        bail!("unbalanced listener.ora");
    }
    names.sort();
    names.dedup();
    Ok(names)
}
fn choose_listener(text: &str, hint: Option<String>) -> Result<String> {
    if let Some(name) = hint {
        if !safe_name(&name) {
            bail!("invalid --listener name");
        }
        return Ok(name);
    }
    let names = listeners(text)?;
    if names.len() != 1 {
        bail!(
            "expected one listener; found {:?}. Use --listener (also for IFILE configurations)",
            names
        );
    }
    Ok(names[0].clone())
}
fn status_path(output: &str, label: &str) -> Result<PathBuf> {
    let re = Regex::new(&format!(r"(?m)^\s*{}\s+(.+?)\s*$", regex::escape(label)))?;
    let path = PathBuf::from(
        re.captures(output)
            .with_context(|| format!("{label} not reported by listener"))?[1]
            .trim(),
    );
    if !path.is_absolute() {
        bail!("listener reported non-absolute {label}");
    }
    Ok(path)
}
fn text_log(path: &Path) -> Result<PathBuf> {
    if path
        .file_name()
        .is_some_and(|s| s.eq_ignore_ascii_case("listener.log"))
    {
        return Ok(path.into());
    }
    if path
        .file_name()
        .is_some_and(|s| s.eq_ignore_ascii_case("log.xml"))
        && path
            .parent()
            .and_then(|p| p.file_name())
            .is_some_and(|s| s.eq_ignore_ascii_case("alert"))
    {
        return Ok(path
            .parent()
            .and_then(|p| p.parent())
            .context("ADR home")?
            .join("trace/listener.log"));
    }
    bail!(
        "unsupported listener log {}: cannot safely map to listener.log",
        path.display()
    )
}
fn detected_services(output: &str) -> Vec<String> {
    let re = Regex::new(r#"(?i)Service\s+"([^"]+)"\s+has\s+\d+\s+instance"#).unwrap();
    let mut names: Vec<String> = re
        .captures_iter(output)
        .map(|c| c[1].to_string())
        .filter(|s| {
            safe_name(s)
                && !s.eq_ignore_ascii_case("CLRExtProc")
                && !s.eq_ignore_ascii_case("PLSExtProc")
        })
        .collect();
    names.retain(|s| missing_services(output, std::slice::from_ref(s)).is_empty());
    names.sort();
    names.dedup();
    names
}
fn endpoints(output: &str) -> Result<Vec<SocketAddr>> {
    let address = Regex::new(r"(?i)\(ADDRESS\s*=\s*((?:\([^()]*\)\s*)+)\)")?;
    let attr = Regex::new(r"(?i)\(\s*(PROTOCOL|HOST|PORT)\s*=\s*([^()]+)\)")?;
    let mut found = Vec::new();
    for body in address.captures_iter(output) {
        let mut protocol = String::new();
        let mut host = String::new();
        let mut port = String::new();
        for cap in attr.captures_iter(&body[1]) {
            match cap[1].to_ascii_uppercase().as_str() {
                "PROTOCOL" => protocol = cap[2].trim().into(),
                "HOST" => host = cap[2].trim().into(),
                "PORT" => port = cap[2].trim().into(),
                _ => (),
            }
        }
        if !protocol.eq_ignore_ascii_case("TCP") {
            continue;
        }
        let port: u16 = port
            .parse()
            .context("invalid TCP port in listener status")?;
        if port == 0 {
            bail!("listener reported port 0");
        }
        let ips: Vec<IpAddr> = match host.parse::<IpAddr>() {
            Ok(ip) if ip.is_unspecified() => vec![if ip.is_ipv4() {
                "127.0.0.1".parse()?
            } else {
                "::1".parse()?
            }],
            Ok(ip) => vec![ip],
            Err(_) => (host.as_str(), port)
                .to_socket_addrs()
                .context("resolve listener hostname")?
                .map(|a| a.ip())
                .collect(),
        };
        for ip in ips {
            // Ensure discovery only selects a local interface, never a remote
            // listener mentioned in configuration/output. Bind an ephemeral port.
            if TcpListener::bind(SocketAddr::new(ip, 0)).is_ok() {
                let endpoint = SocketAddr::new(ip, port);
                if !found.contains(&endpoint) {
                    found.push(endpoint);
                }
            }
        }
    }
    Ok(found)
}

pub(super) fn initialize(
    path: &Path,
    home: Option<PathBuf>,
    admin: Option<PathBuf>,
    listener: Option<String>,
) -> Result<()> {
    if path.exists() {
        bail!(
            "configuration already exists: {}; choose a new filename (never overwritten)",
            path.display()
        );
    }
    let target = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let inv = match inventory() {
        Ok(inv) => inv,
        Err(e) if home.is_some() => {
            event("discovery_inventory_warning", e.to_string());
            Inventory::default()
        }
        Err(e) => return Err(e),
    };
    let home = choose_home(home, &inv)?;
    let admin = choose_admin(&home, admin, &inv)?;
    let listener = choose_listener(&fs::read_to_string(admin.join("listener.ora"))?, listener)?;
    let temporary = tempfile::tempdir()?;
    let mut config = Config {
        oracle_home: home,
        tns_admin: admin,
        listener,
        endpoint: "127.0.0.1:1521".parse()?,
        expected_services: Vec::new(),
        listener_log: PathBuf::new(),
        state_dir: temporary.path().into(),
        max_log_bytes: default_size(),
        retention_days: default_retention(),
        log_check_seconds: default_daily(),
        health_check_seconds: default_health(),
        command_timeout_seconds: default_timeout(),
        repair_cooldown_seconds: default_cooldown(),
        recovery_checks: default_attempts(),
        auto_repair: true,
        allow_restart: true,
        sqlplus_wallet_alias: None,
    };
    let status = ctl(&config, "status").context("first discovery requires a responding local listener; saved configuration remains usable when it later stops")?;
    let actual_admin = status_path(&status, "Listener Parameter File")?
        .parent()
        .context("parameter file parent")?
        .to_path_buf();
    if actual_admin.canonicalize()? != config.tns_admin.canonicalize()? {
        bail!(
            "listener uses a different configuration directory {}; rerun with --tns-admin",
            actual_admin.display()
        );
    }
    config.listener_log = text_log(&status_path(&status, "Listener Log File")?)?;
    if !regular_file(&config.listener_log)? {
        bail!(
            "discovered text listener.log not found: {}",
            config.listener_log.display()
        );
    }
    let candidates = endpoints(&status)?;
    config.endpoint = candidates
        .iter()
        .find(|e| TcpStream::connect_timeout(e, Duration::from_secs(2)).is_ok())
        .copied()
        .context("no reported local TCP listener endpoint is reachable")?;
    config.expected_services = detected_services(&ctl(&config, "services")?);
    if config.expected_services.is_empty() {
        bail!("no usable business service registered; refusing to generate healthy-looking config from CLRExtProc only");
    }
    config.validate()?;
    let health = check(&config);
    if !health.healthy() {
        bail!("discovered listener did not pass health checks");
    }
    config.state_dir = target
        .parent()
        .context("configuration parent")?
        .join("state")
        .join(&config.listener);
    let contents = format!("# Generated from this server by bh_oracle_util --init-config.\n# Review expected_services if this listener exposes multiple business services.\n{}", toml::to_string_pretty(&config)?);
    // Atomic publication via hard link: refuses collisions, and never leaves a
    // partial config at the requested path if interrupted while writing.
    let parent = target.parent().context("configuration parent")?;
    let mut draft = tempfile::NamedTempFile::new_in(parent)?;
    draft.write_all(contents.as_bytes())?;
    draft.as_file().sync_all()?;
    fs::hard_link(draft.path(), &target).context("publish new config without overwriting")?;
    event(
        "config_generated",
        serde_json::json!({"config":target,"oracle_home":config.oracle_home,"tns_admin":config.tns_admin,"listener":config.listener,"listener_log":config.listener_log,"endpoint":config.endpoint,"expected_services":config.expected_services,"state_dir":config.state_dir}),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn service_command_path_with_spaces_and_quotes() {
        assert_eq!(
            home_from_service(r#""E:\Oracle Home\dbhome_1\bin\tnslsnr.exe" LISTENER -inherit"#),
            Some(PathBuf::from(r"E:\Oracle Home\dbhome_1"))
        );
        assert_eq!(
            home_from_service(r"E:\app\dbhome\bin\tnslsnr.exe LISTENER"),
            Some(PathBuf::from(r"E:\app\dbhome"))
        );
        assert_eq!(home_from_service("not-oracle.exe"), None);
    }
    #[test]
    fn listener_definitions_ignore_parameters_and_nested_services() {
        let text = "ADR_BASE_LISTENER = E:\\app\nSID_LIST_LISTENER = (SID_LIST=(SID_DESC=(SID_NAME=orcl)))\nLISTENER =\n (DESCRIPTION_LIST=(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=0.0.0.0)(PORT=1521))))\nLISTENER2=(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=localhost)(PORT=1522)))\n";
        assert_eq!(listeners(text).unwrap(), vec!["LISTENER", "LISTENER2"]);
        assert!(choose_listener(text, None).is_err());
        assert_eq!(
            choose_listener(text, Some("LISTENER2".into())).unwrap(),
            "LISTENER2"
        );
    }
    #[test]
    fn xml_log_maps_to_matching_home_text_log() {
        assert_eq!(
            text_log(Path::new(
                "/oracle/diag/tnslsnr/host/listener/alert/log.xml"
            ))
            .unwrap(),
            PathBuf::from("/oracle/diag/tnslsnr/host/listener/trace/listener.log")
        );
        assert!(text_log(Path::new("/oracle/listener.ora")).is_err());
    }
    #[test]
    fn business_services_exclude_extproc_and_blocked_instances() {
        let output="Service \"CLRExtProc\" has 1 instance(s).\n Instance \"x\", status UNKNOWN\nService \"orcl\" has 1 instance(s).\n Instance \"x\", status READY\nService \"blocked\" has 1 instance(s).\n Instance \"x\", status BLOCKED";
        assert_eq!(detected_services(output), vec!["orcl"]);
    }
    #[test]
    fn wildcard_endpoint_local_and_remote_refused() {
        let output="(ADDRESS=(PROTOCOL=TCP)(HOST=0.0.0.0)(PORT=1521))\n(ADDRESS=(PROTOCOL=TCP)(HOST=192.0.2.123)(PORT=1521))";
        assert_eq!(
            endpoints(output).unwrap(),
            vec!["127.0.0.1:1521".parse::<SocketAddr>().unwrap()]
        );
    }
}
