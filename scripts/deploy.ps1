# 构建 + 部署：cargo build --release -> 停掉正在跑的面板 -> 备份旧 exe -> 覆盖 -> 重启
#
#   powershell -ExecutionPolicy Bypass -File scripts\deploy.ps1
#   powershell -File scripts\deploy.ps1 -Deploy "D:\Other\NST" -NoRestart
#
# 默认部署到 D:\OneDrive\桌面\NST（任务栏面板日常跑的那份）。
# 旧 exe 备份在 %TEMP%\netspeed-before-deploy.exe，拷贝校验不过会自动还原。

param(
    [string]$Deploy = "D:\OneDrive\桌面\NST",
    [string]$Repo = "",
    [switch]$NoRestart
)

$ErrorActionPreference = "Stop"
if (-not $Repo) { $Repo = Split-Path -Parent $PSScriptRoot }

$source = Join-Path $Repo "target\release\netspeed.exe"
$target = Join-Path $Deploy "netspeed.exe"
$backup = Join-Path $env:TEMP "netspeed-before-deploy.exe"

if (-not (Test-Path $Deploy)) { throw "deploy dir not found: $Deploy" }

# 1) 先记下谁在跑：跑的是部署那份才在部署完拉起来（dev 跑 target 的那份就不管了）
$running = @(Get-Process netspeed -ErrorAction SilentlyContinue)
$restart = $false
foreach ($process in $running) {
    if ($process.Path -and $process.Path -ieq $target) { $restart = $true }
}
if ($running.Count -gt 0) {
    $ids = ($running | ForEach-Object { $_.Id }) -join ","
    $running | Stop-Process -Force
    Start-Sleep -Milliseconds 800
    Write-Output "stopped: $ids"
}

# 2) 构建（进程停了，target\release\netspeed.exe 才不会被占用）
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
Push-Location $Repo
try {
    & cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build --release failed ($LASTEXITCODE)" }
} finally {
    Pop-Location
}
if (-not (Test-Path $source)) { throw "build output missing: $source" }

# 3) 备份 + 覆盖 + 校验
if (Test-Path $target) { Copy-Item $target $backup -Force }
Copy-Item $source $target -Force

# 图标素材包跟着一起部署（程序会从 exe 同目录读 icons.png）
$iconsSource = Join-Path (Split-Path -Parent $PSScriptRoot) "icons.png"
if (Test-Path $iconsSource) {
    Copy-Item $iconsSource (Join-Path (Split-Path -Parent $target) "icons.png") -Force
}

$expected = (Get-FileHash $source -Algorithm MD5).Hash
$actual = (Get-FileHash $target -Algorithm MD5).Hash
if ($expected -ne $actual) {
    if (Test-Path $backup) { Copy-Item $backup $target -Force }
    throw "copy mismatch ($expected != $actual), restored backup"
}

# 4) 拉起（只有原来跑着才拉）
if ($restart -and -not $NoRestart) {
    Start-Process -FilePath $target
    Start-Sleep -Milliseconds 1500
    if (-not (Get-Process netspeed -ErrorAction SilentlyContinue)) {
        Write-Warning "启动失败，看 %TEMP%\netspeed-rs.log"
    }
}

if (-not (Test-Path (Join-Path $Deploy "models"))) {
    Write-Warning "部署目录里没有 models\，取字（OCR）会不可用"
}

Write-Output "deployed $target ($actual)"
Write-Output "backup   $backup"
