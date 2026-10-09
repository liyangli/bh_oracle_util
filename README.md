# bh_oracle_util

Rust 编写的 Oracle 11g 监听监测与日志维护工具，面向 Windows Server，并提供 Linux 部署方式。日志默认每 24 小时检查，监听服务可更频繁检查。路径、服务名、阈值与保留时间均通过 TOML 配置。

## 已实现的处理

- 检查 `listener.log` 大小，默认阈值 **1 GiB**，在达到阈值时轮转。
- 轮转顺序：确认日志原先开启 → 指定监听器 → 暂停日志写入 → 在原目录重命名归档 → 恢复并确认写入开启 → 再检查监听健康。任何暂停或重命名失败都尝试恢复写入；不截断、不直接删除活动日志。
- 默认保留归档 **30 天**，根据归档文件名中的生成时间计算年龄，避免旧日志的修改时间导致刚归档就被删除。每天清理本工具命名的过期归档，即使当前日志没有超限也会清理。
- 同时检查 `lsnrctl services` 响应、配置的 TCP 端口、精确匹配的数据库服务名，以及实例状态 `READY` 或 `UNKNOWN`。只有 `CLRExtProc`、服务缺失或 `BLOCKED` 均不能通过检查。
- 监听控制或 TCP 异常时自动尝试 `lsnrctl start <listener>`。配置 `allow_restart = true` 时，如果启动仍不能恢复控制/端口，可进一步执行一次 stop/start，随后重新验证。每次修复尝试先写入持久冷却时间，默认 30 分钟内不重复修复。
- 支持 Oracle wallet 的可选 SQL 探测，实际通过目标监听器执行 `SELECT ... FROM dual`，避免把端口开放或静态注册误当成数据库可用。
- 命令超时、Oracle 报错检测（即使退出码为 0）、输出大小限制、单实例锁、状态原子替换与中断后的日志写入恢复。
- JSON 行格式审计记录：标准输出及 `state_dir/audit.jsonl`。审计文件达到约 10 MiB 后轮转，保留 3 个历史文件。单次模式失败返回非零退出码；常驻模式记录错误并继续下轮检查。

`listener.ora` 是监听配置，不是日志。程序校验日志文件名，仅允许轮转 `listener.log`，拒绝日志路径中的符号链接。每个监听器必须使用独立的 `state_dir`；同一监听器的所有任务必须共用这个目录，以共享锁和恢复记录。

## 构建与配置

```sh
cargo build --locked --release
cargo test --locked
```

Windows 可执行文件为 `target/release/bh_oracle_util.exe`，Linux 为 `target/release/bh_oracle_util`。GitHub Actions 在 Windows 和 Linux 上执行格式检查、Clippy、测试与 release 构建，并保存可下载的构建产物。Unix 下的模拟 Oracle 流程测试不会在 Windows 上执行；Windows 工作流包含编译和跨平台单元测试。

复制 `config/windows.example.toml` 或 `config/linux.example.toml` 为本地配置。示例中的 Oracle 路径来自常见布局，`SERVER_NAME` 等占位符必须替换；不可把示例当成现场清单。实际路径可从 `lsnrctl status <listener>` 输出中确认，ADR 启用时文本日志通常位于对应 home 的 `trace/listener.log`。

| 配置项 | 作用 | 默认值 |
| --- | --- | --- |
| `oracle_home` / `tns_admin` | 选择真实 Oracle 二进制和监听配置 | 必填绝对路径 |
| `listener` | 要维护的监听器 | 必填 |
| `endpoint` | 同一服务器监听器的具体 IP 和端口，不能用 `0.0.0.0` | 必填 |
| `expected_services` | 实际业务服务名，支持多个精确名称 | 必填且非空 |
| `listener_log` | 实际活动文本日志路径 | 必填绝对路径 |
| `state_dir` | 锁、恢复状态和审计记录 | 必填绝对路径 |
| `max_log_bytes` | 达到此字节数时轮转 | 1073741824 |
| `retention_days` | 仅删除工具生成的过期归档 | 30 |
| `log_check_seconds` | 日志维护间隔，跨任务运行保存 | 86400 |
| `health_check_seconds` | 常驻模式检查完成后的等待时间 | 60 |
| `command_timeout_seconds` | 每条外部命令最长等待 | 30 |
| `repair_cooldown_seconds` | 两次修复尝试的最小间隔 | 1800 |
| `recovery_checks` | 每次启动后的复查次数，间隔 2 秒 | 6 |
| `auto_repair` | 控制/TCP 异常时尝试启动 | true |
| `allow_restart` | 启动无效时允许一次 stop/start | 默认 false，部署示例为 true |
| `sqlplus_wallet_alias` | 可选钱包网络连接别名 | 默认不启用 |

先在 Oracle 所在服务器，以拥有监听管理权限的账户执行只读巡检：

```powershell
.\bh_oracle_util.exe --config E:\bh_oracle_util\config.toml --dry-run
```

`--dry-run` 不修改 Oracle、日志或恢复状态；它会创建本地状态目录及锁文件。根据输出核对路径、端口、业务服务。启用钱包探测时，钱包连接别名必须经由本配置的监听器连接业务数据库，并仅授予所需查询权限。程序不接收明文数据库密码，不打印外部命令原始输出。

```powershell
# 单次运行：检查健康；到日志维护间隔时执行维护
.\bh_oracle_util.exe --config E:\bh_oracle_util\config.toml

# 常驻运行：持续检查，按持久间隔维护日志
.\bh_oracle_util.exe --config E:\bh_oracle_util\config.toml --watch
```

