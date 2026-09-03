# Live tool interop & old-RAR write roadmap

现状：RAR4/老格式**读取**已对 rars 语料 + WinRAR 5.91 生成档全对齐（字节级）。
本文件记录**两条后续主线**的工程设计：① 用真实工具当场生成/校验的交互测试
（逐步摆脱仓库内夹具）；② 老 RAR **创建**（`rar a -ma4`，对齐 WinRAR 5.x）。

## 为什么非要用真工具

- WinRAR 7.x 已删 `-ma4`；RAR 3.x 时代（PPMd 等）只有旧版才能产——"我们拥有旧版
  WinRAR" = 我们能自主造档、全矩阵回归，不必依赖 rars 语料。
- 官方二进制只作黑盒参照（与现 SA_OFFICIAL_* 一致，NOTICE 不变）。

## 工具矩阵（跨平台）

| 用途 | Windows | Linux/macOS |
|---|---|---|
| 老 RAR 生成（-ma4 全开关） | WinRAR 5.91（rarlab，最后支持 -ma4） | 无官方包；wine 跑 5.91，或跳过生成场景 |
| 老 RAR 校验（解压/列表真值） | UnRAR 5.91 | 同上 |
| 7.x 生成/校验（RAR5 对等） | WinRAR 7.x 最新 | rarlab rarlinux/rarosx 7.x 最新 |
| 安装包自解压 | 7-Zip ZS（mcmilk GitHub 最新，Inno 静默装/已有安装检测） | 系统 7z / 下载 zip |

测试入口 `crates/rar/tests/live_interop.rs`，**默认关闭**（无网 `cargo test` 不受扰）：

- `RAR_LIVE_INTEROP=1` 开启；工具缺失时自跳过并打印原因（不失败）。
- 工具解析顺序：`RAR_TOOLS_DIR/{winrar591,winrar7,7z}` → `RAR_591_DIR`/`RAR_7_DIR` →
  已知安装位（含 7-Zip ZS 常见路径）→ `RAR_LIVE_DOWNLOAD=1` 时联网取（curl，
  7-Zip ZS 走 GitHub API latest，WinRAR 5.91 从 rarlab，7z 解包 exe）。
- 缓存 `$RAR_CACHE_DIR`（默认系统 temp/rar-rs-interop）。

场景（现已跑通，WinRAR 5.91 `-ma4` 生成 → 我方读 vs UnRAR 5.91 解出，sha256 对齐）：
m0/m3/m5（文本/随机/类音频语料）、`-s` solid、`-p`、`-hp`（头加密，名字隐藏）、
`-v` 分卷。后续把同构场景推广到 7.x ↔ RAR5（我方创建的 RAR5 由 7.x UnRAR 校验）。

夹具迁移：现有 `fixtures/rar40/` 测试保留为离线回归；新能力一律走 live 测试；
稳定后按 PLAN 决定去留（目标是"完全摆脱仓库测试文件"）。

## 老 RAR 创建（-ma4 对齐 WinRAR 5.x）路线

复用已按 rars 解码半移植的引擎 + NOTICE 已登记的 rars（WTFPL）许可，补**编码半**
（rars `rar15_40/write.rs` ≈2900 行 + 各 codec 编码器：rar20 ≈2300 行、rar29 ≈2200
行、ppmd 编码 ≈700 行、rarvm/filters 编码），按成员/块管线移植，镜像现有 RAR5
写管线结构：

1. **RAR4 成员写核心**（LZSS+Huffman m1–m4）：块结构、表刷新（keep/new）、
   end-of-block 标记；先不做最优解析（用与 rars 编码器同源的贪心+lazy），目标
   = 5.91 字节级可解 + 我方读回一致，压缩率差距记录在案。
2. **PPMd m5 路径**（文本择优）与**音频/过滤器**（RAR2 音频表、RAR3 delta/x86/
   E8E9/RGB/Audio 编码 + 记录字节码发射）——PPMd 的 m5 择优启发式照 rars。
3. **归档层**：`-ma4` 主头/文件头写（含 EXTTIME、unicode 名、salt、字典位）、
   solid（`-s`，窗口跨成员）、分卷（`-v`，新老命名）、`-p`/`-hp` 加密写、
   `-rr` 恢复记录按官方限制（分卷用 .rev）——逐开关对照 5.91 实测。

验收：live 测试双向矩阵——5.91 `-ma4` 各开关产物我方读 == 5.91 解；我方创建的
RAR4 由 **5.91 UnRAR** 解 == 我方读 == 源字节；任一方向不一致即红。这样不依赖
任何固定夹具即可回归全部交互。

进度与结论记 PLAN.md；本文件随设计演进更新。
