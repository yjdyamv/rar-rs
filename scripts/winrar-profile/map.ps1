# Maps profiler samples (module-relative RVAs) onto Ghidra's function list.
#
#   powershell -NoProfile -File tmp/map.ps1 -Samples samples_m3.txt -Funcs funcs.json
param(
    [Parameter(Mandatory = $true)][string]$Samples,
    [Parameter(Mandatory = $true)][string]$Funcs,
    [int]$Top = 25
)

$functions = (Get-Content -Raw -Path $Funcs | ConvertFrom-Json).functions |
    ForEach-Object {
        [pscustomobject]@{
            Name = $_.name
            Start = [uint64]([System.Convert]::ToUInt64($_.addr, 16) - 0x140000000)
            Size = [uint64]$_.size
        }
    } | Sort-Object Start

$tally = @{}
$total = 0
foreach ($line in Get-Content -Path $Samples) {
    $parts = $line -split ' '
    if ($parts.Count -ne 2) { continue }
    $rva = [uint64][System.Convert]::ToUInt64($parts[0], 16)
    $count = [int]$parts[1]
    $total += $count

    # Last function that starts at or before the sample.
    $hit = '<unknown>'
    $lo = 0
    $hi = $functions.Count - 1
    while ($lo -le $hi) {
        $mid = [int](($lo + $hi) / 2)
        if ($functions[$mid].Start -le $rva) { $lo = $mid + 1 } else { $hi = $mid - 1 }
    }
    if ($hi -ge 0) {
        $candidate = $functions[$hi]
        if ($rva -lt $candidate.Start + $candidate.Size) { $hit = $candidate.Name }
    }
    if ($tally.ContainsKey($hit)) { $tally[$hit] += $count } else { $tally[$hit] = $count }
}

"total samples $total"
$tally.GetEnumerator() |
    Sort-Object Value -Descending |
    Select-Object -First $Top |
    ForEach-Object { '{0,-24} {1,6}  {2,5:N1}%' -f $_.Key, $_.Value, (100.0 * $_.Value / $total) }
