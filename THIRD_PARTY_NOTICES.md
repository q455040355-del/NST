# 第三方代码与许可

## LightScroll（滚动截图的位移匹配）

- 来源：https://github.com/kodaihaoshen-chn/lightscroll （`src/CaptureCore.cs`）
- 许可：MIT
- 用在哪：`crates/app/src/scroll.rs` 里 `Signature` / `signature_score` / `find_motion`
  这一节（行签名、按对比度筛掉空白行、逐像素误差截断、粗搜 + 精算、
  位移有歧义时拒绝而不是猜）。按我们的数据结构重写，逻辑沿用上游。

```
MIT License

Copyright (c) 2026 kodaihaoshen-chn

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Snow Shot

只参考了交互思路（浮窗摆放、自适应抓帧节奏、接缝处理、诊断计数），**没有使用其代码**：
该项目是 GPL-3.0-or-later，与本项目的 MIT 不兼容。
