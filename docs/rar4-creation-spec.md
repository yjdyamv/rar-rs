# RAR4 Creation Feature Spec

> 最后核对：2026-09-22 @ `fbe2f8c`；字节级行为由测试与官方工具对拍锁定。

## 目标

rar-rs 支持创建 RAR 1.5 / 2.x / 3.x-4.x（unp_ver
15/20/29）归档，创建的归档**必须能被 WinRAR 6.23 解压**。

## 范围

### 功能

| 功能                                                                                             | 状态 |
| ------------------------------------------------------------------------------------------------ | ---- |
| 单卷创建、STORE、LZSS+Huffman m1-m5、Solid（`-s`）、成员级加密（`-p`）、多卷（`-v`）、CLI `-ma4` | ✅   |
| PPMd 编码（`-m4`/`-m5`）                                                                         | ✅   |
| 头加密（`-hp`）                                                                                  | ✅   |
| 自动 VM 过滤器（E8/E8E9/Delta/Audio 探测 + RGB/Itanium 编码），solid 链内同样生效                | ✅   |
| 内联恢复记录（NEWSUB 0x7a RR）写/修                                                              | ✅   |
| 多文件并行 batch                                                                                 | ✅   |
| solid 链 PPMd 模型延续（0x87 头）                                                                | ✅   |

> PPMd 不是独立开关：`-m0` = STORE（WinRAR 定义）；PPMd 由 RAR29 编码器在
> `-m4`/`-m5`（非 solid）按候选竞争，solid 链内作为与 LZ 并行的模型链、赢者推进
> （见 `codec/legacy/rar29_encoder.rs`）。方法字节仍按 `-m`
> 级写（`0x30+m`），块内 首个标志位指示 PPMd。**续模型**：首个 PPMd
> 成员发新模型头（0xA7），其后连续的 PPMd
> 成员发续模型头（0x87）并共享模型，中间夹 LZ 成员则回到新模型；官方 6.23 与
> 7.23 都能解出。**6.23 的 RAR4 写入器不产 PPMd，所以写入侧没有官方参考。**

**Recovery volumes（`.rev`）** 两种布局均已支持；格式细节见
[`../CONTEXT.md`](../CONTEXT.md) 的「Recovery volumes」条目。

### 不支持（RAR4 格式无此功能）

- Quick-open（QO）
- BLAKE2sp 哈希
- RAR5 vint 编码头

（内联 RR 已支持，见 [`../PLAN.md`](../PLAN.md)「现状」的 RAR4 创建能力。）

## 验证

端到端由测试锁定，不在此重述过程：`rar4_create.rs`（roundtrip、加密、多卷、solid、
各级别、边界）、`winrar_interop/rar4_create.rs` 的
`we_create_rar4_*`（m3/m5、solid PPMd、Delta、`-p`、`-hp`、`-rr` 双字节校验）与
`cli_behavior/legacy.rs` 的 `cli_ma4_*`。耗时与跑法见
[`testing.md`](testing.md)。

## 架构设计

模块拆分（`format/rar4/` 一族与 `format/rar5/write/` 同形）见
[`ARCHITECTURE.md`](ARCHITECTURE.md)。以下只记 RAR4 写的格式与编解码事实。

### 写管线流程

```
ArchiveWriter::open_write()
  ├─ rar4? → write_rar4_signature()     (7 bytes: "Rar!\x1a\x07\x00")
  │          write_rar4_main_header()    (13 bytes, CRC16)
  └─ rar5? → write_signature()          (8 bytes: "Rar!\x1a\x07\x01\x00")
             write_archive_header()

ArchiveWriter::add_file()
  ├─ rar4? → format::rar4::write 管线 + RAR29 编码器（archive/create.rs 调度）
  │          ├─ encode → Vec<u8>        (Unpack29Encoder)
  │          ├─ encrypt if -p           (Rar30Cipher)
  │          └─ write_file_header + data
  └─ rar5? → format::rar5::write 管线（write_streamed_payload）

ArchiveWriter::close()
  ├─ rar4? → write_rar4_end_block() (always; the header is encrypted under -hp)
  └─ rar5? → write_end_block()      (QO + RR + end)
```

### RAR4 块格式

```
[2B head_crc16] [1B head_type] [2B flags] [2B head_size] [body...]
```

- CRC16：标准 CRC-32 截断为 16 位，存储在块的前 2 字节
- head_type：MARK_HEAD(0x72) / MAIN_HEAD(0x73) / FILE_HEAD(0x74) /
  ENDARC_HEAD(0x7b)
- flags：低位=块标志，高位=字典大小（压缩块）或额外标志
- head_size：包含 CRC + type + flags + size 自身的总头大小

### FILE_HEAD 序列化

```
偏移  大小  字段
0     2    head_crc16
2     1    head_type = 0x74
3     2    flags (FHD_SOLID | FHD_PASSWORD | FHD_UNICODE | FHD_EXTTIME | ...)
5     2    head_size = 32 + name_len + salt_len + exttime_len
7     4    packed_size (压缩后大小，含加密头)
11    4    unpacked_size (原始大小)
15    1    host_os (0 = DOS, 1 = OS/2, 2 = Windows（写侧默认）, 3 = Unix, 4 = Mac)
16    4    file_crc32
20    4    file_time (DOS 本地时间)
24    1    unp_ver (29 default; 15/20 for -ma15/-ma2)
25    1    method (0x30=STORE, 0x31-0x35=m1-m5)
26    2    name_size
28    4    file_attr (0x20 file / 0x10 directory)
32    N    filename (UTF-16LE if FHD_UNICODE)
32+N  8    salt (if FHD_PASSWORD; v29 only — v15/v20 are saltless)
40+N  ?    exttime (if FHD_EXTTIME)
```

