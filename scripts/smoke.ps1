# 面板 / 设置窗冒烟测试（UI 回归用）
#
#   powershell -ExecutionPolicy Bypass -File scripts\smoke.ps1
#   powershell -ExecutionPolicy Bypass -File scripts\smoke.ps1 -Dark
#
# 做四件事：
#   1. 打开设置窗 -> 截图
#   2. 压到 660x520 -> 应出现滚动条、页脚固定 -> 截图
#   3. 滚轮 + 拖滚动条 -> 内容应移动 -> 截图
#   4. 点 AI 总开关 -> ini 的 AiEnabled 应翻转 -> 再点回来复原
#
# 全程只动 AiEnabled（测完恢复原值），截图落在 %TEMP%\nst-smoke\。

param(
    [switch]$Dark,
    [string]$Exe = "D:\NST\target\release\netspeed.exe"
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class Smoke {
  public delegate bool Proc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] static extern bool EnumWindows(Proc p, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll", SetLastError=true)] public static extern bool PostMessageW(IntPtr h, uint msg, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
  [DllImport("user32.dll")] static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }

  public static IntPtr FindClass(string cls) {
    IntPtr found = IntPtr.Zero;
    EnumWindows(delegate(IntPtr h, IntPtr l) {
      StringBuilder sb = new StringBuilder(256);
      GetClassNameW(h, sb, 256);
      if (sb.ToString() == cls) { found = h; return false; }
      return true;
    }, IntPtr.Zero);
    return found;
  }
  public static bool Resize(IntPtr h, int x, int y, int cx, int cy) {
    return SetWindowPos(h, IntPtr.Zero, x, y, cx, cy, 0x0004);
  }
  public static void Wheel(IntPtr h, int delta) {
    PostMessageW(h, 0x020A, (IntPtr)((uint)(ushort)(short)delta << 16), IntPtr.Zero);
  }
  public static void Click(IntPtr h, int x, int y) {
    IntPtr lp = (IntPtr)((y << 16) | (x & 0xFFFF));
    PostMessageW(h, 0x0201, (IntPtr)1, lp);
    PostMessageW(h, 0x0202, IntPtr.Zero, lp);
  }
  public static void Drag(IntPtr h, int x1, int y1, int x2, int y2) {
    IntPtr from = (IntPtr)((y1 << 16) | (x1 & 0xFFFF));
    IntPtr to = (IntPtr)((y2 << 16) | (x2 & 0xFFFF));
    PostMessageW(h, 0x0201, (IntPtr)1, from);
    PostMessageW(h, 0x0200, IntPtr.Zero, to);
    PostMessageW(h, 0x0202, IntPtr.Zero, to);
  }
  public static string RectOf(IntPtr h) {
    RECT r; GetWindowRect(h, out r);
    return r.Left + "," + r.Top + " " + (r.Right - r.Left) + "x" + (r.Bottom - r.Top);
  }
}
"@

[void][Smoke]::SetProcessDPIAware()
$shots = Join-Path $env:TEMP "nst-smoke"
New-Item -ItemType Directory -Force -Path $shots | Out-Null

$failures = New-Object System.Collections.Generic.List[string]
function Check($name, $ok, $detail = "") {
    if ($null -ne $ok -and [bool]$ok) {
        Write-Host ("  [PASS] {0} {1}" -f $name, $detail) -ForegroundColor Green
    } else {
        Write-Host ("  [FAIL] {0} {1}" -f $name, $detail) -ForegroundColor Red
        $failures.Add([string]$name)
    }
}

function Wait-Class([string]$class, [int]$timeoutMs) {
    $deadline = (Get-Date).AddMilliseconds($timeoutMs)
    while ((Get-Date) -lt $deadline) {
        $h = [Smoke]::FindClass($class)
        if ($h.ToInt64() -ne 0) { return $h }
        Start-Sleep -Milliseconds 100
    }
    return [IntPtr]::Zero
}

