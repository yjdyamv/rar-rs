# Live tool interop & old-RAR write roadmap

现状：RAR4/老格式**读取**已对 rars 语料 + 真工具生成档全对齐（字节级）；
**RAR5 双向 live 对等已落地**（我方读 WinRAR 7.x 建的 RAR5、7.x UnRAR 读我方建的
RAR5）。本文件记录工具矩阵、live 测试设计，与老 RAR **创建**（`-ma4`）的工程路线。

## 版本地图（软件版本 ≠ 容器格式）

| 软件版本 | 写的容器 | 签名 | 数据编解码（unp_ver） |
|---|---|---|---|
| 1.3/1.4（DOS） | RE~^（前容器） | `RE~^` | 13/14 |
| **1.5–4.x** | **“RAR4”容器** | `Rar!\x1a\x07\x00` | **15**（1.5）→ **20/26**（2.x，音频块）→ **29**（3.0+：PPMd/过滤器） |
| 5.x–7.x | RAR5 容器 | `Rar!\x1a\x07\x01\x00` | v50 / v70（7.0 起） |

- 读老 RAR = RAR4 容器 + 三代引擎（15/20/29）全实现（已做）；PPMd 只见于 RAR 3.x
  时代 -m5 文本（5/6 的 RAR4 写器不产，故夹具只能来自 rars 语料）。
- **创建参考 = WinRAR 6.23**（最后的 `-ma4`；实测 5.91 与 6.23 同输入同开关产物除
  时间戳外逐字节一致）。两者写的都是 **unp_ver 29**（RAR 3/4 引擎），不可控制为
  15/20；`-m5` 也不产 PPMd。→ 写侧只需 **rar29 LZSS+Huffman 编码器** + 归档层
  （无需 15/20 写器、PPMd、音频表/老过滤器写侧——不产 PPMd 仍是合法 RAR4）。

## 参考工具矩阵（跨平台）

| 用途 | Windows | Linux/macOS |
|---|---|---|
| 老 RAR 生成（-ma4 全开关） | **WinRAR 6.23**（rarlab；最后的 -ma4。5.91 输出与之等价，仅在 6.23 不可得时兜底） | **RAR for Linux 6.23**（rarlab `rarlinux-x64-623.tar.gz`，同样 -ma4） |
| 老 RAR 校验（解压真值） | UnRAR 6.23 | 同上（`unrar`） |
| RAR5 生成/校验（双向对等） | WinRAR 7.x 最新（`C:\Program Files\WinRAR` 或 env） | rarlab rarlinux 7.x 最新（`rar`+`unrar` 于 PATH/env） |
| 安装包自解压 | 7-Zip ZS（`yjdyamv/7-Zip-zstd-native` fork 原生 zip，unzip/PowerShell 解；已有安装检测） | fork 原生 tar.gz（系统 tar） |

RAR7（v70）**创建**不做 live 对等：WinRAR 无法指定生成小 RAR7 档（控制台无
`-ma7`；RAR7 只在超大字典场景自动使用），读取侧对等仍由既有 fixture 覆盖。

## Live 测试（`crates/rar/tests/live_interop.rs`，零固定夹具）

默认关闭（无网 `cargo test` 不受扰）：`RAR_LIVE_INTEROP=1` 开启，缺工具自跳过并
打印原因。工具解析顺序：`RAR_TOOLS_DIR/{winrar6,winrar591,winrar7,7z}` →
**持久工具缓存 `.cache/winrar/<ver>/`**（仓库根下，`RAR_CACHE_DIR` 可改；解压一次
终身复用，可手动把任意真品解压目录拷入，如 `6-23`/`5-91`/`7-23`）→ `RAR_7_DIR`/
已知安装位（含 7-Zip ZS、`Program Files\WinRAR`）→ `RAR_LIVE_DOWNLOAD=1` 时联网取
（curl；7-Zip ZS 走 GitHub API latest 静默装到 `.cache/7z-zs`，WinRAR 6.23 从
rarlab 解到 `.cache/winrar/6-23`；安装包留在 `.cache/downloads` 只解不重下）。
临时目录每次调用唯一（并行测试安全）。

已跑通场景（sha256 对齐判定）：
- **老 RAR**（6.23/5.91 `-ma4` 生成 → 我方读 vs 其 UnRAR 解）：m0/m3/m5 ×
  文本/随机/类音频语料、`-s` solid、`-p`、`-hp`（名字隐藏，无口令拒开）、`-v` 分卷。
- **RAR5 双向**（vs WinRAR 7.x）：① 7.x `-ma5` 生成 m0/m3/m5/`-s`/`-p` → 我方读
  == 7.x UnRAR 解；② 我方 lib 建 RAR5（m0/m3/m5、solid+`-p`+`-hp` 组合）→ 7.x
  UnRAR 解 == 源字节。

夹具迁移：`fixtures/rar40/` 保留为离线回归；新能力一律走 live；live 全覆盖后按
PLAN 决定删除固定夹具（目标："完全摆脱仓库测试文件"）。

## 老 RAR 创建（-ma4 对齐 WinRAR 6.23/5.91）路线

复用已按 rars 解码半移植的引擎 + NOTICE 已登记的 rars（WTFPL）许可，补**编码半**
（rars `rar15_40/write.rs` ≈2900 行 + 各 codec 编码器：rar20 ≈2300、rar29 ≈2200、
ppmd ≈700、rarvm/filters），镜像现有 RAR5 写管线：

1. **RAR4 成员写核心**：对齐 6.23 只需 **rar29 LZSS+Huffman 编码器（m1–m5 同一引擎，m5 不产 PPMd）**——块结构/表刷新/end-of-block；先贪心+lazy（与 rars 编码器同源），目标 = 6.23 字节级可解 + 我方读回一致，压缩率差距记录在案。
2. **归档层**：主头/文件头写（EXTTIME、unicode 名、salt、字典位）、solid `-s`（窗口跨成员）、分卷 `-v`（新老命名）、`-p`/`-hp` 加密写、`-rr` 按官方限制（分卷用 .rev）——逐开关对照 6.23 实测。**明确不做**：15/20 写器、PPMd 发射、音频表/RAR2 老过滤器写侧（现代工具不产；不产 PPMd 仍是合法 RAR4）。

验收：live 双向矩阵——6.23 `-ma4` 各开关产物我方读 == 6.23 解；我方建的 RAR4
由 6.23 UnRAR 解 == 我方读 == 源字节；任一方向不一致即红。

进度与结论记 PLAN.md；本文件随设计演进更新。
