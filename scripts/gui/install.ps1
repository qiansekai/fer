# 把 fer-gui 装到用户目录并创建开始菜单快捷方式（可选，需手动运行）。
# 用法：pwsh -File scripts\install.ps1 [-Uninstall]
param([switch]$Uninstall)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$dstDir = Join-Path $env:LOCALAPPDATA 'fer-gui'
$dst = Join-Path $dstDir 'fer-gui.exe'
$programs = [Environment]::GetFolderPath('Programs')
$lnk = Join-Path $programs 'fer GUI.lnk'

if ($Uninstall) {
    foreach ($p in @($lnk, $dst)) { if (Test-Path $p) { Remove-Item $p -Force; Write-Host ("removed " + $p) } }
    Write-Host 'uninstalled（配置 %LOCALAPPDATA%\fer-gui\config.json 保留）'
    exit 0
}

$src = Join-Path $root 'target-gnu\release\fer-gui.exe'
if (-not (Test-Path $src)) { throw ("未找到 release 产物：" + $src + "；先跑 cargo build --release（GNU 链）") }

New-Item -ItemType Directory -Force -Path $dstDir | Out-Null
Copy-Item $src $dst -Force
Write-Host ("installed " + $dst)

$ws = New-Object -ComObject WScript.Shell
$sc = $ws.CreateShortcut($lnk)
$sc.TargetPath = $dst
$sc.WorkingDirectory = $dstDir
$sc.Description = 'fer GUI - File Express Retriever'
$sc.Save()
Write-Host ("shortcut " + $lnk)
Write-Host '提示：fer serve 需要在线（计划任务 fer-serve-autostart，或界面上的「启动服务」按钮）'
