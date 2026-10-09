# Run under the actual Oracle owner / local listener administrator.
# S4U is intentionally avoided: Oracle wallet/network access may require credentials.
[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$Executable,
    [Parameter(Mandatory=$true)][string]$Config,
    [string]$TaskName = 'BH Oracle Listener Monitor'
)
$ErrorActionPreference = 'Stop'
$Executable = (Resolve-Path -LiteralPath $Executable).Path
$Config = (Resolve-Path -LiteralPath $Config).Path
if ($Executable.Contains('"') -or $Config.Contains('"')) { throw 'Invalid path quote' }
& $Executable --config $Config --dry-run
if ($LASTEXITCODE -ne 0) { throw 'Dry run failed. Resolve reported issues before installing.' }
$Credential = Get-Credential -Message 'Oracle listener administration account for scheduled monitoring'
$Action = New-ScheduledTaskAction -Execute $Executable -Argument ('--config "{0}"' -f $Config) -WorkingDirectory (Split-Path $Executable)
$Trigger = New-ScheduledTaskTrigger -Once -At (Get-Date).AddMinutes(1) -RepetitionInterval (New-TimeSpan -Minutes 5)
$Settings = New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew -StartWhenAvailable -ExecutionTimeLimit (New-TimeSpan -Minutes 20)
# Do not silently replace a task belonging to an existing installation.
Register-ScheduledTask -TaskName $TaskName -Action $Action -Trigger $Trigger -Settings $Settings -User $Credential.UserName -Password $Credential.GetNetworkCredential().Password -RunLevel Highest
Write-Host "Installed $TaskName. Health checks every 5 minutes; log checks follow config.log_check_seconds."
Write-Host 'Review LastTaskResult in Task Scheduler. Run manually for JSON diagnostic output.'
