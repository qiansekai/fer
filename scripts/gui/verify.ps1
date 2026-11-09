# fer-gui 验收脚本（Lead 与 verifier 共用）
# 用法：pwsh -File scripts\verify.ps1 [-SkipGui]
# 退出码 0 = 全部 PASS，非 0 = 有 FAIL（逐项打印）。
param([switch]$SkipGui)

$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root 'target-gnu\release\fer-gui.exe'
$serve = 'http://127.0.0.1:19876'
$fail = 0

function Check($name, [bool]$okv, $detail) {
    if ($okv) { Write-Host ("PASS  " + $name + "  " + $detail) }
    else { Write-Host ("FAIL  " + $name + "  " + $detail) -ForegroundColor Red; $script:fail++ }
}

Write-Host "== 1. 产物 =="
Check 'exe 存在' (Test-Path $exe) $exe
if (Test-Path $exe) {
    $size = (Get-Item $exe).Length
    Check 'exe 体积 < 3MB' ($size -lt 3MB) ("{0:N0} 字节" -f $size)
}

Write-Host "== 2. serve 在线 =="
try {
    $h = (Invoke-WebRequest "$serve/api/health" -TimeoutSec 5).Content
    Check 'serve 健康' ($h -match '"ok":true') $h
} catch { Check 'serve 健康' $false $_.Exception.Message }

Write-Host "== 3. HTTP 与 CLI 对照（同查询 total 一致） =="
$queries = @('ext:rs', 'main*.rs', 'size:>1mb', 'type:dir')
foreach ($q in $queries) {
    $enc = [uri]::EscapeDataString($q)
    try {
        $api = (Invoke-WebRequest "$serve/api/search?q=$enc&limit=1" -TimeoutSec 30).Content | ConvertFrom-Json
        $cli = & $env:FER search $q --count-only 2>$null
        if (-not $cli) { $cli = 'n/a' }
        $apiTotal = [int64]$api.total
        $cliTotal = if ($cli -match '^\d+$') { [int64]$cli } else { -1 }
        $okv = ($cliTotal -lt 0) -or ($apiTotal -eq $cliTotal)
        Check ("对照 " + $q) $okv ("api=" + $apiTotal + " cli=" + $cliTotal)
    } catch { Check ("对照 " + $q) $false $_.Exception.Message }
}

if ($SkipGui) { Write-Host "SKIP GUI 段"; exit $fail }

Write-Host "== 4. GUI 启动 =="
$proc = Start-Process -FilePath $exe -PassThru
Start-Sleep -Seconds 3
Check '进程存活' (-not $proc.HasExited) ("pid=" + $proc.Id)
if ($proc.HasExited) { exit $fail }

$ct = (Get-Process -Id $proc.Id).Modules | Where-Object { $_.ModuleName -eq 'COMCTL32.dll' } | Select-Object -First 1
Check 'COMCTL32.dll 为 v6' ($ct -and $ct.FileName -match 'common-controls_6') ($ct.FileName)

Start-Sleep -Seconds 2
Write-Host "== 5. 收尾 =="
Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
Write-Host ("失败项: " + $fail)
exit $fail
