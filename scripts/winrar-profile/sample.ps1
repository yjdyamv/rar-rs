# Poor man's sampling profiler for a user-mode process (no admin, no ETW).
#
# Attaches to a running process, suspends its threads as fast as it can, reads
# RIP, and tallies samples per module-relative address. Enough to attribute
# time to functions of an unstripped binary: map the RVAs against the function
# list from Ghidra.
#
#   powershell -NoProfile -File tmp/sample.ps1 -Exe <path> -Args '<args>' -Out samples.txt
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [string]$Args = '',
    [string]$Out = 'samples.txt'
)

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class Prof {
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern IntPtr OpenProcess(uint access, bool inherit, int pid);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern IntPtr OpenThread(uint access, bool inherit, int tid);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern uint SuspendThread(IntPtr thread);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern uint ResumeThread(IntPtr thread);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool CloseHandle(IntPtr handle);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool GetThreadContext(IntPtr thread, IntPtr context);

    public const uint PROCESS_QUERY_INFORMATION = 0x0400;
    public const uint THREAD_SUSPEND_RESUME = 0x0002;
    public const uint THREAD_GET_CONTEXT = 0x0008;
    public const uint THREAD_QUERY_INFORMATION = 0x0040;
    public const uint CONTEXT_CONTROL = 0x00100001;

    public static IntPtr AllocContext() {
        IntPtr raw = Marshal.AllocHGlobal(1232 + 16);
        long aligned = (raw.ToInt64() + 15) & ~15L;
        return new IntPtr(aligned);
    }

    public static void PrepareContext(IntPtr context) {
        for (int i = 0; i < 1232; i++) Marshal.WriteByte(context, i, 0);
        Marshal.WriteInt32(context, 0x30, (int)CONTEXT_CONTROL);
    }

    public static ulong ReadRip(IntPtr context) {
        return (ulong)Marshal.ReadInt64(context, 0xF8);
    }
}
'@

$proc = Start-Process -FilePath $Exe -ArgumentList $Args -PassThru -WindowStyle Hidden
$handle = [Prof]::OpenProcess([Prof]::PROCESS_QUERY_INFORMATION, $false, $proc.Id)
if ($handle -eq [IntPtr]::Zero) { throw "OpenProcess failed: $([System.Runtime.InteropServices.Marshal]::GetLastWin32Error())" }
$base = [uint64]$proc.MainModule.BaseAddress.ToInt64()

$samples = @{}
$total = 0
$context = [Prof]::AllocContext()
[Prof]::PrepareContext($context)

# Thread handles are stable for the life of the process: open once, reuse.
$threads = @{}
$spin = 0

while (-not $proc.HasExited) {
    try {
        foreach ($thread in $proc.Threads) {
            if (-not $threads.ContainsKey($thread.Id)) {
                $h = [Prof]::OpenThread([Prof]::THREAD_SUSPEND_RESUME -bor [Prof]::THREAD_GET_CONTEXT -bor [Prof]::THREAD_QUERY_INFORMATION, $false, $thread.Id)
                if ($h -eq [IntPtr]::Zero) { continue }
                $threads[$thread.Id] = $h
            }
            $t = $threads[$thread.Id]
            [void][Prof]::SuspendThread($t)
            if ([Prof]::GetThreadContext($t, $context)) {
                $rip = [Prof]::ReadRip($context)
                if ($samples.ContainsKey($rip)) { $samples[$rip]++ } else { $samples[$rip] = 1 }
                $total++
            }
            [void][Prof]::ResumeThread($t)
        }
    } catch { }
    # Yield every so often so the sampler does not starve the target.
    $spin++
    if (($spin % 64) -eq 0) { Start-Sleep -Milliseconds 1 }
}

$lines = foreach ($key in $samples.Keys) {
    '{0:x} {1}' -f ([uint64]$key - $base), $samples[$key]
}
$lines | Sort-Object { [int]($_ -split ' ')[1] } -Descending | Set-Content -Path $Out
"sampled $total ticks, $($samples.Count) distinct addresses, base 0x$('{0:x}' -f $base)" | Write-Host
