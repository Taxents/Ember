param([string]$Name = "music-cli", [int]$Seconds = 60, [int]$Every = 5)
# Samples a running process once per second: private working set (what Task Manager shows as
# "Memory") and CPU as a percentage of the whole machine. Prints an average every $Every s.
$ErrorActionPreference = "Stop"
$p = $null
for ($i = 0; $i -lt 50 -and -not $p; $i++) {
    $p = Get-Process -Name $Name -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $p) { Start-Sleep -Milliseconds 200 }
}
if (-not $p) { throw "process $Name not found" }
$cores = [Environment]::ProcessorCount
$t0 = Get-Date
$prevCpu = $p.TotalProcessorTime.TotalMilliseconds
$prevT = Get-Date
$mem = @(); $cpu = @()
for ($i = 1; $i -le $Seconds; $i++) {
    Start-Sleep -Seconds 1
    if ($p.HasExited) { "exited"; break }
    $p.Refresh()
    $now = Get-Date
    $c = $p.TotalProcessorTime.TotalMilliseconds
    $cpu += 100.0 * ($c - $prevCpu) / (($now - $prevT).TotalMilliseconds * $cores)
    $prevCpu = $c; $prevT = $now
    $mem += (Get-CimInstance Win32_PerfFormattedData_PerfProc_Process -Filter "IDProcess=$($p.Id)").WorkingSetPrivate / 1MB
    if ($i % $Every -eq 0) {
        $m = ($mem | Measure-Object -Average -Maximum); $u = ($cpu | Measure-Object -Average)
        "{0,4}s  mem {1,5:N1} MB (max {2,5:N1})  cpu {3,5:N2}% of machine ({4,5:N2}% of one core)" -f `
            [math]::Round(($now - $t0).TotalSeconds), $m.Average, $m.Maximum, $u.Average, ($u.Average * $cores)
        $mem = @(); $cpu = @()
    }
}
