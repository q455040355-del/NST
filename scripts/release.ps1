# 发版一条龙：改 workspace 版本 -> 刷新 Cargo.lock -> 提交 -> 打 tag -> 推 main + tag
# （推 tag 会触发 .github/workflows/release.yml，在 windows-latest 上出 zip + 裸 exe）
#
#   powershell -ExecutionPolicy Bypass -File scripts\release.ps1              # 自动 +1 patch
#   powershell -ExecutionPolicy Bypass -File scripts\release.ps1 0.1.49
#   powershell -ExecutionPolicy Bypass -File scripts\release.ps1 -Notes notes.md -Watch
#   powershell -ExecutionPolicy Bypass -File scripts\release.ps1 0.1.49 -DryRun   # 只演练，不改仓库
#
# 会拦下来的情况：工作区不干净 / 不在 main / 落后于 origin / tag 已存在 /
# 版本号没变大 / 版本号和 tag 对不上（tag 必须等于 v+版本号）。

param(
    # 目标版本，如 0.1.49 或 v0.1.49；省略 = 当前版本 +1 patch
    [string]$Version = "",
    # 额外的 release 说明文件（追加到自动生成的提交列表后面）
    [string]$Notes = "",
    # 跳过本地 release 构建（CI 还会再编一次，但本地先编能早点发现编译不过）
    [switch]$NoBuild,
    # 只演练：改完文件、刷完 lock、编完就回滚，不提交/不打 tag/不推
    [switch]$DryRun,
    # 推完盯着 Release 流水线跑完，并列出资产
    [switch]$Watch
)

$ErrorActionPreference = "Stop"

# ---------------------------------------------------------------- 小工具

function Fail([string]$Message) {
    Write-Host ""
    Write-Host "✗ $Message" -ForegroundColor Red
    exit 1
}

function Step([string]$Message) {
    Write-Host "→ $Message" -ForegroundColor Cyan
}

function Run([string]$Exe, [string[]]$Arguments) {
    & $Exe @Arguments
    if ($LASTEXITCODE -ne 0) {
        Fail "$Exe $($Arguments -join ' ') 失败（exit $LASTEXITCODE）"
    }
}

function Capture([string]$Exe, [string[]]$Arguments) {
    $out = & $Exe @Arguments 2>&1
    if ($LASTEXITCODE -ne 0) {
        Fail "$Exe $($Arguments -join ' ') 失败：$out"
    }
    return ($out | Out-String).Trim()
}

function CaptureSoft([string]$Exe, [string[]]$Arguments) {
    $out = & $Exe @Arguments 2>&1
    if ($LASTEXITCODE -ne 0) {
        return ""
    }
    return ($out | Out-String).Trim()
}

function WriteUtf8NoBom([string]$Path, [string]$Text) {
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($Path, $Text, $utf8)
}

# ---------------------------------------------------------------- 前置条件

# 从哪个目录调都能跑：先切到脚本所在目录（仓库里），再定位仓库根
if ($PSScriptRoot) {
    Set-Location $PSScriptRoot
}
$root = Capture git @("rev-parse", "--show-toplevel")
Set-Location $root
$cargoToml = Join-Path $root "Cargo.toml"
$cargoLock = Join-Path $root "Cargo.lock"

# cargo 不在 PATH 里时（比如从任务计划/精简终端里跑），去常见位置找一找：
# 优先 rustup 的 shim（会读 rust-toolchain.toml），其次各工具链自己的 bin。
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $candidates = @("$env:USERPROFILE\.cargo\bin\cargo.exe")
    $candidates += @(Get-ChildItem "$env:USERPROFILE\.rustup\toolchains\*\bin\cargo.exe" -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName })
    $found = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    if ($found) {
        $env:PATH = (Split-Path $found) + ";" + $env:PATH
        Step "cargo 不在 PATH 里，临时改用 $found"
    }
}

foreach ($tool in @("git", "cargo")) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        Fail "找不到 $tool，先在 PATH 里装好"
    }
}

$branch = Capture git @("rev-parse", "--abbrev-ref", "HEAD")
if ($branch -ne "main") {
    Fail "当前在分支 $branch，发版只允许从 main"
}