function Shot([string]$name, [IntPtr]$hwnd) {
    $r = New-Object Smoke+RECT
    [void][Smoke]::GetWindowRect($hwnd, [ref]$r)
    $w = $r.Right - $r.Left; $h = $r.Bottom - $r.Top
    if ($w -le 0 -or $h -le 0) { throw "窗口矩形不合法: ${w}x${h}" }
    $bmp = New-Object System.Drawing.Bitmap $w, $h
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    # 优先让窗口自己画一张（窗口被挡住也能截到），失败再退回屏幕拷贝
    $hdc = $g.GetHdc()
    $printed = [Smoke]::PrintWindow($hwnd, $hdc, 2)
    $g.ReleaseHdc($hdc)
    if (-not $printed) { $g.CopyFromScreen($r.Left, $r.Top, 0, 0, $bmp.Size) }
    $path = Join-Path $shots "$name.png"
    [void]$bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $g.Dispose(); $bmp.Dispose()
    Write-Host ("  截图 {0} ({1}x{2})" -f $path, $w, $h)
    return $path
}

function Hash-Of([string]$path) {
    if (-not (Test-Path $path)) { return "" }
    return (Get-FileHash -Algorithm MD5 -Path $path).Hash
}

$ini = Join-Path $env:LOCALAPPDATA "NetSpeedTaskbar\settings.ini"
function Ini-Ai() {
    if (-not (Test-Path $ini)) { return "" }
    return [string](Get-Content $ini | Select-String '^AiEnabled=').Line
}
$before = Ini-Ai

# ---------------------------------------------------------------- 启动
$panel = [Smoke]::FindClass("NetSpeedTaskbarRs.Panel")
if ($panel.ToInt64() -eq 0) {
    Write-Host "启动 netspeed..."
    if ($Dark) { $env:NST_FORCE_DARK = "1" } else { Remove-Item Env:\NST_FORCE_DARK -ErrorAction SilentlyContinue }
    Start-Process $Exe | Out-Null
    $panel = Wait-Class "NetSpeedTaskbarRs.Panel" 8000
    Start-Sleep -Milliseconds 600
}
Check "面板窗口存在" ($panel.ToInt64() -ne 0)
if ($panel.ToInt64() -eq 0) { exit 1 }

Write-Host ("主题: " + $(if ($Dark) { "深色（强制）" } else { "跟随系统" }))

# ---------------------------------------------------------------- 打开设置
# WM_APP_MENU_PICK(0x8041) + ID_SETTINGS(1301)：和右键菜单点「显示设置」等价
$settings = [IntPtr]::Zero
for ($attempt = 0; $attempt -lt 6 -and $settings.ToInt64() -eq 0; $attempt++) {
    [void][Smoke]::PostMessageW($panel, 0x8041, [IntPtr]1301, [IntPtr]::Zero)
    $settings = Wait-Class "NetSpeedTaskbarRs.Settings" 1000
}
Check "设置窗打开" ($settings.ToInt64() -ne 0)
if ($settings.ToInt64() -eq 0) { exit 1 }
[void][Smoke]::SetForegroundWindow($settings)
Start-Sleep -Milliseconds 400
[void](Shot "settings-full" $settings)

# ---------------------------------------------------------------- 压矮 + 滚动
# 坐标全部按窗口实际尺寸换算：布局是 440 逻辑像素宽，150% 下就是 660。
# 换缩放比例时这里自动跟着变，不用改脚本。
function Window-Scale([IntPtr]$hwnd) {
    $parts = ([Smoke]::RectOf($hwnd)).Split(' ')
    $width = [int]($parts[1].Split('x')[0])
    return $width / 440.0
}
$scale = Window-Scale $settings
$shortW = [int][math]::Round(440 * $scale)
$shortH = [int][math]::Round(347 * $scale)

[void][Smoke]::Resize($settings, 900, 120, $shortW, $shortH)
Start-Sleep -Milliseconds 500
$short = Shot "settings-short" $settings
$rect = [Smoke]::RectOf($settings)
Check "压矮后尺寸不变" ($rect -eq "900,120 ${shortW}x${shortH}") $rect

[Smoke]::Wheel($settings, -360)
Start-Sleep -Milliseconds 350
$wheeled = Shot "settings-wheel" $settings

