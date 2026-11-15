# NetSpeedTaskbar (Rust)

C# 版的 Rust 重构，目标是**功能对齐后再替换**。当前是阶段一：任务栏面板 + 网速/内存视图 + 指示条 + 右键菜单。

```
rs/
  Cargo.toml            workspace
  crates/core/          纯逻辑：settings / net / memory / format（可单测，无 UI）
  crates/app/           Win32 任务栏子窗口 + GDI 绘制
```

## 构建与运行

```powershell
cd rs
cargo build --release
.\target\release\netspeed.exe
```

`cargo test -p netspeed-core` 跑核心逻辑的单测（单位换算、设置规范化、视图循环）。

## 已完成

- **窗口**：`Shell_TrayWnd` 的 `WS_CHILD` 子窗口，注册类 `NetSpeedTaskbarRs.Panel`，贴在通知区域左侧；启动时 `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)`，坐标/尺寸都是物理像素。
- **绘制**：GDI 双缓冲 + `DrawTextW`，字体 Microsoft YaHei UI / Consolas；配色跟随 `AppsUseLightTheme`，底色采样任务栏（采到图标时回退到标准色）。
- **视图**：网速（上/下）、内存（用/总）、**温度（CPU 28°C / GPU 50°C 12W）**、**AI 实时速度 + 今日累计**；左键循环切换并落盘。
- **温度**：CPU 走 WMI `Win32_PerfFormattedData_Counters_ThermalZoneInformation`（0.1K → °C，取最高热区），GPU 走常驻 `nvidia-smi --loop=1`（温度 + 功耗）；两个后台线程各自 2s/1s 刷新，15 秒没更新就显示 `--`，首次进温度视图才启动。
- **AI 采集（三源）**：OpenCode（rusqlite 只读 + 增量）、DSH（ruzstd 解压 `.zstd`）、WorkBuddy / WorkBuddy AI（`.jsonl` 扫 `rawUsage`）。文件按「路径 + mtime」缓存，只重解析变过的文件；采集跑在后台线程（首轮解析几百 MB 不卡 UI），懒启动：只有 AI 视图 / AI 指示条才启用。
- **速度算法**：3 秒窗口摊算 + 在飞估算（未完成响应 + 最近 3 次实测平均）；只把 15 分钟内的未完成行当作在飞，避免库里残留的残缺行让 `generating` 常亮。
- **面板合成**：背缓冲用 `CreateDIBSection` + 全量 alpha=255 —— 普通的 `CreateCompatibleBitmap` alpha 恒为 0，DWM 合成时文字会被当成透明（表现为字极淡、几乎看不见）。
- **指示条**：跟随时按视图取源（网速对数刻度 / 内存占用），右键菜单可改来源或关闭。
- **菜单**：显示网速/内存/温度/AI、底部指示条子菜单、隐藏面板、退出；面板右键和托盘图标共用同一份菜单。
- **托盘图标**：GDI 现画 32×32（深蓝底 + 白色 ↕），左键/右键弹菜单，双击恢复隐藏的面板；悬浮提示随当前视图更新（`网速 上 168 K/s  下 756 K/s`）。
- **设置**：与 C# 版共用 `%LOCALAPPDATA%\NetSpeedTaskbar\settings.ini`，未识别的键原样保留。
- **资源**：工作集约 6 MB，空闲 CPU 0。

## 待办（按优先级）

1. ~~温度采集~~（已完成）。
2. **AI 采集补齐**：Claude Code / Codex（C# 版有，Rust 版还没接），以及按来源拆分显示。
3. **历史窗口**：Win32 窗口 + 自绘表格/图表，移植 C# 的区间胶囊、堆叠柱、每日表、弹簧动效（`DampedSpring` 已可直接复用算法）。
4. **设置窗口**：原生对话框（面板宽度滑块 + 指示条下拉 + 关闭），滑块联动已验证（`WM_HSCROLL` → 面板尺寸 + 写入 ini），`netspeed.exe --settings` 可直接打开。

## 用量历史窗口

`netspeed.exe --history`（或右键菜单「Token 用量历史…」）：

- **区间胶囊**：今天 / 昨天 / 7 天 / 30 天 / 全部；
- **四张卡片**：输出 / 输入 / 请求 / 平均速度，带日均、输入输出倍数等小字；
- **堆叠柱状图**：按来源分层（OpenCode / DSH / WorkBuddy / WorkBuddy AI），带图例和 Y 轴；
- **每日明细表**：日期 / 请求 / 输出 / 输入 / 平均速度 / 峰值 / 主要模型，滚轮翻页；
- **弹簧动效**：切换区间时柱高按 `DampedSpring(0.30s / 0.86)` 过渡（和 AgentSpeed 同参数）；
- **两阶段加载**：先读 OpenCode（毫秒级）出数，再后台扫 DSH / WorkBuddy 文件并追加（首次约 20~30 秒）。
5. **多显示器**：现在只处理主任务栏 `Shell_TrayWnd`，`Shell_SecondaryTrayWnd` 未处理。

## 已知差异

- 没有托盘图标（C# 版有），退出走右键菜单。
- 面板宽度固定读设置，尚未提供 UI 调整。
- 与 C# 版共用 settings.ini 和任务栏区域，**不要同时运行两个版本**。
