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
- **底色**：在面板左右两侧的空隙各取 4 个点，按亮度取中位数作为背景色，和任务栏融为一体；采不到才回退到标准色。
- **DPI**：启动即 `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)`；
  窗口里取 DPI 一律走 `GetDpiForWindow`（分层窗口未显示时 `GetDC(hwnd)+LOGPIXELSX` 会返回 96，会把面板算成未缩放尺寸）。
- **刷新**：后台线程每 500ms 向面板投递 `WM_APP_TICK`；历史窗口的弹簧动效用另一个线程每 16ms 投递 `WM_APP_HISTORY_ANIM`。
  ⚠️ 本进程里 `SetTimer` / `WM_TIMER`（含 `TIMERPROC` 回调形式）实测完全不触发，`SetTimer` 还返回成功 ID——只能用线程投递。

## 功能

- **视图**：网速（上/下）、内存（用/总）、温度（CPU/GPU，GPU 带功耗）、AI（实时速度 + 今日累计）；左键循环切换并落盘。
- **指示条**：跟随时按视图取源（网速对数刻度 / 内存占用 / CPU / AI），右键菜单可固定来源或关闭。
- **菜单**：面板右键与托盘共用——显示网速/内存/温度/AI、底部指示条子菜单、Token 用量历史、设置、隐藏面板、退出。
  子窗口不能抢前台，菜单前会先把一个屏幕外的 1×1 顶层窗口设为前台，否则 `TrackPopupMenu` 立刻返回、菜单不显示。
- **托盘图标**：GDI 现画 32×32（深蓝底 + 白色 ↕），双击恢复隐藏的面板，悬浮提示随视图更新（文字不变才 `NIM_MODIFY`，避免闪烁）。
- **温度**：CPU 走 WMI `Win32_PerfFormattedData_Counters_ThermalZoneInformation`（0.1K → °C，取最高热区），
  GPU 走常驻 `nvidia-smi --loop=1`；后台线程分别 2s / 1s 刷新，15 秒没更新显示 `--`，首次进温度视图才启动。
- **AI 采集（三源）**：OpenCode（rusqlite 只读 + WAL 短路）、DSH（ruzstd 解压 `.zstd`）、WorkBuddy / WorkBuddy AI（`.jsonl` 扫 `rawUsage`）。
  文件按「路径 + mtime」缓存只重解析变过的；采集在后台线程，懒启动（AI 视图 / AI 指示条才启用）。
- **全屏避让**：前台窗口铺满整块显示器（且不是任务栏/桌面/弹窗）时自动隐藏面板，退出全屏自动回来；手动「隐藏面板」后不再自动显示，只有双击托盘恢复。

## 截图 + 标注

`Ctrl+Shift+A`（或右键菜单「截图…」）：

- **选区**：全屏冻结 + 压暗遮罩，拖拽选区，实时显示 `宽 × 高`；`Esc` / 右键取消，`Enter` 确认；
- **编辑**：箭头 / 矩形 / 画笔 / 文字 / 马赛克（马赛克按块取平均色），撤销；
- **取字**：`Ctrl+Shift+O` 直接对选区做 OCR；编辑窗里也有「取字」按钮（对当前成品图识别）。结果窗多行可编辑、自动进剪贴板，`Ctrl+A/Ctrl+C` 直接复制；
- **输出**：`Ctrl+C` 复制到剪贴板（CF_DIB）、`Ctrl+S` 另存为 PNG（标准另存为对话框）、`Ctrl+Z` 撤销、`Esc` 关闭；
- **贴图**：一键把结果钉在最上层，拖动移动，右键 / 双击 / Esc 关闭。

实现说明：抓屏走 `BitBlt(SRCCOPY|CAPTUREBLT)`，编辑窗是普通窗口 + GDI 双缓冲；图形统一画在「图片坐标系」里，靠 `SetViewportOrgEx` 平移到显示位置，导出时把原点归零再合成一遍。文字工具用真正的 `EDIT` 子窗口（这样中文输入法可用），`Enter` 提交、`Esc` 取消。另存为对话框要求进程先 `CoInitializeEx`，否则 `GetSaveFileNameW` 直接失败。

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
- **四张卡片**：输出 / 输入 / 请求 / 平均速度，带日均、输入输出倍数等小字；
- **堆叠柱状图**：按来源分层 + 图例 + Y 轴，切区间时柱高按 `DampedSpring(0.30s / 0.86)` 过渡；
- **每日明细表**：日期 / 请求 / 输出 / 输入 / 平均速度 / 峰值 / 主要模型，滚轮翻页；
- **两阶段加载**：先读 OpenCode（毫秒级）出数，再后台扫 DSH / WorkBuddy 文件并追加（首次约 20~30 秒）。

日期分桶统一按**本地日**（`ai::day_index`），不再混用 UTC 天数，否则「今天」会差 8 小时。

## 待办

1. AI 来源补齐：Claude Code / Codex。
2. 多显示器：目前只处理主任务栏 `Shell_TrayWnd`，`Shell_SecondaryTrayWnd` 未处理。
3. 历史窗口记住来源选择 / 支持多选。
