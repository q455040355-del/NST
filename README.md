# NetSpeedTaskbar (Rust)

任务栏网速/内存/温度/AI 监控面板，纯 Win32 + GDI 原生实现。
**C# 版已放弃**，这里是唯一在维护的实现。

```
Cargo.toml            workspace
crates/core/          纯逻辑：settings / net / memory / temperature / ai / history（可单测，无 UI）
crates/app/           Win32 窗口 + GDI 绘制：panel / tray / settings_ui / history_ui / theme
```

## 构建与运行

```powershell
cd D:\NST
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
cargo build --release          # 先结束 netspeed 进程，否则 os error 5
.\target\release\netspeed.exe
```

工具链由 `rust-toolchain.toml` 锁定 **nightly-2026-09-08**（OCR 引擎 `simd-paddle-ocr-rs` 用了 `portable_simd`，stable 编不过）。

`models\` 里是 PaddleOCR 的 det/cls/rec 模型 + 字典（共 7 MB）；程序按 `exe 同级 models\` → `仓库根 models\` → `当前目录 models\` 的顺序查找，取字功能才可用。

`cargo test -p netspeed-core` 跑核心逻辑单测（换算、设置规范化、区间聚合等，14 个）。

单实例：互斥体 `Local\NetSpeedTaskbar.Rust`。配置：`%LOCALAPPDATA%\NetSpeedTaskbar\settings.ini`。

## 架构要点

- **面板是顶层分层窗口**：`WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST`，
  `WS_POPUP | WS_VISIBLE`，**owner 设为 `Shell_TrayWnd`**——拥有窗口永远排在 owner 之上，
  Explorer 反复重绘任务栏也盖不住它（挂在任务栏下面的 `WS_CHILD` 子窗口会被 `MSTaskListWClass` 压住，点击全丢）。
- **呈现**：背缓冲 `CreateDIBSection`（32bpp，行序 top-down）+ 全量 alpha=255，整体 `UpdateLayeredWindow` 推给 DWM。
  普通 `CreateCompatibleBitmap` 的 alpha 恒为 0，合成后文字近乎透明，必须用 DIB 并手写 alpha。
  ⚠️ 分层窗口的 `WM_PAINT` 里必须自己 `ValidateRect`：`UpdateLayeredWindow` 不会清更新区，
  不清就会无穷重发 `WM_PAINT`（面板又是 TOPMOST，永远排在队首），把同线程其它窗口的
  `WM_PAINT` 全饿死 —— 截图编辑器拖箭头/马赛克发顿就是这个原因。
- **底色**：在面板左右两侧的空隙各取 4 个点，按亮度取中位数作为背景色，和任务栏融为一体；采不到才回退到标准色。
- **DPI**：启动即 `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)`；
  窗口里取 DPI 一律走 `GetDpiForWindow`（分层窗口未显示时 `GetDC(hwnd)+LOGPIXELSX` 会返回 96，会把面板算成未缩放尺寸）。
  面板收到 `WM_DPICHANGED` 会按新 DPI 重建字体/尺寸/背缓冲（拖到缩放不同的屏或改缩放都立即生效）；
  `WM_DISPLAYCHANGE` 会重新贴回任务栏。
- **多显示器**：面板贴着**它所在那块屏**的任务栏（主屏 `Shell_TrayWnd`、副屏 `Shell_SecondaryTrayWnd`，
  各自带 `TrayNotifyWnd` 托盘区），不再一律按主屏算坐标；设置窗/历史窗/取字窗开在**鼠标所在屏**的工作区中间。
  悬浮气泡、右键菜单也按出现位置那块屏的 DPI 和可用区域摆；设置窗拖到缩放不同的屏会重排（字号/尺寸/布局全部按新 DPI 重算）。
  设置窗和历史窗都可以 `Esc` 关闭。
  竖排任务栏（Win10 侧边）：面板是横条塞不进 48px 宽的竖栏，会贴在栏的外侧、纵向对齐托盘。
- **刷新**：后台线程每 500ms 向面板投递 `WM_APP_TICK`；历史窗口的弹簧动效用另一个线程每 16ms 投递 `WM_APP_HISTORY_ANIM`。
  ⚠️ 本进程里 `SetTimer` / `WM_TIMER`（含 `TIMERPROC` 回调形式）实测完全不触发，`SetTimer` 还返回成功 ID——只能用线程投递。

## 功能

- **视图**：网速（上/下）、内存（用/总）、温度（CPU/GPU，GPU 带功耗）、AI（实时速度和今日的输入+输出累计）；左键循环切换并落盘。
- **网速网卡**：默认是「自动（全部合计）」；设置里可选单块网卡（WLAN / 以太网 / Tailscale / vEthernet…），只统计那一块。
  NDIS 过滤层/虚拟交换端口（名字带 `-0000` 的那堆）一律排除——它们和底层网卡上报同一份计数，一起加会把网速放大 5～6 倍。
  单位：≥ 100 KB/s 直接进位到 `M/s`（两位小数），避免 `136.7 K/s` 这种占地方又不好读的写法。
- **刷新间隔**：设置里可选 0.5 / 1 / 2 秒（默认 0.5），四个视图共用；改完立刻生效并落盘。
  计时器线程固定 500ms 投递一次，真正的采样由面板按这个间隔限流（`Panel::last_refresh`）。
- **设置窗口**：全自绘（`WS_POPUP` + 圆角 + 1px 描边），没有系统控件的凹槽边框；
  宽度是自绘滑杆（4px 一档，拖动实时改面板），其余三项是自绘下拉——弹层单独一个 `WS_POPUP`
  窗口，带勾选、悬停高亮、`↑↓/Enter/Esc` 键盘操作、点窗外自动收起。
  改动全部即时生效，不存在「确定/取消」。窗口放不下时内容区可滚轮滚动、也可拖右侧滚动条，
  页头页脚固定；高度按显示器工作区钳制（小屏 / 高缩放不会顶出屏幕）。
- **键盘操作**：设置窗里 `Tab / Shift+Tab` 在控件间循环，`↑↓←→` 改下拉值和面板宽度，
  `空格 / Enter` 拨开关（AI 总开关、四个来源、开机自启）或按「完成」，`Esc` 关闭。
  焦点框只在用键盘时画，鼠标一点就收起来。
- **明暗配色**：面板跟任务栏深浅走，其余窗口（设置 / 历史 / 取字结果 / 截图编辑器 /
  右键菜单 / 悬浮气泡）跟系统 `AppsUseLightTheme` 走（2 秒缓存，切主题不用重启，
  开着窗口也会收到 `WM_SETTINGCHANGE` 立刻重画）。两套颜色集中定义在 `theme::ui()`。
- **开机自启**：设置窗页脚左侧的开关，写 `HKCU\…\Run\NetSpeedTaskbar`（存 exe 绝对路径，
  程序挪位置后下次启动会自动修正）。自启项写失败不影响程序运行。
- **指示条**：只有两种指标 —— 内存占用百分比、Token 速度（0~250 t/s 铺满整条），外加「关闭」；
  旧配置里的 `view/speed/cpu` 会自动迁移到内存。数值变化走缓动动画（约 60fps 推帧，空闲不推）。
  鼠标停在指示条上会弹一个小气泡，说明当前是哪个指标、具体数值多少。
- **面板可拖动**：左键按住面板拖到任务栏任意位置（移动超过 3px 才算拖动，否则还是点一下切视图），
  位置写进 `PanelX/PanelY`（-1 = 自动贴着托盘，托盘图标增减会自己跟着挪）。
- **菜单**：面板右键与托盘共用，**全自绘**（`NetSpeedTaskbarRs.Menu`）：分组标题、勾选当前项、右侧快捷键、
  悬停高亮、`↑↓/Enter/Esc`；「底部指示条」不再是二级菜单，直接平铺成一节，少一次飞起落下。
- **托盘图标**：程序自己算像素画 32×32（圆角深色徽标 + 绿↑ 蓝↓ 双箭头，8×8 超采样、带透明通道），
  托盘、设置窗、历史窗、取字窗共用。悬浮提示随视图更新（文字不变才 `NIM_MODIFY`，避免闪烁）。
  面板被手动隐藏后，**左键单击托盘即可唤回**（不用双击——双击的第一下原来只会弹菜单）。
  Explorer 重启后会重新注册图标（`TaskbarCreated`）。
- **字体**：全程序统一 `Microsoft YaHei UI`（面板的数字也不再是 Consolas）；字号可选 90/100/110/125%，
  面板五个字号等比缩放。面板左侧标签列按实际字宽排，`CPU` / `GPU` 不会再被切掉；
  窄面板 + 大字号时会按剩余宽度继续缩字，保证不被 `DrawText` 裁掉。
- **温度**：CPU 走 WMI `Win32_PerfFormattedData_Counters_ThermalZoneInformation`（0.1K → °C，取最高热区），
  GPU 走常驻 `nvidia-smi --loop=1`；后台线程分别 2s / 1s 刷新，15 秒没更新显示 `--`，首次进温度视图才启动。
- **AI 采集（六源，需手动开启）**：设置里「AI 统计」总开关，默认**关**——关着时不建采集线程、
  不打开任何会话文件/数据库，菜单里也不会出现 AI 视图和 Token 指示条。打开后常驻采集（停在别的视图也在累计），
  并可按来源勾选（两列三行）：
  OpenCode（rusqlite 只读 + WAL 短路）、DSH（`ruzstd` 流式解压）、
  WorkBuddy / WorkBuddy AI（`.jsonl` 扫 `rawUsage`）、
  Claude Code（`~/.claude/projects/**/*.jsonl` 的 `message.usage`，同一条消息流式重复上报按 id 合并取大）、
  Codex（`~/.codex/sessions/**/rollout-*.jsonl` 的 `token_count` 事件，取 `last_token_usage` 增量）。
  没装过的工具目录不存在就直接跳过，不会报错也不会建目录。
  文件按「路径 + mtime + 长度」缓存；jsonl 是**流式逐行**读（峰值只跟最长一行有关，不再整文件进内存），
  且只解析**新增的字节**（记录上次偏移，文件被重写时用头部指纹识别后重来）。坏字节只丢一行，不会废掉整个文件。
  > 面板的「今日」是**输入 + 输出**的合计（口径同历史窗口），且只统计**勾选**的来源（没全开时悬浮说明会标 `n / 6 来源`），
  > 历史窗口统计**全部**来源，两个数字对不上时先看这里；都勾上就一致了。
- **全屏避让**：前台窗口铺满**面板所在那块屏**时才自动隐藏（主屏打游戏不会把副屏的面板藏掉），
  退出全屏自动回来；手动「隐藏面板」后不再自动显示，左键点托盘恢复。

## 截图 + 标注

全部自绘：遮罩、编辑窗（无边框，工具栏空白处可拖窗口）、取字结果窗、贴图窗都是 `WS_POPUP` + 圆角。

`Ctrl+Shift+A`（或右键菜单「截图…」）：

- **选区**：全屏冻结 + 压暗遮罩，拖拽选区，实时显示 `宽 × 高`；`Esc` / 右键取消，`Enter` 确认；
- **编辑**：箭头 / 矩形 / 画笔 / 文字 / 马赛克（马赛克按块取平均色），撤销；
- **选框提示**：马赛克 / 文字拖动时都有**虚线选框 + 尺寸标签**，拖到哪一目了然；马赛克还会实时显示打码效果；
- **文字**：先拖一个框，输入框就开在框里（浅蓝底 + 虚线框），`Enter` 提交，宽度跟框一致；
- **取字（框选区域）**：编辑窗点「OCR」→ 拖框选一块 → **只识别这块**；整图就拖满。
  `Ctrl+Shift+O` 则是先截图再取字，同样支持框选；结果窗多行可编辑、自动进剪贴板；
- **输出**：`Ctrl+C` 复制到剪贴板（CF_DIB）、`Ctrl+S` 另存为 PNG（标准另存为对话框）、`Ctrl+Z` 撤销、`Esc` 关闭；
- **贴图**：一键把结果钉在最上层，拖动移动，右键 / 双击 / Esc 关闭。

实现说明：抓屏走 `BitBlt(SRCCOPY|CAPTUREBLT)`，编辑窗是普通窗口 + GDI 双缓冲；图形统一画在「图片坐标系」里，靠 `SetViewportOrgEx` 平移到显示位置，导出时把原点归零再合成一遍。文字工具用真正的 `EDIT` 子窗口（这样中文输入法可用），`Enter` 提交、`Esc` 取消。另存为对话框要求进程先 `CoInitializeEx`，否则 `GetSaveFileNameW` 直接失败。

拖拽性能（之前拖箭头/马赛克会顿）：原图只拷进常驻内存 DC 一次、背缓冲按客户区尺寸复用、字体走 `cached_font` 缓存；
马赛克从「每块 `CreateSolidBrush` + `FillRect`」改成「先算成一张块级小位图，再一次 `StretchBlt(COLORONCOLOR)` 放大」。
选区遮罩同理：整屏画布只建一次，每帧只刷「上一帧选区」和「这一帧选区」两种行。
实测 600×420 选区拖拽每帧 ≈ 2.0ms（原来要建上千个 GDI 对象），加上面板 `ValidateRect` 修好后不再掉帧。

> 交互参考 [Snow Shot](https://github.com/mg-chao/snow-shot)（GPL-3.0），但**没有复用其任何代码**，全部是这里的 Win32/GDI 实现。

### 取字（OCR）的实现

用 [`simd-paddle-ocr-rs`](https://github.com/mg-chao/simd-paddle-ocr-rs)（**Apache-2.0**，纯 Rust 的 PaddleOCR 推理，不依赖 ONNX Runtime）：

- 引擎在后台线程现建现用：模型加载 ~17 ms、720×220 识别 ~37 ms，不阻塞 UI；
- 模型用 PaddleOCR v6 **tiny** 中文模型（det 1.78 MB + cls 1.02 MB + rec 4.46 MB + 字典），识别完把 `\n` 统一成 `\r\n` 再喂给 EDIT / 剪贴板；
- 结果通过 `PostMessage(WM_APP_OCR_READY)` 回 UI 线程开窗，`Box<OcrOutcome>` 指针走 `wparam`，窗口里 `Box::from_raw` 收回。

> ⚠️ Snow Shot 自己的 `snow-ocr-process` 那层包装是 GPL-3.0，**没有用**；只用 Apache-2.0 的引擎 crate，不传染授权。

## 用量历史窗口

`netspeed.exe --history`（或右键菜单「Token 用量历史…」）：

- **区间胶囊**：今天 / 昨天 / 7 天 / 30 天 / 全部；关掉再打开会记住上次选的区间和来源
  （存在同目录的 `history-prefs.txt`，不动 `settings.ini`，免得和面板那份配置互相覆盖）；
- **来源筛选（多选）**：`全部` 一键全开，右边四个来源点一下开关一个，可任意组合；
  没选中的来源文字变灰、圆点变淡，图例和柱子跟着消失。全关会退回全选（不留空视图）。
  卡片、柱子、表格、副标题都按选中的来源一起过滤；
- **四张卡片**：输出 / 输入 / 请求（小字带**缓存命中率**）/ 平均速度，带日均、输入输出倍数等小字；
- **堆叠柱状图**：按来源分层 + 图例 + Y 轴（六个来源放不下时自动截断图例），切区间时柱高按 `DampedSpring(0.30s / 0.86)` 过渡；
- **每日明细表**：日期 / 请求 / 输出 / 输入 / 平均速度 / 峰值 / 主要模型，滚轮翻页；
- **两阶段加载**：先读 OpenCode（毫秒级）出数，再后台扫文件来源（DSH / WorkBuddy / Claude Code / Codex）并追加。
- **磁盘索引（分片）**：解析结果按「文件 mtime 那天」分片存在
  `%LOCALAPPDATA%\NetSpeedTaskbar\index\usage-YYYYMMDD.bin`，每片头部带内容指纹——
  今天聊了几句只有今天那片会重写（几百 KB），历史那些天原封不动。冷启约 3~5 秒（旧版 20~30 秒），
  之后热启 130~350 ms。老的单文件 `usage-index.bin` 会在首次运行时自动迁移并删除。

日期分桶统一按**本地日**（`ai::day_index`），不再混用 UTC 天数，否则「今天」会差 8 小时。

## 开发 / 回归

```powershell
cargo test --workspace                  # core 的单元测试（解析、分片指纹、索引往返）
powershell -File scripts\smoke.ps1      # UI 冒烟：开窗→压矮→滚轮→拖滚动条→点开关→截图
powershell -File scripts\smoke.ps1 -Dark
```

`smoke.ps1` 会自己起进程（没起的话），用 `WM_APP_MENU_PICK` 打开设置窗，
截图落在 `%TEMP%\nst-smoke\`，并把 `AiEnabled` / `AutoStart` 改完再改回去。
点击坐标按窗口实际宽度换算（`440` 逻辑像素），换缩放比例也能跑。
`-Dark` 走 `NST_FORCE_DARK=1` 强制深色，用来检查两套配色。

CI（`.github/workflows/ci.yml`）在 `windows-latest` 上跑 `cargo test --workspace` 和
`cargo check --workspace --all-targets`；界面相关的验证只能本地跑 smoke。

## 发布

打一个 `v*` 的 tag（例如 `v0.2.0`）就会触发 `.github/workflows/release.yml`：
在 `windows-latest` 上构建 release，把 `netspeed.exe` + **`models/`**（取字模型，约 7 MB）
+ `README.md` + `LICENSE` 打成 `netspeed-<tag>-win64.zip`，挂到对应的 GitHub Release 上
（Release notes 自动生成）。`models/` 不在包里的话「取字」会提示找不到模型，
所以流水线里加了检查，缺了就直接让构建失败。

绿色版：解压后 `netspeed.exe` 旁边必须有 `models\`，配置在 `%LOCALAPPDATA%\NetSpeedTaskbar\`。

崩溃会在 `%TEMP%\netspeed-rs.log` 留一条 `PANIC 文件:行: 消息`（日志超过 1 MB 自动重建）。

## 待办

- 历史窗口来源支持多选（现在是单选，选项本身已会记住）。
