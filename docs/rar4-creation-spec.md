# RAR4 Creation Feature Spec

## 目标

rar-rs 支持创建 RAR3/4（unp_ver=29）归档，创建的归档**必须能被 WinRAR 6.23 解压**。

## 范围

### Phase 1（本次实现）

| 功能 | 状态 |
|------|------|
| STORE（不压缩） | ✅ |
| LZSS+Huffman m1-m5 | ✅ |
| Solid 模式（-ms） | ✅ |
| 成员级加密（-p） | ✅ |
| 多卷（-v） | ✅ |
| 单卷创建 | ✅ |
| CLI -ma4 开关 | ✅ |

### Phase 2（后续迭代）

| 功能 | 状态 |
|------|------|
| PPMd（-m0） | ❌ |
| 头加密（-hp） | ❌ |

### 不支持（RAR4 格式无此功能）

- Quick-open（QO）
- Recovery record（RR）
- Recovery volumes（.rev）
- BLAKE2sp 哈希
- RAR5 vint 编码头

## 架构设计

### 模块结构

```
rar40/
  mod.rs          ← 已有：常量、flag、block 结构、LegacyDecoder
  read.rs         ← 已有：成员解码门面
  write.rs        ← 新建：RAR4 写管线
codec/
  rar29_encoder.rs ← 新建：RAR29 编码器（从 rars 移植）
```

### 写管线流程

```
ArchiveWriter::open_write()
  ├─ rar4? → write_rar4_signature()     (7 bytes: "Rar!\x1a\x07\x00")
  │          write_rar4_main_header()    (13 bytes, CRC16)
  └─ rar5? → write_signature()          (8 bytes: "Rar!\x1a\x07\x01\x00")
             write_archive_header()

ArchiveWriter::add_file()
  ├─ rar4? → rar40::write::add_member()
  │          ├─ encode → Vec<u8>  (RAR29 encoder)
  │          ├─ encrypt if -p     (Rar30Cipher)
  │          └─ write_file_header + data
  └─ rar5? → rar50::write::add_file()

ArchiveWriter::close()
  ├─ rar4? → write_rar4_endarc()    (optional, only for -hp)
  └─ rar5? → write_end_block()      (QO + RR + end)
```

### RAR4 块格式

```
[2B head_crc16] [1B head_type] [2B flags] [2B head_size] [body...]
```

- CRC16：标准 CRC-32 截断为 16 位，存储在块的前 2 字节
- head_type：MARK_HEAD(0x72) / MAIN_HEAD(0x73) / FILE_HEAD(0x74) / ENDARC_HEAD(0x7b)
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
15    4    host_os = 0 (Windows)
19    4    file_crc32
23    1    unp_ver = 29
24    1    method (0x30=STORE, 0x31-0x35=m1-m5)
25    2    name_size
27    N    filename (UTF-16LE if FHD_UNICODE)
27+N  8    salt (if FHD_PASSWORD)
35+N  ?    exttime (if FHD_EXTTIME)
```

### 编码器接口

```rust
// rar40/write.rs
pub fn encode_member(
    data: &[u8],
    solid: bool,
    level: u8,        // 1-5
    encoder_state: &mut Option<LegacyEncoder>,
) -> RarResult<Vec<u8>>
```

编码器输出 RAR3/4 格式的压缩块序列（不含 FILE_HEAD，只含压缩数据流）。写管线负责：
1. 调用编码器得到压缩数据 `Vec<u8>`
2. 如果有密码，用 Rar30Cipher 加密
3. 构造 FILE_HEAD（含 packed_size、unpacked_size、CRC32）
4. 写入 [FILE_HEAD + encrypted_data]

### 多卷切分

RAR4 多卷切分点在成员边界：
1. 写成员前检查：`head_size + data_size > volume_remaining?`
2. 如果是：填零到卷尾，打开新卷
3. 新卷开头写 MAIN_HEAD + 第一个成员带 `FHD_SPLIT_BEFORE`
4. 最后一个成员带 `FHD_SPLIT_AFTER`（如果是最后一卷则不带）

### 加密

RAR4 成员级加密（-p）：
- 每个成员生成 8 字节随机 salt
- 密钥派生：PBKDF2(password, salt, 0x100000 iterations)
- 加密：AES-256-CBC，IV = 0
- 加密范围：FILE_HEAD 中的文件头（从 packed_size 字段之后开始）+ 数据区
- 未加密的 FILE_HEAD 字段（CRC、大小等）保持明文

### CLI 集成

```bash
# 创建 RAR3/4 归档
rar a -ma4 archive.rar file1 file2

# 创建 solid RAR3/4 归档
rar a -ma4 -ms archive.rar file1 file2

# 创建加密 RAR3/4 归档
rar a -ma4 -p archive.rar file1 file2

# 创建多卷 RAR3/4 归档
rar a -ma4 -v1m archive.rar file1 file2
```

在 `rar-cli/src/rar.rs` 中：
- `archive_version()` 解析 `"4"` → `ArchiveVersion::V29`（旧 `archive_format_force_v70()`，
  2026-09 收敛为单一版本表）
- `CreateOptions` 的 `compression` 字段类型为 `ArchiveVersion`（`"4"` → `V29`，字段原名 `format_version`，2026-09 与 `WriterOptions::compression` 统一）
- RAR4 不兼容的选项（quick_open、blake2、recovery_record、encrypt_headers）在 `-ma4` 时报错

## 测试策略

### 集成测试（`crates/rar/tests/rar4_write.rs`）

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

1. **RAR4 头序列化**：`rar40/write.rs` 中的 `write_block_header()`、`write_file_head()`、`write_main_head()`
2. **STORE-only 创建**：最简单的路径，验证头格式正确
3. **RAR29 编码器移植**：从 rars 移植 `Unpack29Encoder`，适配 rar-rs 错误类型
4. **LZSS 压缩创建**：m1-m5 各级别
5. **Solid 模式**：跨成员共享编码器状态
6. **加密**：成员级 -p
7. **多卷**：-v 参数
8. **CLI 集成**：-ma4 开关
9. **Roundtrip 测试**：创建 → 解压 → diff
10. **WinRAR 互操作测试**