Step "拉取远端状态"
Run git @("fetch", "origin", "--tags", "--prune")
$dirty = Capture git @("status", "--porcelain")
if ($dirty -ne "") {
    Fail "工作区不干净，先提交或 stash：`n$dirty"
}
$behind = Capture git @("rev-list", "--count", "origin/main..HEAD")
$ahead = Capture git @("rev-list", "--count", "HEAD..origin/main")
if ([int]$ahead -gt 0) {
    Fail "本地 main 落后 origin/main $ahead 个提交，先 git pull"
}

# ---------------------------------------------------------------- 算版本号

$text = [System.IO.File]::ReadAllText($cargoToml)
$anchor = $text.IndexOf("[workspace.package]")
if ($anchor -lt 0) {
    Fail "Cargo.toml 里找不到 [workspace.package]"
}
$match = [regex]::Match($text.Substring($anchor), '(?m)^version\s*=\s*"([^"]+)"')
if (-not $match.Success) {
    Fail "[workspace.package] 下面找不到 version"
}
$current = $match.Groups[1].Value

if ($Version -eq "") {
    $parts = $current.Split(".")
    if ($parts.Count -ne 3) {
        Fail "当前版本 $current 不是 x.y.z，请手动指定目标版本"
    }
    $Version = "$($parts[0]).$($parts[1]).$([int]$parts[2] + 1)"
}
$Version = $Version -replace "^[vV]", ""

if ($Version -notmatch '^\d+\.\d+\.\d+$') {
    Fail "版本号 $Version 不合法，要 x.y.z"
}
if ([version]$Version -le [version]$current) {
    Fail "目标版本 $Version 不大于当前 $current"
}

$tag = "v$Version"
$existing = Capture git @("tag", "--list", $tag)
if ($existing -ne "") {
    Fail "本地已有 tag $tag"
}
$remoteTag = Capture git @("ls-remote", "--tags", "origin", "refs/tags/$tag")
if ($remoteTag -ne "") {
    Fail "远端已有 tag $tag"
}
if ($Notes -ne "" -and -not (Test-Path $Notes)) {
    Fail "说明文件不存在：$Notes"
}

Write-Host ""
Write-Host "  当前版本 : $current"
Write-Host "  目标版本 : $Version   (tag $tag)"
Write-Host "  分支     : $branch（领先 origin $behind 个提交）"
Write-Host ""

# ---------------------------------------------------------------- 改版本 + 刷 lock

Step "改 Cargo.toml 的 workspace 版本"
$head = $text.Substring(0, $anchor)
$tail = $text.Substring($anchor)
$tail = ([regex]'(?m)^version\s*=\s*"[^"]+"').Replace($tail, ('version = "' + $Version + '"'), 1)
WriteUtf8NoBom $cargoToml ($head + $tail)

$lockText = [System.IO.File]::ReadAllText($cargoLock)
$lockPattern = '(?m)(?<head>name = "netspeed-(?:app|core)"\r?\nversion = ")[^"]+(?<tail>")'
$lockNew = ([regex]$lockPattern).Replace($lockText, ('${head}' + $Version + '${tail}'))
if ($lockNew -ne $lockText) {
    WriteUtf8NoBom $cargoLock $lockNew
}

Step "让 cargo 复核/刷新 Cargo.lock"
$metadata = & cargo metadata --format-version 1 --no-deps 2>&1
if ($LASTEXITCODE -ne 0) {
    Fail "cargo metadata 失败：$($metadata | Out-String)"
}

$lockCheck = [System.IO.File]::ReadAllText($cargoLock)
if ($lockCheck -notmatch ('name = "netspeed-app"\r?\nversion = "' + [regex]::Escape($Version) + '"')) {
    Fail "Cargo.lock 没刷新到 $Version，手动检查一下"
}

# ---------------------------------------------------------------- 构建

if (-not $NoBuild) {
    Step "release 构建（CI 也会再编一次，这里先编能早点发现编译不过）"
    Run cargo @("build", "--release", "-p", "netspeed-app")
}

# ---------------------------------------------------------------- 演练模式

