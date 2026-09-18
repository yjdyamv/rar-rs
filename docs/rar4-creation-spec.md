# RAR4 Creation Feature Spec

> 最后核对：2026-09-18 @ `1fbfaba`（v15/RAR13 大成员改为增量压缩为本轮改动）；
> 字节级行为由测试与官方工具对拍锁定。

## 目标

rar-rs 支持创建 RAR 1.5 / 2.x / 3.x-4.x（unp_ver
15/20/29）归档，创建的归档**必须能被 WinRAR 6.23 解压**。

## 范围

### Phase 1（本次实现）

| 功能               | 状态 |
| ------------------ | ---- |
| STORE（不压缩）    | ✅   |
| LZSS+Huffman m1-m5 | ✅   |
| Solid 模式（-s）   | ✅   |
| 成员级加密（-p）   | ✅   |
| 多卷（-v）         | ✅   |
| 单卷创建           | ✅   |
| CLI -ma4 开关      | ✅   |

### Phase 2（已完成，2026-09）

| 功能                                                          | 状态 |
| ------------------------------------------------------------- | ---- |
| PPMd 编码                                                     | ✅   |
| 头加密（-hp）                                                 | ✅   |
| 自动 VM 过滤器（E8/E8E9/Delta/Audio 探测 + RGB/Itanium 编码） | ✅   |
| solid 链内自动 VM 过滤器                                      | ✅   |
| 内联恢复记录（NEWSUB 0x7a RR）写/修                           | ✅   |
| 多文件并行 batch                                              | ✅   |
| solid 链 PPMd 模型延续（0x87 头）                             | ⚠️    |

> PPMd 不是独立开关：`-m0` = STORE（WinRAR 定义）；PPMd 由 RAR29 编码器在
> `-m4/-m5`（非 solid）按候选竞争，solid 链内作为与 LZ 并行的模型链赢者推进（见
> `codec/legacy/rar29_encoder.rs` 与 `PLAN.md`「现状」的 RAR4 创建能力）。
> 方法字节仍按 `-m` 级写（0x30+m），块内首个标志位指示 PPMd。
>
> solid 链内 PPMd 成员目前每块都发新模型头（0xA7）：0x87
> 续模型分支在产物里不可达。 开放项见
> [`../PLAN.md`](../PLAN.md)「下一步」。solid 链内的自动 VM 过滤器与之相反，
> 已生效（读者窗口持有的即 LZ 层编码的字节，过滤成员仍是普通链环）。

Recovery volumes（`.rev`）两种布局均已支持，见
`docs/issues/rar4-recovery-volumes.md`。

### 不支持（RAR4 格式无此功能）

- Quick-open（QO）
- BLAKE2sp 哈希
- RAR5 vint 编码头

（内联 RR 已支持，见 `PLAN.md`「现状」的 RAR4 创建能力。）

## 验证记录（2026-09-07 CLI 实测）

`rar a -ma4` 压缩创建端到端可用：2.36 MB 文本语料上 `-m0` → Store （2,367,201
B）、`-m3` → Normal（23,048 B，1.0%）、`-m5` → Best （17,497
B，0.7%）、`-ma4 -s -m5` → Best。互操作覆盖见
`crates/rar-cli/tests/winrar_interop/rar4_create.rs` 的
`we_create_rar4_*`（m3/m5、 solid PPMd、Delta 过滤器、`-p`、`-hp`、`-rr`
双字节校验）与 `cli_behavior/legacy.rs` 的 `cli_ma4_*`。

## 架构设计

### 模块结构

```
format/rar4/
  mod.rs          ← 已有：常量、flag、block 结构、LegacyDecoder（扫描/解析）
  read.rs         ← 已有：成员解码门面
  write/{mod,pipeline,cbc}.rs ← 写管线（头序列化 / 编排 / 加密区间）
  create.rs       ← 选项校验（`validate_rar4_only`）
codec/
  legacy/rar29_encoder.rs ← 新建：RAR29 编码器（从 rars 移植）
```

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

```rust
// format/rar4/write/pipeline.rs
pub fn encode_member(
    data: &[u8],
    solid: bool,
    level: u8,        // 1-5
    encoder_state: &mut Option<LegacyEncoder>,
) -> RarResult<Vec<u8>>
```

编码器输出 RAR3/4 格式的压缩块序列（不含
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

RAR4 成员级加密（-p）按代分派（`archive/create.rs` 的 `rar4_member_encrypt`）：

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

### 集成测试（`crates/rar/tests/rar4_create.rs`）

1. **Roundtrip**：创建 → 解压 → diff 原始文件
2. **WinRAR 兼容**：`unrar l` / `unrar x` 能正确处理我们创建的归档
3. **加密**：创建加密归档 → 用密码解压 → 验证内容
4. **多卷**：创建多卷归档 → 解压 → 验证内容
5. **Solid**：solid 归档 → 解压 → 验证内容
6. **各压缩级别**：m1-m5 各创建一个 → 解压 → 验证
7. **STORE**：不压缩 → 解压 → 验证
8. **空文件/目录**：边界情况

### 互操作测试（需要 WinRAR 6.23）

- `rar create -ma4 test.rar` → WinRAR 能解压
- `rar create -ma4 -p test.rar` → WinRAR 能用密码解压
- `rar create -ma4 -v1m test.rar` → WinRAR 能识别分卷

## 实现顺序

1. **RAR4 头序列化**：`format/rar4/write/mod.rs` 中的
   `build_file_header()`、`write_file_header()`、`build_main_header()`
2. **STORE-only 创建**：最简单的路径，验证头格式正确
3. **RAR29 编码器移植**：从 rars 移植 `Unpack29Encoder`，适配 rar-rs 错误类型
4. **LZSS 压缩创建**：m1-m5 各级别
5. **Solid 模式**：跨成员共享编码器状态
6. **加密**：成员级 -p
7. **多卷**：-v 参数
8. **CLI 集成**：-ma4 开关
9. **Roundtrip 测试**：创建 → 解压 → diff
10. **WinRAR 互操作测试**
