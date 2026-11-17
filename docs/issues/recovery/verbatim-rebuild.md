# 重建（`rar r` 无恢复记录时）应原样搬块，而不是解压后重存

状态：未开始。设计已定、所需 API 已逐条核实，可以直接动工。

## 症状

同一份归档（text.bin 140,890 B + rand.bin 524,288 B，`-m3`，无 `-rr`），
`rar r` 重建出来的产物：

| | 体积 |
| --- | --- |
| 原归档 | 528,797 B |
| 我们 `rebuilt.x.rar` | **665,288 B** |
| 官方 `rebuilt.y.rar` | **528,789 B** |

`665,288` 正好是两个成员解压后的总长：我们是**逐成员解压、再以 STORE 重新加入**。
官方是**把已压缩的块原样搬过去**。

## 连带丢失

`crates/rar/src/archive/reconstruct.rs` 的函数注释写明「per-member timestamps and
attributes are not preserved」——因为它们住在成员自己的**头块**里，而我们是重新
生成的。官方搬块，所以这三样都保住了：

1. 压缩数据（体积）
2. 每个成员的时间戳
3. 每个成员的属性

注意 legacy（RAR 1.3–4.x）源重建为 RAR4 容器时，STORE 是**有意**的（让 legacy
成员的 `unp_ver` 走 20 而不是容器里的 29）。所以本次改动**只针对 RAR5 源**，
legacy 路径保持原样。

## 为什么现在是安全的（关键设计）

新路径走完之后，**打开产物并逐个成员解码校验**；任何一步不过，就**回退**到现有
的「解码 + STORE」路径。这样即使实现在某个边界上判断错了，也只会退化成今天的行为，
不会抢救出损坏的数据。修复路径上不允许「有时对」。

## 所需零件（都已核实存在）

| 用途 | 位置 | 可见性 |
| --- | --- | --- |
| 宽容地逐块走 | `format/rar5/headers::BlockCursor`（`parse.rs:149`） | `pub(crate)` |
| 主头块构建 | `format/rar5/headers/locator.rs::build_main_header`（:103） | `pub(crate)` |
| 剥掉 QO/RR 记录 | `format/rar5/headers::split_main_extra`（`parse.rs:1227`）→ `(had_qo, had_rr, rest)` | `pub(crate)` |
| 块类型常量 | `format/rar5/mod.rs:35-39`：`BLOCK_TYPE_ARCHIVE_HEADER=0x01`、`FILE_HEADER=0x02`、`END_ARCHIVE=0x05` | `pub` |
| 块元数据 | `format/rar5/headers::BlockMeta`（`mod.rs:55`）：`block_start` / `data_offset` / `data_end` / `header_bytes` / `block_type` / `flags` | `pub` |
| vint | `crate::vint::decode_from_slice` | — |

**关键性质**：RAR5 的文件块是「头 + 数据」的**单一连续跨度**，块头自带 CRC32 且
**不存绝对偏移**，所以

- 搬一个成员 = 拷贝 `block_start..data_end`（`header_bytes` 与数据是连续的）；
- 搬过去之后**不需要修正任何偏移**，块的 CRC 自洽。

## 算法

1. 用既有的宽松/严格打开拿到成员表（**沿用现状**，它就是「哪些成员还能解码」的答案）。
2. 另开 `BlockCursor`（明文归档 `key = None`；`archive_block_key` 那条路先不支持）
   顺序走块，收集每个 `FILE_HEADER` 块的 `(block_start, data_end, 名字)`。
3. 从源的主头块体里取 `(arch_flags, extra)`（`[type vint][flags vint][extra…]`，
   `vint::decode_from_slice` 解两下），用 `split_main_extra` 剥掉 QO/RR。
4. 按名字把「块」和「能解码的成员」对上（用游标顺序匹配，不要假定索引一一对应：
   salvage 扫描会丢掉头损坏的成员）。
5. 落盘：`build_main_header(flags, &extra, false, false, None, offset_width)` 得到主头
   （`quick_open=false, recovery=false`）→ 顺序写入保留块的原始跨度 → 手写
   `BLOCK_TYPE_END_ARCHIVE` 块（`[CRC32][size vint][type=5][flags=0]`）。
6. 校验：重新打开产物、逐个成员解码；统计出的成员集合必须和第 1 步那个集合一致。
   不一致或任何错误 → 返回“不可用”，调用方走旧路径。

## 两个必须先定的语义

- **solid 链**：单个成员不能单独搬（它引用共享窗口）。只有「从链头起、连续完好」
  的一整段可以搬；断链处要么整段降级为重新编码，要么丢。`transaction/solid.rs`
  里有现成的链逻辑可以借鉴。
- **`-hp` 头部加密**：搬的块本来就是磁盘上的加密字节，直接搬是对的；但主头要按加密
  形态重建，而且要 `archive_block_key`。建议先明确拒绝（走旧路径），别半支持。

## 参考：现成的搬运实现

`archive/transaction/` 的 `plan_rewrite`（`plan.rs:23`，用 `BlockCursor` 生成
`RewriteOp::CopyBlock { header_bytes, src_data, len }`）与 `execute.rs` 的落盘器，
就是同一件事的完整实现——只是它们要求归档**能正常打开**，而重建面对的正是打不开的
归档，且 `RewriteOp`/`RewritePlan`/`plan_rewrite`/`execute` 都是模块私有。
如果不想重复实现，另一条路是给它加一个 salvage 模式；无论哪条，上面第 2–6 步的
内容都跑不掉。

## 验收

- 同一份归档重建后体积与官方同量级（≈ 原体积，而不是解压后总和）；
- 时间戳与属性往返保留（用 `lt` 对比原归档与重建产物）；
- 损坏成员被丢弃、其余成员逐字节可解；
- 官方 UnRAR 能解重建产物；
- 现有 `reconstruct`/`recovery` 测试全绿；未支持的情况（legacy、`-hp`）明确走旧路径。
