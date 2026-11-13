# Measure the memory number closest to Task Manager's Memory column.
# Nova must already be running, and its single-instance guard means exactly
# one process should be present.
[CmdletBinding()]
param(
    [ValidateRange(5, 600)]
    [int]$DurationSeconds = 30,
    [ValidateRange(32, 4096)]
    [int]$LimitMB = 150,
    [ValidateRange(1, 30)]
    [int]$IntervalSeconds = 2
)

$ErrorActionPreference = "Stop"
$processes = @(Get-Process -Name nova -ErrorAction Stop)
if ($processes.Count -ne 1) {
    throw "Expected exactly one Nova process, found $($processes.Count)."
}

$process = $processes[0]
$samples = @()
$started = Get-Date
while (((Get-Date) - $started).TotalSeconds -lt $DurationSeconds) {
    Start-Sleep -Seconds $IntervalSeconds
    $process.Refresh()
    if ($process.HasExited) {
        throw "Nova exited while memory was being measured."
    }

    $privateCounter = (Get-Counter '\Process(nova)\Working Set - Private').CounterSamples |
        Where-Object { $_.InstanceName -eq 'nova' } |
        Select-Object -First 1
    if (-not $privateCounter) {
        throw "Windows did not return Nova's private working-set counter."
    }

    $samples += [pscustomobject]@{
        Seconds             = [math]::Round(((Get-Date) - $started).TotalSeconds, 1)
        PrivateWorkingSetMB = [math]::Round($privateCounter.CookedValue / 1MB, 1)
        PrivateCommitMB     = [math]::Round($process.PrivateMemorySize64 / 1MB, 1)
        TotalWorkingSetMB   = [math]::Round($process.WorkingSet64 / 1MB, 1)
    }
}

$samples | Format-Table -AutoSize
$peak = ($samples | Measure-Object PrivateWorkingSetMB -Maximum).Maximum
Write-Host "Peak private working set: $peak MB (limit: $LimitMB MB)"
if ($peak -gt $LimitMB) {
    Write-Error "Nova exceeded the configured private working-set limit."
    exit 1
}
exit 0