# 拖滚动条滑块（右侧细轨道，从上往下拖）
$barX = $shortW - [int][math]::Round(7 * $scale)
[Smoke]::Drag($settings, $barX, [int]($shortH * 0.3), $barX, [int]($shortH * 0.85))
Start-Sleep -Milliseconds 350
$dragged = Shot "settings-drag" $settings

Check "滚轮改变了内容" ((Hash-Of $wheeled) -ne (Hash-Of $short))
Check "拖动滚动条改变了内容" ((Hash-Of $dragged) -ne (Hash-Of $wheeled))

# ---------------------------------------------------------------- AI 总开关
# 先重开一个干净的设置窗（滚到底之后开关已经滚出可见区），
# 再滚 3 格（3 × 66 逻辑像素），让 AI 总开关正好落在可见区
[void][Smoke]::PostMessageW($settings, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero)
Start-Sleep -Milliseconds 300
$settings = [IntPtr]::Zero
for ($attempt = 0; $attempt -lt 6 -and $settings.ToInt64() -eq 0; $attempt++) {
    [void][Smoke]::PostMessageW($panel, 0x8041, [IntPtr]1301, [IntPtr]::Zero)
    $settings = Wait-Class "NetSpeedTaskbarRs.Settings" 1000
}
[void][Smoke]::Resize($settings, 900, 120, $shortW, $shortH)
Start-Sleep -Milliseconds 400
[Smoke]::Wheel($settings, -360)
Start-Sleep -Milliseconds 350

# AI 总开关中心：x = 右边距 - 39，y = 449 - 3 格滚动
$aiX = $shortW - [int][math]::Round(39 * $scale)
$aiY = [int][math]::Round((449 - 198) * $scale)
[Smoke]::Click($settings, $aiX, $aiY)
Start-Sleep -Milliseconds 600
$after = Ini-Ai
Check "点击开关翻转 AiEnabled" ($after -ne $before) "$before -> $after"

# 复原
[Smoke]::Click($settings, $aiX, $aiY)
Start-Sleep -Milliseconds 600
$restored = Ini-Ai
Check "再点一次恢复原值" ($restored -eq $before) $restored

# ---------------------------------------------------------------- 开机自启
$runKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
$runName = "NetSpeedTaskbar"
$hadRun = $null -ne (Get-ItemProperty -Path $runKey -Name $runName -ErrorAction SilentlyContinue)

# 页脚左侧的开关（滚轮不影响页脚位置）：x = 175，y = 底边 - 36
$autoX = [int][math]::Round(175 * $scale)
$autoY = $shortH - [int][math]::Round(36 * $scale)
[Smoke]::Click($settings, $autoX, $autoY)
Start-Sleep -Milliseconds 600
$autoIni = [string](Get-Content $ini | Select-String '^AutoStart=').Line
$runValue = (Get-ItemProperty -Path $runKey -Name $runName -ErrorAction SilentlyContinue).$runName
Check "开关写入 AutoStart" ($autoIni -eq "AutoStart=True") $autoIni
Check "注册表自启项已写入" ($null -ne $runValue) $runValue

# 再点一次关掉（回到测试前的状态）
[Smoke]::Click($settings, $autoX, $autoY)
Start-Sleep -Milliseconds 600
$autoIni = [string](Get-Content $ini | Select-String '^AutoStart=').Line
$runValue = (Get-ItemProperty -Path $runKey -Name $runName -ErrorAction SilentlyContinue).$runName
Check "再点一次清掉 AutoStart" ($autoIni -eq "AutoStart=False") $autoIni
if (-not $hadRun) {
    Check "注册表自启项已删除" ($null -eq $runValue) "$runValue"
}

# ---------------------------------------------------------------- 收尾
[void][Smoke]::PostMessageW($settings, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero)
Start-Sleep -Milliseconds 300
Check "设置窗可关闭" (-not [Smoke]::IsWindow($settings))

Write-Host ""
if ($failures.Count -eq 0) {
    Write-Host "全部通过，截图在 $shots" -ForegroundColor Green
    exit 0
}
Write-Host ("失败 {0} 项: {1}" -f $failures.Count, ($failures -join ", ")) -ForegroundColor Red
exit 1
