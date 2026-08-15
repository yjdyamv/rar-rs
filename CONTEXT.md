# CONTEXT — rar-rs

领域词汇。给架构审查和后续 skill 使用；新术语先查这里，模糊了就地改。

## 领域词汇

- **Archive（归档）** — 一个 RAR5 归档：单卷文件或 `.partN.rar` 分卷集。
- **Member（成员）** — 归档中的一个条目（文件 / 目录 / 重定向），对应一个文件头 + 数据区。
- **Volume（分卷）** — 多卷归档的单个 `.partN.rar` 文件；成员数据按卷切成 Chunk。
- **Chunk（分块）** — 跨卷成员在某卷中的数据段。非末块头携带该块密文 CRC32；末块携带（hash-key MAC 过的）明文 CRC，并携带完整 extra 记录。
- **Solid chain（固态链）** — 连续压缩成员共享一个 LZ 窗口；EncoderState/DecoderState 跨成员保持。单卷专属（分卷 + solid 尚未支持）。
- **EncoderState / DecoderState** — 跨块/跨成员保持的编解码状态（lookbehind tail、dist cache、last length、Huffman 表）。
- **Spill file（溢出文件）** — 大文件（≥ `STREAM_COMPRESS_THRESHOLD`，64 MiB）压缩路径的临时落盘文件：压缩流先溢出，头写出后再流式进归档，保证内存有界。
- **Streaming payload（流式负载）** — `write_streamed_payload`：统一流式写路径（单卷/分卷 + 可选流式 AES-256-CBC），`write_stored_file` 是其 STORE 特例。
- **CbcRangeEmitter** — 连续 CBC 密文按任意字节区间发出的机制（read-ahead 到块边界 + ≤15B carry），使加密分块边界任意、卷大小仍精确（与 WinRAR 字节级一致）。
- **Header encryption（-hp）** — 归档级加密头（每卷开头明文），其后所有块为 `[IV][AES-256-CBC 加密头]`。
- **Recovery record（恢复记录）** — 单卷内联 "RR" 服务块，奇偶校验保护归档前缀。
- **Recovery volumes（.rev 恢复卷）** — 分卷集的 Reed-Solomon 奇偶校验卷，可重建缺失卷。
- **Quick-open（QO）** — 主头 locator + 末尾 "QO" 服务块，缓存文件头副本加速列表。
- **BLAKE2sp / hash-key MAC** — 成员哈希记录（`-htb`）；加密成员的校验和用 hash key MAC 保护。
- **Redirect（重定向）** — symlink / hardlink / file-copy 成员（无数据区，仅 extra 记录）。
- **SFX** — 归档前带 stub 的自解压文件；`sfx_offset_of` 定位归档起点。
- **Locator（定位器）** — 主头中的 QO/RR 偏移记录，close 时回填。

## 项目事实

- 库 crate：`rar5`，RAR5 创建/读取 + RAR4 读取；两个 CLI：`rar`（创建/修改/提取）、`unrar`（提取/列表）。
- 热点：`src/archive.rs`（~7500 行，几乎所有提交都动它）。
- 互操作：`tests/winrar_interop.rs`（Windows 本机 WinRAR 双向验证）、`tests/interop.rs`（官方 rar/unrar，env 门控）。
- 计划：`PLAN.md`（已完成记录 + WinRAR 7.23 差距清单）。
