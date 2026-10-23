# Measures the app against the plan's targets: open idle, open playing, minimized playing,
# minimized paused. Drives it through its own taskbar buttons (a targeted WM_COMMAND), so no
# other app's media session is touched. Needs a saved queue (queue.json) to have something to play.
#   tools\app_measure.ps1 -Exe target\release\music.exe
param([string]$Exe = "target\release\music.exe", [int]$Phase = 20)
$ErrorActionPreference = "Stop"
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class W {
    [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint m, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
}
"@
function Sample([System.Diagnostics.Process]$p, [string]$label, [int]$secs) {
    $cores = [Environment]::ProcessorCount
    $p.Refresh(); $c0 = $p.TotalProcessorTime.TotalMilliseconds; $t0 = Get-Date
    $mem = @()
    for ($i = 0; $i -lt $secs; $i++) {
        Start-Sleep -Seconds 1
        $mem += (Get-CimInstance Win32_PerfFormattedData_PerfProc_Process -Filter "IDProcess=$($p.Id)").WorkingSetPrivate / 1MB
    }
    $p.Refresh()
    $cpu = 100.0 * ($p.TotalProcessorTime.TotalMilliseconds - $c0) / (((Get-Date) - $t0).TotalMilliseconds * $cores)
    $m = $mem | Measure-Object -Average -Maximum
    # Skip the settling seconds for the average.
    $tail = $mem | Select-Object -Last ([math]::Max(3, [int]($secs / 2))) | Measure-Object -Average
    "{0,-22} mem {1,6:N1} MB (settled {2,6:N1}, max {3,6:N1})  cpu {4,5:N2}% of machine" -f $label, $m.Average, $tail.Average, $m.Maximum, $cpu
}
$p = Start-Process -FilePath $Exe -PassThru
for ($i = 0; $i -lt 50 -and $p.MainWindowHandle -eq 0; $i++) { Start-Sleep -Milliseconds 200; $p.Refresh() }
Start-Sleep -Seconds 2; $p.Refresh(); $h = $p.MainWindowHandle
$WM_COMMAND = 0x0111; $THBN_CLICKED = 0x1800; $ID_PLAY = 2
$playPause = [IntPtr](($THBN_CLICKED -shl 16) -bor $ID_PLAY)
Start-Sleep -Seconds 3
Sample $p "open, idle" $Phase
[W]::PostMessageW($h, $WM_COMMAND, $playPause, [IntPtr]::Zero) | Out-Null
Start-Sleep -Seconds 3
Sample $p "open, playing" $Phase
[W]::ShowWindow($h, 6) | Out-Null   # SW_MINIMIZE
Start-Sleep -Seconds 2
Sample $p "minimized, playing" $Phase
[W]::PostMessageW($h, $WM_COMMAND, $playPause, [IntPtr]::Zero) | Out-Null
Start-Sleep -Seconds 7               # the device stream stops after 5 s paused
Sample $p "minimized, paused" $Phase
[W]::ShowWindow($h, 9) | Out-Null   # SW_RESTORE
Start-Sleep -Seconds 2
Sample $p "restored, idle" 8
$p.CloseMainWindow() | Out-Null