## Windows 自动运行

建议使用 Windows 计划任务，每 5 分钟启动单次巡检；日志维护仍按配置每 24 小时执行。以管理员身份运行安装脚本，输入实际 Oracle 监听管理账户的计划任务凭据：

```powershell
.\deploy\install-task.ps1 -Executable E:\bh_oracle_util\bh_oracle_util.exe -Config E:\bh_oracle_util\config.toml
```

脚本先运行只读检查，再注册任务，不覆盖同名任务。检查计划任务的 `LastTaskResult` 和 `state\audit.jsonl`。第一次日志维护会立即运行，此后按照完成时间间隔执行；这不是固定每天某个时刻。需要更快发现服务异常，可使用 `--watch` 并由服务管理器托管。

停止自动运行：

```powershell
Disable-ScheduledTask -TaskName 'BH Oracle Listener Monitor'
```

不应同时为同一监听器部署多个不同状态目录的任务。现代 Rust 的 Windows 构建需匹配支持的操作系统；老旧 Windows Server 版本需要另外确认兼容性，不能仅根据 Oracle 11g 版本判断可执行文件可用。

## Linux 自动运行

以实际 Oracle 运行账户维护监听。把 release 二进制安装为 `/opt/bh_oracle_util/bh_oracle_util`，配置放在 `/etc/bh_oracle_util/config.toml`。确认 Oracle 账户能读取配置、管理监听、重命名日志并写入状态目录。

```sh
sudo -u oracle /opt/bh_oracle_util/bh_oracle_util --config /etc/bh_oracle_util/config.toml --dry-run
sudo cp deploy/bh-oracle-util.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now bh-oracle-util
journalctl -u bh-oracle-util -f
```

修改 service 中的 `User`、`Group` 与路径以匹配现场。也可以用 cron 每 5 分钟运行单次模式。单次巡检返回失败并不保证本轮没有执行修复，应查看审计中的 `repair_result`、`log_error` 与 `final_health`。

## FAQ 故障的诊断范围

本项目按提供的《Oracle11g监听日志过大导致连接异常全程排查处理FAQ文档》中的日志轮转和服务监测需求实现，并核对 Oracle 11g 官方控制命令。文档里的现场路径、实例名、内存配置和全覆盖配置操作不构成自动执行指令。

- 服务注册缺失（包括可能导致 ORA-12514 的情况）：报告 `missing_services`。若控制响应与 TCP 均正常，不因为数据库服务缺失而不断重启监听；由 DBA 排查数据库 OPEN、服务名、动态/静态注册及连接描述符。
- 静态注册的 `UNKNOWN` 仅表示监听器未通过动态注册获得实例状态，不能证明数据库已 OPEN。启用钱包 SQL 探测后才能增加数据库连接层面的证据；没有钱包时 `sql_ok` 为 null，检查范围限于监听控制、TCP 和注册信息。
- 不自动执行数据库 `startup/shutdown`、内存参数修改、修改防火墙或覆盖 `listener.ora`。若正在崩溃恢复，不应因前端输出延迟反复启动实例。
- 静态注册若配置了 `GLOBAL_DBNAME`，也可以支持匹配的 `SERVICE_NAME` 连接；不能把文档中的斜杠/冒号口诀当成通用连接规则。应以真实 Oracle Net 连接描述符为准。
- 活动 `listener.log` 作为整体归档，保留期作用于归档生成时间，不解析或逐条删除日志记录。当天归档会暂时继续占用磁盘，待过期后删除；需要应急释放空间时先明确审计保留要求。
- ADR 的 XML 日志与数据库 `alert_<SID>.log` 不使用文本监听日志的删除策略；本版本不修改这些文件。ADR 历史数据应使用指定 ADR home 的 ADRCI 策略管理，避免清理其他数据库诊断数据。
- 在线关闭写入或重命名失败时保留活动文件并记录失败；控制/TCP 同时异常时按恢复配置尝试启动或 stop/start。停止命令无法正常完成时不会强杀 Oracle 进程，避免把超时当作服务确实停止。
- pending 恢复记录必须保留，下次运行会尝试恢复日志写入。状态损坏或与监听配置不匹配时拒绝自动重置，防止丢失恢复记录或绕过冷却时间。

程序必须部署在 Oracle 主机本地。`endpoint` 用于探测此监听器；局域网客户端、防火墙外部连通性仍需从客户端验证。当前开发环境没有现场 Oracle，因此模拟测试不等于现场修复验证。

## 验证与参考

测试覆盖精确服务匹配、READY/UNKNOWN/BLOCKED、Oracle 退出码 0 时的报错、归档保留时间、符号链接防护，以及模拟监听器的轮转、过期清理、只读模式、异常恢复写入、监听启动与超时。

- [Oracle 11g Listener Control Utility](https://docs.oracle.com/cd/E11882_01/network.112/e10835/lsnrctl.htm)：指定监听器、SET/SHOW LOG_STATUS、SERVICES、START/STOP。
- [Oracle 11g ADRCI](https://docs.oracle.com/cd/E11882_01/server.112/e22490/adrci.htm)：ADR home 和按年龄清理诊断数据。
- [Oracle 11g Net Services Administrator’s Guide](https://docs.oracle.com/cd/E11882_01/network.112/e41945.pdf)：静态注册的 GLOBAL_DBNAME 与客户端 SERVICE_NAME 的匹配关系。

源 FAQ 原件未提交到仓库。
