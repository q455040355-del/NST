# 三个窗口的 UI 预览：一次生成编辑器 / 设置 / 历史三张图，并打印 sha256
#
#   powershell -ExecutionPolicy Bypass -File scripts\preview.ps1
#   ...\preview.ps1 -SaveBaseline      把这次结果存成基准
#   ...\preview.ps1 -Compare           和基准比，有变化就返回非零退出码
#   ...\preview.ps1 -Open              生成后打开看
#
# 用途：改 UI 之前存一份基准，改完再 -Compare，就知道有没有意外改动像素。
# 三张图分别走程序里的 NST_EDITOR_PREVIEW / NST_SETTINGS_PREVIEW / NST_HISTORY_PREVIEW。
#
# 注意：预览进程会短暂打开窗口，跑之前会先停掉正在跑的面板，跑完再拉起来。

param(
    [string]$Exe = "",
    [string]$OutDir = "$env:TEMP\nst-preview",
    [switch]$SaveBaseline,
    [switch]$Compare,
    [switch]$Open
)

$ErrorActionPreference = "Stop"

if (-not $Exe) {
    $Exe = Join-Path (Split-Path -Parent $PSScriptRoot) "target\release\netspeed.exe"
}
if (-not (Test-Path $Exe)) {
    throw "找不到 exe：$Exe（先 cargo build --release）"
}

# Compare = 这张图能不能做像素比对。
# 历史窗的数据是后台异步加载的（每次截图时加载进度不同），所以只生成不比对，
# 需要看图就直接打开。
$shots = @(
    @{ Name = "editor.png";   Env = "NST_EDITOR_PREVIEW";   Compare = $true },
    @{ Name = "settings.png"; Env = "NST_SETTINGS_PREVIEW"; Compare = $true },
    @{ Name = "history.png";  Env = "NST_HISTORY_PREVIEW";  Compare = $false }
)

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$baselineDir = Join-Path $OutDir "baseline"

# 面板在跑的话先停掉（预览进程要自己开窗口）
$wasRunning = $false
$runningPath = $null
$running = @(Get-Process netspeed -ErrorAction SilentlyContinue)
if ($running.Count -gt 0) {
    $wasRunning = $true
    # 记下它原来从哪个路径启动的，跑完还从这个路径拉起来
    try { $runningPath = $running[0].Path } catch { $runningPath = $Exe }
    $running | Stop-Process -Force
    Start-Sleep -Milliseconds 700
}

$hashes = @{}
try {
    foreach ($shot in $shots) {
        $target = Join-Path $OutDir $shot.Name
        if (Test-Path $target) { Remove-Item $target -Force }

        $env:NST_EDITOR_PREVIEW = $null
        $env:NST_SETTINGS_PREVIEW = $null
        $env:NST_HISTORY_PREVIEW = $null
        [Environment]::SetEnvironmentVariable($shot.Env, $target, "Process")

        $process = Start-Process -FilePath $Exe -PassThru
        $deadline = (Get-Date).AddSeconds(25)
        while (-not (Test-Path $target) -and (Get-Date) -lt $deadline) {
            Start-Sleep -Milliseconds 200
        }
        if (-not $process.HasExited) {
            $process.WaitForExit(5000) | Out-Null
            if (-not $process.HasExited) { $process.Kill() }
        }
        [Environment]::SetEnvironmentVariable($shot.Env, $null, "Process")

        if (-not (Test-Path $target)) {
            Write-Host ("  {0,-14} 生成失败" -f $shot.Name) -ForegroundColor Red
            continue
        }
        $size = (Get-Item $target).Length
        $hashes[$shot.Name] = (Get-FileHash $target -Algorithm SHA256).Hash
        Write-Host ("  {0,-14} {1,8:N0} 字节  {2}" -f $shot.Name, $size, $hashes[$shot.Name].Substring(0, 16))
    }
}
finally {
    if ($wasRunning) {
        # 面板启动要读配置、加载模型；按进程名轮询，别用 PID 判断（有子进程时不准）
        $launch = if ($runningPath -and (Test-Path $runningPath)) { $runningPath } else { $Exe }
        Start-Process -FilePath $launch | Out-Null
        $deadline = (Get-Date).AddSeconds(12)
        $alive = 0
        while ((Get-Date) -lt $deadline) {
            Start-Sleep -Milliseconds 300
            $alive = @(Get-Process netspeed -ErrorAction SilentlyContinue).Count
            if ($alive -gt 0) { break }
        }
        if ($alive -gt 0) {
            Write-Host ("面板已重启：{0}（{1} 个进程）" -f $launch, $alive) -ForegroundColor Green
        }
        else {
            Write-Host "面板没起来，手动启动一下：$launch" -ForegroundColor Red
        }
    }
}

$stampFile = Join-Path $OutDir "hashes.txt"
$hashes.GetEnumerator() | Sort-Object Name | ForEach-Object { "{0} {1}" -f $_.Value, $_.Key } |
    Set-Content -Path $stampFile -Encoding UTF8

if ($SaveBaseline) {
    New-Item -ItemType Directory -Force -Path $baselineDir | Out-Null
    foreach ($shot in $shots) {
        $from = Join-Path $OutDir $shot.Name
        if (Test-Path $from) { Copy-Item $from (Join-Path $baselineDir $shot.Name) -Force }
    }
    Copy-Item $stampFile (Join-Path $baselineDir "hashes.txt") -Force
    Write-Host "基准已保存：$baselineDir" -ForegroundColor Green
}

if ($Compare) {
    $baseStamp = Join-Path $baselineDir "hashes.txt"
    if (-not (Test-Path $baseStamp)) {
        throw "没有基准可比：先跑一次 -SaveBaseline"
    }
    $base = @{}
    foreach ($line in Get-Content $baseStamp) {
        if ($line -match '^([0-9a-fA-F]{64})\s+(\S+)$') { $base[$Matches[2]] = $Matches[1] }
    }
    $diff = 0
    foreach ($shot in $shots) {
        $name = $shot.Name
        if (-not $shot.Compare) {
            Write-Host ("  {0,-14} 跳过（内容随时间变化）" -f $name) -ForegroundColor DarkGray
            continue
        }
        if (-not $base.ContainsKey($name) -or -not $hashes.ContainsKey($name)) {
            Write-Host ("  {0,-14} 缺少一侧" -f $name) -ForegroundColor Yellow
            $diff++
            continue
        }
        if ($base[$name] -eq $hashes[$name]) {
            Write-Host ("  {0,-14} 没变化" -f $name) -ForegroundColor Gray
        }
        else {
            Write-Host ("  {0,-14} 有变化（基准图在 {1}）" -f $name, $baselineDir) -ForegroundColor Yellow
            $diff++
        }
    }
    if ($diff -gt 0) {
        Write-Host "$diff 张图和基准不一致" -ForegroundColor Yellow
        exit 1
    }
    Write-Host "可比的图和基准完全一致" -ForegroundColor Green
}

if ($Open) {
    foreach ($shot in $shots) {
        $path = Join-Path $OutDir $shot.Name
        if (Test-Path $path) { Start-Process $path }
    }
}

Write-Host "输出目录：$OutDir"
