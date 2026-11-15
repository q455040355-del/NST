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
- **刷新**：后台线程每 500ms 向面板投递 `WM_APP_TICK`；历史窗口的弹簧动效用另一个线程每 16ms 投递 `WM_APP_HISTORY_ANIM`。
  ⚠️ 本进程里 `SetTimer` / `WM_TIMER`（含 `TIMERPROC` 回调形式）实测完全不触发，`SetTimer` 还返回成功 ID——只能用线程投递。

## 功能

- **视图**：网速（上/下）、内存（用/总）、温度（CPU/GPU，GPU 带功耗）、AI（实时速度 + 今日累计）；左键循环切换并落盘。
- **网速网卡**：默认是「自动（全部合计）」；设置里可选单块网卡（WLAN / 以太网 / Tailscale / vEthernet…），只统计那一块。
  NDIS 过滤层/虚拟交换端口（名字带 `-0000` 的那堆）一律排除——它们和底层网卡上报同一份计数，一起加会把网速放大 5～6 倍。
  单位：≥ 100 KB/s 直接进位到 `M/s`（两位小数），避免 `136.7 K/s` 这种占地方又不好读的写法。
- **刷新间隔**：设置里可选 0.5 / 1 / 2 秒（默认 0.5），四个视图共用；改完立刻生效并落盘。
  计时器线程固定 500ms 投递一次，真正的采样由面板按这个间隔限流（`Panel::last_refresh`）。
- **设置窗口**：全自绘（`WS_POPUP` + 圆角 + 1px 描边，和历史窗口同一套浅色），没有系统控件的凹槽边框；
  宽度是自绘滑杆（4px 一档，拖动实时改面板），其余三项是自绘下拉——弹层单独一个 `WS_POPUP`
  窗口，带勾选、悬停高亮、`↑↓/Enter/Esc` 键盘操作、点窗外自动收起。
  改动全部即时生效，不存在「确定/取消」。
- **指示条**：跟随时按视图取源（网速对数刻度 / 内存占用 / CPU / AI），右键菜单可固定来源或关闭。
- **面板可拖动**：左键按住面板拖到任务栏任意位置（移动超过 3px 才算拖动，否则还是点一下切视图），
  位置写进 `PanelX/PanelY`（-1 = 自动贴着托盘，托盘图标增减会自己跟着挪）。
- **菜单**：面板右键与托盘共用，**全自绘**（`NetSpeedTaskbarRs.Menu`）：分组标题、勾选当前项、右侧快捷键、
  悬停高亮、`↑↓/Enter/Esc`；「底部指示条」不再是二级菜单，直接平铺成一节，少一次飞起落下。
- **托盘图标**：GDI 现画 32×32（深蓝底 + 白色 ↕），悬浮提示随视图更新（文字不变才 `NIM_MODIFY`，避免闪烁）。
  面板被手动隐藏后，**左键单击托盘即可唤回**（不用双击——双击的第一下原来只会弹菜单）。
  Explorer 重启后会重新注册图标（`TaskbarCreated`）。
- **字体**：全程序统一 `Microsoft YaHei UI`（面板的数字也不再是 Consolas）；
  面板左侧标签列按实际字宽排，`CPU` / `GPU` 不会再被切掉，放不下时数值自动降一档字号。
- **温度**：CPU 走 WMI `Win32_PerfFormattedData_Counters_ThermalZoneInformation`（0.1K → °C，取最高热区），
  GPU 走常驻 `nvidia-smi --loop=1`；后台线程分别 2s / 1s 刷新，15 秒没更新显示 `--`，首次进温度视图才启动。
- **AI 采集（三源）**：OpenCode（rusqlite 只读 + WAL 短路）、DSH（ruzstd 解压 `.zstd`）、WorkBuddy / WorkBuddy AI（`.jsonl` 扫 `rawUsage`）。
  文件按「路径 + mtime」缓存只重解析变过的；采集在后台线程，懒启动（AI 视图 / AI 指示条才启用）。
- **全屏避让**：前台窗口铺满整块显示器（且不是任务栏/桌面/弹窗）时自动隐藏面板，退出全屏自动回来；手动「隐藏面板」后不再自动显示，左键点托盘恢复。

## 截图 + 标注

全部自绘：遮罩、编辑窗（无边框，工具栏空白处可拖窗口）、取字结果窗、贴图窗都是 `WS_POPUP` + 圆角。

`Ctrl+Shift+A`（或右键菜单「截图…」）：

- **选区**：全屏冻结 + 压暗遮罩，拖拽选区，实时显示 `宽 × 高`；`Esc` / 右键取消，`Enter` 确认；
- **编辑**：箭头 / 矩形 / 画笔 / 文字 / 马赛克（马赛克按块取平均色），撤销；
- **选框提示**：马赛克 / 文字拖动时都有**虚线选框 + 尺寸标签**，拖到哪一目了然；马赛克还会实时显示打码效果；
- **文字**：先拖一个框，输入框就开在框里（浅蓝底 + 虚线框），`Enter` 提交，宽度跟框一致；
- **取字（框选区域）**：编辑窗点「取字」→ 拖框选一块 → **只识别这块**；整图就拖满。
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

- **区间胶囊**：今天 / 昨天 / 7 天 / 30 天 / 全部；
- **来源筛选**：全部 / OpenCode / DSH / WorkBuddy / WorkBuddy AI，和区间可组合，卡片、柱子、表格、副标题一起过滤；
- **四张卡片**：输出 / 输入 / 请求（小字带**缓存命中率**）/ 平均速度，带日均、输入输出倍数等小字；
- **堆叠柱状图**：按来源分层 + 图例 + Y 轴，切区间时柱高按 `DampedSpring(0.30s / 0.86)` 过渡；
- **每日明细表**：日期 / 请求 / 输出 / 输入 / 平均速度 / 峰值 / 主要模型，滚轮翻页；
- **两阶段加载**：先读 OpenCode（毫秒级）出数，再后台扫 DSH / WorkBuddy 文件并追加（首次约 20~30 秒）。

日期分桶统一按**本地日**（`ai::day_index`），不再混用 UTC 天数，否则「今天」会差 8 小时。

## 待办

1. AI 来源补齐：Claude Code / Codex。
2. 多显示器：目前只处理主任务栏 `Shell_TrayWnd`，`Shell_SecondaryTrayWnd` 未处理。
3. 历史窗口记住来源选择 / 支持多选。
