# GUI 探针：向正在运行的 fer-gui 注入查询并截屏（供人工/agent 验证使用，不依赖 UIA）。
# 用法：pwsh -File scripts\gui_probe.ps1 -Pid <pid> -Query "ext:rs size:>100kb" [-Out _build\shot.png] [-WaitMs 2500]
# 并行验收务必传 -Pid（不传则按窗口标题抓第一个匹配，会把查询注入别人的实例）。
param(
    [string]$Query = '',
    # 只截屏、不注入查询（用于观察当前状态）
    [switch]$ShotOnly,
    [string]$Out = '_build\shot.png',
    [int]$WaitMs = 2500,
    # 多人/多实例并行时必须传：按 pid 定位自己的实例，否则按标题抓到的可能是别人的窗口
    # 注：参数不能叫 $Pid —— 那是 PowerShell 内置只读变量（当前进程 id）
    [int]$TargetPid = 0
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public class FerProbe {
  public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr lParam);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr hWnd, StringBuilder sb, int max);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowEx(IntPtr parent, IntPtr child, string cls, string win);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr SendMessage(IntPtr h, uint msg, IntPtr w, string l);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  public static IntPtr FindByPid(uint want) {
    IntPtr found = IntPtr.Zero;
    EnumWindows(delegate(IntPtr h, IntPtr l) {
      uint pid; GetWindowThreadProcessId(h, out pid);
      if (pid == want) { found = h; return false; }
      return true;
    }, IntPtr.Zero);
    return found;
  }
  public static IntPtr FindByTitle(string needle) {
    IntPtr found = IntPtr.Zero;
    EnumWindows(delegate(IntPtr h, IntPtr l) {
      var sb = new StringBuilder(512);
      GetWindowTextW(h, sb, sb.Capacity);
      if (sb.ToString().Contains(needle)) { found = h; return false; }
      return true;
    }, IntPtr.Zero);
    return found;
  }
}
"@

$hwnd = if ($TargetPid -ne 0) { [FerProbe]::FindByPid([uint32]$TargetPid) } else { [FerProbe]::FindByTitle('fer GUI') }
if ($hwnd -eq [IntPtr]::Zero) { throw 'fer-gui 主窗口未找到（先启动 target-gnu\debug\fer-gui.exe 或 _build\lead-verify\debug\fer-gui.exe）' }
Write-Host ("window hwnd=" + $hwnd)

$edit = if ($ShotOnly) { [IntPtr]::Zero } else { [FerProbe]::FindWindowEx($hwnd, [IntPtr]::Zero, 'Edit', $null) }
if (-not $ShotOnly -and $edit -eq [IntPtr]::Zero) { throw '搜索框（Edit）未找到' }
if (-not $ShotOnly -and [string]::IsNullOrWhiteSpace($Query)) { throw '需要 -Query，或用 -ShotOnly 只截屏' }
if (-not $ShotOnly) { [FerProbe]::SendMessage($edit, 0x000C, [IntPtr]::Zero, $Query) | Out-Null }   # WM_SETTEXT -> 触发 EN_CHANGE
Start-Sleep -Milliseconds $WaitMs

[FerProbe]::SetForegroundWindow($hwnd) | Out-Null
Start-Sleep -Milliseconds 300
$r = New-Object FerProbe+RECT
[FerProbe]::GetWindowRect($hwnd, [ref]$r) | Out-Null
$w = $r.Right - $r.Left; $h = $r.Bottom - $r.Top
$bmp = New-Object System.Drawing.Bitmap($w, $h)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($r.Left, $r.Top, 0, 0, $bmp.Size)
$path = [System.IO.Path]::GetFullPath((Join-Path (Split-Path -Parent $PSScriptRoot) $Out))
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $path) | Out-Null
$bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()
Write-Host ("screenshot=" + $path + " (" + (Get-Item $path).Length + " bytes)")