if ($DryRun) {
    Step "演练结束，回滚改动"
    Run git @("checkout", "--", "Cargo.toml", "Cargo.lock")
    Write-Host ""
    $buildNote = if ($NoBuild) { "（-NoBuild，构建没跑）" } else { "、构建能过" }
    Write-Host "✓ DryRun 通过：版本 $current -> $Version、lock 能刷新$buildNote。" -ForegroundColor Green
    Write-Host "  正式发版去掉 -DryRun 即可（会提交、打 tag $tag、推 main + tag）。"
    exit 0
}

# ---------------------------------------------------------------- 提交 + 打 tag

Step "提交版本号"
Run git @("add", "Cargo.toml", "Cargo.lock")
Run git @("commit", "-m", "chore(release): $Version")

$prevTag = CaptureSoft git @("describe", "--tags", "--abbrev=0", "HEAD^")
$logArgs = @("log", "--oneline", "--no-decorate")
if ($prevTag -ne "") {
    $logArgs += "$prevTag..HEAD"
}
$logText = Capture git @($logArgs)
if ($prevTag -ne "") {
    # 去掉这次自己的版本提交，别让它出现在 release notes 里
    $logText = ($logText -split "`r?`n" | Where-Object { $_ -notmatch "chore\(release\)" }) -join "`n"
}

$notesPath = Join-Path $env:TEMP "nst-release-notes-$Version.txt"
$body = "v$Version`n`n"
if ($logText -ne "") {
    $body += $logText + "`n"
}
if ($Notes -ne "") {
    $body += "`n" + ([System.IO.File]::ReadAllText((Resolve-Path $Notes)))
}
WriteUtf8NoBom $notesPath $body

Step "打 tag $tag"
Run git @("tag", "-a", $tag, "-F", $notesPath)
Remove-Item $notesPath -ErrorAction SilentlyContinue

# ---------------------------------------------------------------- 推

Step "推 main"
Run git @("push", "origin", "main")
Step "推 tag $tag（会触发 Release 流水线）"
Run git @("push", "origin", $tag)

$sha = Capture git @("rev-parse", "HEAD")
$runUrl = "https://github.com/q455040355-del/NST/actions"
$releaseUrl = "https://github.com/q455040355-del/NST/releases/tag/$tag"

Write-Host ""
Write-Host "✓ 已推送：main + $tag（$sha）" -ForegroundColor Green
Write-Host "  Release 页（流水线跑完才有资产）: $releaseUrl"

# ---------------------------------------------------------------- 盯流水线

if ($Watch) {
    Step "盯 Release 流水线（最多 15 分钟）"
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $repo = "q455040355-del/NST"
    $headers = @{ "User-Agent" = "nst-release-script"; "Accept" = "application/vnd.github+json" }
    $deadline = (Get-Date).AddMinutes(15)
    $lastState = ""
    while ((Get-Date) -lt $deadline) {
        $runs = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/actions/runs?per_page=10" -Headers $headers
        $run = $runs.workflow_runs | Where-Object {
            $_.name -eq "Release" -and $_.head_sha -like "$sha*"
        } | Select-Object -First 1
        if ($run) {
            $state = "$($run.status)/$($run.conclusion)"
            if ($state -ne $lastState) {
                Write-Host "  流水线 #$($run.run_number): $state  $($run.html_url)"
                $lastState = $state
            }
            if ($run.status -eq "completed") {
                if ($run.conclusion -eq "success") {
                    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/tags/$tag" -Headers $headers
                    Write-Host "✓ Release 已生成，资产：" -ForegroundColor Green
                    foreach ($asset in $release.assets) {
                        Write-Host ("  - {0}  {1:N2} MB" -f $asset.name, ($asset.size / 1MB))
                    }
                    Write-Host "  $($release.html_url)"
                    exit 0
                }
                Fail "Release 流水线失败，看 $($run.html_url)"
            }
        }
        Start-Sleep -Seconds 20
    }
    Fail "等了 15 分钟流水线还没结束，自己去看 $runUrl"
}

Write-Host ""
Write-Host "  看流水线: $runUrl"
Write-Host "  下次想连资产一起等，加 -Watch。"