### 编码器接口

编码器是 `codec/legacy/rar{29,20,15}_encoder.rs` 上的 `Unpack29Encoder` /
`Unpack20Encoder` / `Unpack15Encoder`，各有 `encode_member(&mut self, input)`
方法（不是自由函数）。输出 RAR3/4 格式的压缩块 序列（不含
FILE_HEAD，只含压缩数据流）。写管线负责：

1. 调用编码器得到压缩数据 `Vec<u8>`
2. 如果有密码，用**该代的密码**加密（v29 = AES-128-CBC + salt、v20 = 16
   字节分组、v15 = XOR 流；见下面「加密」）
3. 构造 FILE_HEAD（含 packed_size、unpacked_size、CRC32）
4. 写入 [FILE_HEAD + encrypted_data]

RAR2 成员是**LZ 块序列**：块以主表符号 269（end-of-block）结束，下一块重新读
表；块之间**位连续**（无字节对齐），因此整个成员是一条位流。整成员单遍编码器（<
64 MiB 的缓冲路径）只写一块；≥ `STREAM_COMPRESS_THRESHOLD` 的大成员由
`codec/legacy/rar20_encoder.rs::encode_member_windowed_streaming` 每 64 KiB 写
一块（有界内存），块间用 `ParseState` 续传 `old_offsets`/last-match——解码端
这两个寄存器本来就不随块边界重置，所以与单块路径压缩率一致。因此**大成员产物
与单块路径不同**（多块/多表），两者都被官方 UnRAR 7.23 接受。

≥ 64 MiB 的成员统一走 spill 流式：v29 = LZ 流式引擎、v20 = 上面的窗口多块、 v15
与 RAR13 = `Unpack15Encoder::encode_member_streaming`（v15 本来是整成员单遍
自适应流，现改为增量：滚动窗口 ≤ 32 KiB + 一个读取块 + ≤ 259 B 前视，自适应表与
flag 分组跨块续用，产物与整成员编码**逐字节相同**）；加密由范围发射器
（`format/rar4/write/cbc.rs`）分代 产生，所以大成员（含 `-p`）不再整块进内存。

### 多卷切分

RAR4 多卷按 `-v` 精确填充：成员可跨卷，中碎片带
`FHD_SPLIT_BEFORE/AFTER`、各自携带片段 CRC，末碎片携带整成员 CRC 与完整
extra；每卷开头写 MAIN_HEAD。

### 加密

RAR4
成员级加密（-p）按代分派（`format/rar4/write/encode.rs::rar4_member_encrypt`）：

- **v29（RAR3/4，`crypto/rar30.rs`）**：每成员 8 字节随机 salt；密钥/IV 由 SHA-1
  链式 KDF（`HASH_ROUNDS = 0x40000`）从口令 + salt 派生（AES-128-CBC，非
  PBKDF2）；加密范围为 FILE_HEAD 中 `packed_size` 之后的头字段 +
  数据区，未加密字段保持明文。
- **v20（RAR2.x）**：块密码，16 字节对齐，无 salt。
- **v15（RAR1.5）**：流式 XOR 密码，无 salt、无 padding。

仅 v29 置 `FHD_SALT`；v15/v20 只置 `FHD_PASSWORD`。`-hp` 头加密与成员版本无关，
读写两侧统一 AES-128（`Rar30Cipher`），只加密头（每块 `[8B salt][密文]`）。

大成员的流式发射用 `format/rar4/write/cbc.rs` 的**范围密码发射器**：
`Rar4BlockRangeEmitter<C>`（v29/v20，按 16 字节分组、末块零填充、跨范围 carry）
与 `Rar15RangeEmitter`（推进 XOR keystream，新增 `Rar15Cipher::skip`），接口是
`Rar4RangeEmitter::emit_to(reader, plain_len, start, end, out)`，
供单卷与分卷共用，因此 `-p` 不再迫使整个成员进内存。

### CLI 集成

```bash
# 创建 RAR3/4 归档
rar a -ma4 archive.rar file1 file2

# 创建 RAR 2.x / 1.5 归档
rar a -ma2 archive.rar file1 file2
rar a -ma15 archive.rar file1 file2

# 创建 solid RAR3/4 归档
rar a -ma4 -s archive.rar file1 file2

# 创建加密 RAR3/4 归档
rar a -ma4 -p archive.rar file1 file2

# 创建多卷 RAR3/4 归档
rar a -ma4 -v1m archive.rar file1 file2
```

在 `crates/rar-cli/src/bin/rar/{args,create}.rs` 中：

- `archive_version()` 解析 `"4"` → `ArchiveVersion::V29`（旧
  `archive_format_force_v70()`， 2026-09 收敛为单一版本表）
- `CreateOptions` 的 `compression` 字段类型为 `ArchiveVersion`（`"4"` →
  `V29`，字段原名 `format_version`，2026-09 与 `WriterOptions::compression`
  统一）
- RAR4 不兼容的选项（quick_open、blake2、owner/streams、RAR7 字典）在 `-ma4`
  时报错；内联 RR（`-rr`）、头加密（`-hp`）与 `.rev`
  恢复卷（`-rv`/`rv`/`rc`，2026-09）**已支持**

## 测试策略

测试布局与跑法归 [`testing.md`](testing.md)：RAR4 创建端的
roundtrip、加密、多卷、 solid 与官方 WinRAR 6.23 对拍都在那里索引。
