# RAR5 / RAR7 格式参考

> **权威性**：本文以 rar-rs 的实现为唯一权威（对照 WinRAR 7.23 逐字节互操作验证，
> 见 `crates/rar/tests/{official_interop,winrar_interop}.rs`）。
> 与 bitplane/rars 及其 `rar-research` 规格书冲突之处，**以本文（即 rar-rs）为准**；
> 差异逐条列在第 12 节。
>
> 文中所有 `vint` 均为 RAR5 变长整数（第 2.2 节）；多字节整数一律小端
> （little-endian）。

---

## 1. 总览

### 1.1 RAR5 与 RAR7 的关系

RAR5 与 RAR7 共用**同一个容器格式**（同一签名、同一块信封、同一加密/恢复体系）：

| | RAR5 (v50) | RAR7 (v70) |
|---|---|---|
| 容器签名 | `Rar!\x1a\x07\x01\x00`（8 字节） | 相同 |
| 区分方式 | 文件头压缩信息字段 version = 0 | 压缩信息字段 version = 1（逐成员） |
| 字典编码 | 4 位幂字段（128 KiB << log，≤ 4 GiB） | 5 位幂 + 5 位 1/32 增量（≤ 64 GiB，允许非 2 的幂） |
| 距离码表 | 64 码（DC） | 80 码（DCX，距离可达 ~1 TB） |
| 距离运算 | 32 位 | u64 |

一个归档可以混合 v50 与 v70 成员；"RAR7 归档" 指含 v70 成员的 RAR5 容器。

### 1.2 归档整体布局

```text
┌─────────────────────────────────────────────┐
│ [可选] SFX stub（自解压模块，任意长度）        │
│ [签名] Rar!\x1a\x07\x01\x00           8 字节 │
│ [可选] 归档加密头（-hp，见 7.4）              │
│ [主归档头] block 0x01                        │
│ [文件头 + 数据区] block 0x02（每个成员一组）   │
│ ...                                          │
│ [可选] QO quick-open 服务块（归档尾部）        │
│ [可选] RR 内联恢复记录（QO 之后）              │
│ [归档结束头] block 0x05                      │
└─────────────────────────────────────────────┘
```

- **单卷**：上述顺序；RR 在 QO 之后、END 之前。
- **多卷**：每卷末尾是带 `END_FLAG_NEXT_VOLUME` 的结束头；`-hp` 加密分卷的**每卷**
  开头重复明文归档加密头（7.4）。
- **SFX**：`detect::sfx_offset_of` 扫描首个 `Rar!\x1a\x07\x01\x00` 签名即归档起点。

---

## 2. 基础编码

### 2.1 字节序

所有多字节整数（CRC、时间戳、偏移、大小）均为**小端**。

### 2.2 vint —— RAR5 变长整数

每字节贡献 7 个数据位（bit 0–6），bit 7 为续延标志（1 = 还有后续字节），
**小端序**（低位字节在前）：

| 值范围 | 编码 |
|---|---|
| 0 … 127 | `0xxxxxxx`（1 字节） |
| 128 … 16383 | `1xxxxxxx 0xxxxxxx`（2 字节） |
| … | 依此类推，最多 10 字节 |

解码上限 10 字节；第 10 字节超出 bit 63 的位被忽略。

> **非规范定宽 vint（重要）**：WinRAR 7.x 写头部大小等字段时使用**非最简
> 编码**的定宽 vint（短值也可能占 9–10 字节）。因此：
> - 块头 CRC32 必须对**磁盘上存储的原始 vint 字节 + body** 计算，而不是
>   对重新编码后的规范 vint 计算（`read_block` 即如此）；
> - 主头 locator 的 QO/RR 偏移字段用**固定 5 字节 vint**（`vint_fixed5`）
>   预分配，close 时原地回填（值 < 2^35 时成立）。

---

## 3. 块信封（Block Envelope）

### 3.1 通用结构

每个块（无论类型）共用同一外层：

```text
[头部 CRC32]   4 字节 LE，覆盖 [size vint 原字节 + body]
[头部大小]    vint，值为其之后所有字节数（含 type/flags/.../extra）
[块类型]      vint
[块标志]      vint
[extra 大小]  vint —— 仅当块标志含 EXTRA_DATA (0x0001)
[数据大小]    vint —— 仅当块标志含 DATA_AREA (0x0002)
... 类型专属字段 ...
[extra 区]    字节 —— 仅当块标志含 EXTRA_DATA
```

头部大小上限：读取侧拒绝 0 与 > 2 MiB 的值。

### 3.2 头部 CRC32

```text
crc = CRC32(size_vint_原字节 ‖ body)
```

注意是**原始 vint 字节**参与校验（见 2.2 的非规范 vint 说明）。

### 3.3 块类型

| 值 | 名称 | 说明 |
|---|---|---|
| 0x01 | 归档头 | 归档级元数据（分卷/solid/锁定/恢复标志、locator） |
| 0x02 | 文件头 | 一个成员（文件/目录/重定向）+ 紧随的数据区 |
| 0x03 | 服务头 | CMT / QO / RR / STM 等服务块 |
| 0x04 | 归档加密头 | `-hp`：其后所有块为 `[IV][AES-256-CBC 密文]` |
| 0x05 | 归档结束 | 单卷结尾，或带 next-volume 标志的分卷结尾 |

### 3.4 通用块标志

| 位 | 常量 | 含义 |
|---|---|---|
| 0x0001 | EXTRA_DATA | 块头尾部有 extra 区 |
| 0x0002 | DATA_AREA | 块头后紧跟数据区（大小 = 数据大小 vint） |
| 0x0004 | SKIP_IF_UNKNOWN | 未知类型块应被跳过而非报错 |
| 0x0008 | DATA_CONTINUES | 数据区延续到下一卷 |
| 0x0010 | DATA_CONTINUE_TO | 数据区是上一卷的延续（本卷开头无数据） |
| 0x0020 | DEPENDS_PREV | 依赖前一文件头（STM 流记录） |
| 0x0040 | PRESERVE_CHILD | 保留子块（读写器可忽略） |

---

## 4. 块详解

### 4.1 归档签名

```text
52 61 72 21 1A 07 01 00        "Rar!" 1A 07 01 00
```

RAR7 签名相同。RAR4 及更早（`Rar!\x1a\x07\x00` / `RE~^`）被 rar-rs 明确拒绝。

### 4.2 主归档头（0x01）

```text
[块类型 = 0x01] [块标志] [extra 大小?]
[归档标志] vint
[卷号] vint —— 仅当归档标志含 VOLUME_NUM (0x0002)
[extra 区] —— 仅当 EXTRA_DATA
```

**归档标志**：

| 位 | 常量 | 含义 |
|---|---|---|
| 0x0001 | VOLUME | 这是分卷集的一员 |
| 0x0002 | VOLUME_NUM | 后面跟随卷号（1 起始） |
| 0x0004 | SOLID | 固态归档 |
| 0x0008 | RECOVERY | 归档含内联恢复记录（RR） |
| 0x0010 | LOCKED | 归档已锁定（只读） |

**Locator 记录**（主头 extra 区中 type = 0x01 的记录）：

```text
[记录大小 vint][记录类型 vint = 0x01]
[标志 vint]   —— 0x0001 有 QO 偏移，0x0002 有 RR 偏移
[QO 偏移 vint] —— 若有；相对归档起点（签名之后），固定 5 字节 vint
[RR 偏移 vint] —— 若有；同上
```

偏移在 close 时回填（QO 记录起始 / RR 记录起始的绝对位置）。

### 4.3 文件头（0x02）

```text
[块类型 = 0x02] [块标志] [extra 大小?] [数据大小 = packed_size?]
[文件标志] vint
[解压大小] vint
[属性] vint
[mtime] 4 字节 LE —— 仅当文件标志含 TIME_UNIX (0x0002)
[CRC32] 4 字节 LE —— 仅当文件标志含 CRC32 (0x0004)
[压缩信息] vint
[host OS] vint          —— 0 = Windows，1 = Unix
[名字长度] vint
[名字] UTF-8 字节
[extra 区] —— 仅当 EXTRA_DATA
```

**文件标志**：

| 位 | 常量 | 含义 |
|---|---|---|
| 0x0001 | DIRECTORY | 目录条目（无数据区） |
| 0x0002 | TIME_UNIX | 头部有 4 字节 Unix mtime |
| 0x0004 | CRC32 | 头部有 4 字节 CRC32 |
| 0x0008 | UNKNOWN_SIZE | 解压大小未知（流式） |

**压缩信息字段**（一个 vint，位布局）：

```text
bit  0–5   version   算法版本：0 = RAR5 (v50)，1 = RAR7 (v70)
bit  6     solid     固态成员（共享 LZ 窗口）
bit  7–9   method    压缩方法：0 = store，1–5 = 级别
bit 10–14  dict      RAR5：4 位幂（bits 10–13），128 KiB << log
                     RAR7：5 位幂（bits 10–14）+ bits 15–19 的 1/32 增量
bit 15–19  dict_frac RAR7 专用：1/32 增量
bit 20    (rar5_compat 可选标志，rar-rs 不写、读取时忽略，见 11.4)
```

- **RAR5 字典**：`128 KiB << comp_dict_size`（log 0–15，即 128 KiB … 4 GiB）。
- **RAR7 字典**：`(32 + dict_frac) << (dict_power + 12)`（等价
  `base + base/32 × dict_frac`，其中 `base = 128 KiB << dict_power`）。
  非 2 的幂允许，上限 64 GiB。

**压缩方法**：0 = Store；1 = Fastest；2 = Fast；3 = Normal；4 = Good；5 = Best。

### 4.4 服务块（0x03）

服务块复用文件头的外形（type = 0x03，名字在名字字段），数据区承载服务负载，
extra 区承载 SUBDATA 记录（type 0x07）。rar-rs 支持四种服务：

| 名字 | 负载 | extra SUBDATA | 块标志 |
|---|---|---|---|
| `CMT` | 注释文本（数据区，store） | 无 | DATA_AREA |
| `QO` | quick-open 缓存（见下） | 空记录（仅类型字节） | DATA_AREA + SKIP_IF_UNKNOWN |
| `RR` | 内联恢复奇偶数据（9.1） | 1 字节恢复百分比 | DATA_AREA + SKIP_IF_UNKNOWN |
| `STM` | NTFS 备用流内容（数据区） | 流名 `:name` | DATA_AREA + DEPENDS_PREV |

**QO 负载结构**（每个缓存的文件头一条）：

```text
[条目 CRC32] 4 字节 LE，覆盖 [body]
[body 大小] vint
[body] = [条目标志 vint = 0] [相对偏移 vint] [头部大小 vint] [文件头原字节]
```

相对偏移 vint = QO 记录起始位置 − 文件头在归档中的绝对位置（正值，指向回头的字节距离）。

### 4.5 归档加密头（0x04）

```text
[块类型 = 0x04] [块标志 = 0]
[加密版本] vint = 0（AES-256）
[加密标志] vint —— 归档级只写 0x0001（密码校验位）
[强度] 1 字节 —— KDF 迭代次数指数（2^strength，默认 15）
[盐] 16 字节
[12 字节密码校验值] —— 仅当加密标志含 0x0001
```

- 归档级加密头**不含 IV**——其后每个块自带 16 字节 IV。
- `-hp` 分卷集的**每一卷开头**都重复此明文块（同盐/同校验值）。
- 该头之后所有块均为 `[16 字节 IV][AES-256-CBC 加密头密文]`（zero-fill 填充）。

### 4.6 归档结束头（0x05）

```text
[块类型 = 0x05] [块标志 = SKIP_IF_UNKNOWN (0x0004)] [结束标志] vint
```

- **结束标志**：0x0001 = NEXT_VOLUME（后面还有分卷）。
- 分卷非末卷写 `NEXT_VOLUME`，末卷写 0。
- **陷阱**：`0x0004` 是真实的块标志位（7-Zip 如此解析）；把结束标志塞进块标志位
  会让 7-Zip 把 1 误读为 EXTRA_DATA 导致多卷失败。WinRAR 在此写 `SKIP_IF_UNKNOWN`。

---

## 5. Extra 记录

所有 extra 记录共用信封：

```text
[记录大小 vint] —— 值为记录类型字节及之后所有字节数
[记录类型 vint]
[记录体] ...
```

| 类型 | 名称 | 出现位置 |
|---|---|---|
| 0x01 | 文件加密参数 | 加密文件头 |
| 0x02 | 哈希（BLAKE2sp） | 文件头（`-htb`） |
| 0x03 | 文件时间（HTIME） | 文件头（`-ts` 三时间戳） |
| 0x04 | 文件版本 | 文件头（`-ver[n]`） |
| 0x05 | 重定向 | 链接/复制成员（无数据区） |
| 0x06 | owner/group | 文件头（`-ow`） |
| 0x07 | 服务数据 SUBDATA | 服务块（RR 百分比、STM 流名） |

### 5.1 文件加密（0x01）

```text
[加密版本] vint = 0
[加密标志] vint —— 0x0001 有密码校验值；0x0002 用 hash-key MAC（WinRAR 默认）
[强度] 1 字节
[盐] 16 字节
[IV] 16 字节（本文件专属，随机）
[12 字节密码校验值] —— 仅当标志含 0x0001
```

### 5.2 哈希（0x02）

```text
[哈希类型] vint = 0（BLAKE2sp）
[32 字节哈希值]
```

BLAKE2sp 是 8 路并行的 BLAKE2s 树哈希，输出 32 字节（`-htb`）。
加密文件的哈希值用 hash-key MAC 保护（7.5）。

### 5.3 文件时间 HTIME（0x03）

```text
[标志] vint
[时间字段……]（见下）
```

标志位：0x0001 Unix 秒格式；0x0002 mtime；0x0004 ctime；0x0008 atime；
0x0010 纳秒精度。

- **Unix 秒格式**（rar-rs 只写此格式）：每个在场时间 4 字节 LE Unix 秒；
  若 0x0010，则按同样顺序追加每个在场时间的 4 字节纳秒字段（低 30 位有效，
  ≥ 10^9 视为 0）。布局：**先全部秒字段，再全部纳秒字段**（mtime/ctime/atime 序）。
- **FILETIME 格式**（读取兼容）：每个在场时间 8 字节 LE，100 ns 单位自 1601-01-01；
  秒 = `ft / 10^7 − 11644473600`，纳秒 = `(ft % 10^7) × 100`。
- 头部 mtime 字段优先以 HTIME 的 mtime 覆盖。

### 5.4 文件版本（0x04）

```text
[版本号] vint
```

### 5.5 重定向（0x05）

```text
[重定向类型] vint
[标志] vint
[目标名字长度] vint
[目标名字] UTF-8
```

| 类型 | 含义 |
|---|---|
| 0x01 | Unix 符号链接 |
| 0x02 | Windows 符号链接 |
| 0x03 | Windows junction |
| 0x04 | 硬链接 |
| 0x05 | 文件复制（目标已存在于归档内） |

重定向成员无数据区；未知类型提取时降级为空普通文件。

### 5.6 owner/group（0x06）

```text
[标志] vint —— 0x0001 owner 在场；0x0002 group 在场
[owner 长度 vint][owner UTF-8]   —— 若 0x0001
[group 长度 vint][group UTF-8]   —— 若 0x0002
```

### 5.7 服务数据 SUBDATA（0x07）

服务块的 extra 区中类型 0x07 的记录；负载即服务专属数据：
RR 的 1 字节恢复百分比；STM 的流名 `:name`。

---

## 6. 压缩数据流（LZSS + Huffman）

### 6.1 压缩块头

压缩数据是若干块的串联；每块：

```text
[标志] 1 字节
[校验] 1 字节
[大小字段] 1–3 字节 LE
[块数据]……
```

- **标志位**：bit 7 = 含 Huffman 表；bit 6 = 本成员最后一块；bit 5–3 =
  大小字段宽度 − 1（1/2/3 字节）；bit 2–0 = 末字节有效位数 − 1。
- **校验** = `0x5A ^ 标志 ^ 大小字段每字节`。
- **大小字段**：块数据字节数。1 字节 ≤ 0xFF；2 字节 ≤ 0xFFFF；否则 3 字节。
- 空成员：单个标志 = 0x40（末块、无表）的块。

### 6.2 Huffman 表

每块（含表时）先写 4 张表，符号数：

| 表 | 符号数 | 用途 |
|---|---|---|
| BC | 20 | 编码其余表的码长 |
| NC | 306 | 主符号（字面量/过滤器/重复/匹配） |
| DC | 64（RAR5）/ 80（RAR7, DCX） | 距离低位/额外位 |
| LDC | 16 | 大距离的低 4 位 |
| RC | 44 | 过滤器参数 |

码长上限 15 位。解码用 quick-lookup（2^10 表）加速。

**表编码**：4 张表的码长串联（NC + DC + LDC + RC），用 RLE 符号（经 BC 表
Huffman 编码）表示，其中符号 16/17/18/19 带额外位：

| 符号 | 含义 | 额外位 |
|---|---|---|
| 0–15 | 直接码长 | — |
| 16 | 重复前一码长 3–10 次 | 3 位 |
| 17 | 重复前一码长 11–138 次 | 7 位 |
| 18 | 重复 0 码长 3–10 次 | 3 位 |
| 19 | 重复 0 码长 11–138 次 | 7 位 |

BC 码长本身用 4 位半字节写，带零游程转义：`15`（转义）+ `0` = 码长 15；
`15` + `n` = 连续 n+2 个 0。

### 6.3 LZSS 主符号（NC 表）

| 符号 | 含义 |
|---|---|
| 0–255 | 字面量字节 |
| 256 | 过滤器标记（6.7） |
| 257 | 重复匹配（用距离缓存 0 号 + 上一匹配长度） |
| 258–261 | 距离缓存索引 0–3 的匹配（长度见后） |
| 262–305 | 匹配，长度槽 = 符号 − 262（6.4） |

### 6.4 长度解码

```text
slot < 8:   长度 = 2 + slot
slot ≥ 8:   lbits = slot/4 − 1
            base  = 2 + ((4 | (slot & 3)) << lbits)
            长度  = base + 读 lbits 位
```

### 6.5 距离解码与缓存

```text
slot < 4:       距离 = 1 + slot
slot ≥ 4:       dbits = slot/2 − 1
                base  = 1 + ((2 | (slot & 1)) << dbits)
                dbits ≥ 4: 高 (dbits−4) 位直接读，
                           低 4 位用 LDC 表解码
                dbits < 4: 直接读 dbits 位
                距离 = base + 上述增量
```

**距离缓存**：4 项 LRU。重复距离符号（NC 257）与缓存匹配符号
（NC 258–261）读缓存并把它提到队首；新距离压入队首。

**长度奖励**（编码/解码一致）：

```text
距离 > 0x100    → 长度 +1
距离 > 0x2000   → 长度 +1
距离 > 0x40000  → 长度 +1
```

### 6.6 过滤器

NC 符号 256 触发过滤器定义：

```text
[块起始偏移] vint —— 相对当前写出位置
[块长度] vint
[过滤器类型] 3 位 —— 0 Delta / 1 E8 / 2 E8E9 / 3 ARM
[通道数 − 1] 5 位 —— 仅 Delta（1–4 通道）
```

- 过滤器作用于**解压输出**上的一个区域；同一区域可被多个过滤器按记录顺序
  链式处理（记录按产生顺序应用）。
- 区域上限 **0x3FFFF 字节**（256 KiB − 1）：RARLAB 读写器拒绝更大的
  过滤器区域，写入端按此拆分。
- 类型 4–7 显式报错（现实归档不存在，WinRAR 7.23 对齐）。

### 6.7 位序

压缩位流**高位在前**（MSB-first 逐字节打包）；Huffman 码与额外位同序。

---

## 7. 加密

### 7.1 密钥派生（链式 HMAC-SHA256 KDF）

等效 PBKDF2-HMAC-SHA256 但单链一次跑完：

```text
U0 = HMAC-SHA256(password, salt ‖ BE32(1))
U_i = HMAC-SHA256(password, U_{i−1})
acc = XOR 累积
```

单条 HMAC 链共 `2^strength + 32` 次迭代，XOR 折叠累积值在三个切点取样：

| 切点 | 产物 |
|---|---|
| 2^strength | AES-256 数据/头加密密钥（32 字节） |
| 2^strength + 16 | hash key（32 字节，MAC 加密校验和用） |
| 2^strength + 32 | 密码校验值（8 字节，XOR 折叠：`b[i] = T[i]^T[i+8]^T[i+16]^T[i+24]`） |

强度上限 2^24 次迭代（读取侧拒绝更大值，防 CPU DoS）。默认 2^15。

### 7.2 文件加密

- **算法**：AES-256-CBC；每个文件一个随机 IV（加密 extra 记录中）。
- **填充**：**zero-fill 到 16 字节块边界**（不是 PKCS7！7-Zip 会校验填充区全零）；
  空文件也产生 16 字节密文。
- **流式**：IV 链跨分块调用连续（分卷可在任意字节边界切开连续密文流，
  `CbcRangeEmitter` 只读-ahead 到块边界 + ≤15 字节 carry，卷大小仍精确）。
- 加密成员的头 CRC 是**密文 CRC**；明文 CRC 用 hash key MAC 保护（7.5），
  从而密文损坏必被检出。

### 7.3 密码校验值（12 字节）

```text
[0..8)    KDF 末段 XOR 折叠的 8 字节
[8..12)   SHA-256(前 8 字节)[0..4)
```

比较为常数时间。无校验值时（flags 无 0x0001）任何密码都"通过"。

### 7.4 头加密（-hp）

- 归档开头（每卷开头）写明文归档加密头（4.5，**无 IV**）。
- 其后**每个块头**为 `[16 字节随机 IV][AES-256-CBC(zero-fill) 密文]`；
  块内的 CRC/大小/类型/名字全部被加密。
- 密钥每卷重新派生（同盐），块 IV 各自随机。
- 限制（与 WinRAR 一致）：`-hp` 分卷不能用内联 RR（只能用 .rev）；
  QO 不与 `-hp`/分卷组合。

### 7.5 hash-key MAC

加密成员校验和的 MAC 化：

```text
MAC(CRC32)   = XOR 折叠 HMAC-SHA256(hash_key, CRC32_LE) 的 8 个 4 字节块
MAC(hash32)  = 同上逐 4 字节折叠成 32 字节
```

加密 extra 记录标志 0x0002 表示使用。解密端先还原明文 CRC/哈希再校验。

---

## 8. 多卷归档

- **命名**：`name.partN.rar`（N 从 1 起）；`discover_volumes` 自动发现全卷。
- **切分**：成员数据按卷切成数据块（DataChunk）；跨卷成员在旧卷写
  `DATA_CONTINUES` 数据块（非末块头带密文 CRC32），新卷开头写
  `DATA_CONTINUE_TO` 块头（末块头带 MAC 过的明文 CRC + 完整 extra 区）。
- **结束头**：非末卷带 `END_FLAG_NEXT_VOLUME`。
- **支持**：创建/读取/删除/重命名跨卷成员；**不支持**分卷 append、分卷锁定、
  分卷注释、分卷内联 RR（官方 rar 同样拒绝）。
- **恢复卷**见 9.2。

---

## 9. 恢复记录

### 9.1 内联恢复记录（RR，单卷）

RR 服务块数据区 = 若干 `{RB}` 恢复块，每个保护归档前缀的一个分片，校验
与奇偶按 **GF(2^16)**（Cauchy 矩阵）计算。

**`{RB}` 块布局**（固定头 0x48 字节 + 状态表 + 奇偶数据）：

```text
offset  size  字段
0x00    4     "{RB}" 标记
0x04    8     CRC64-XZ（覆盖 0x0c..total）
0x0c    4     total_size（LE）
0x10    4     header_size（LE）= 0x48 + data_shards×8
0x14    1     = 1（版本）
0x15    1     = 1（版本）
0x22    8     protected_size —— 受保护前缀字节数
0x2a    8     group_count —— 每分片字节数（偶数化）
0x32    8     shard_size（= total_size）
0x3a    2     data_shards（u16 LE）
0x3c    2     recovery_shards（u16 LE）
0x3e    2     shard_index（u16 LE，本块序号）
0x40    8×N   data_shard_states（CRC64 状态，每数据分片一个）
…       8     final_state
header_size … total_size：奇偶数据（group_count 字节）
```

**规划**：受保护前缀 ≥ 200 KiB 时 data_shards = 200（WinRAR 6.02 兼容上限）；
否则 `ceil(prefix/1 KiB)`。recovery_shards = `2·pct·data_shards/200`（≥ 1，≤
data_shards）。group_count = `2×ceil(prefix/(2·data_shards))`（偶数）。

**GF(2^16)**：本原多项式 `0x1100b`，域大小 65535；编码矩阵
`M[i][j] = inv((i + data_shards) ^ j)`（Cauchy）。奇偶字节 = 分片字节（16 位
符号）按矩阵行线性组合（XOR + 域乘）。修复 = 从幸存分片 + 奇偶解线性方程
（含矩阵求逆；奇异时报错）。

**修复**：找到损坏分片（CRC64 状态不匹配 / 数据与校验不符），在 GF 上解出
缺失字节；无损坏时原样返回。

### 9.2 恢复卷（.rev，REV5）

REV5 文件（`Rar!\x1aRev` 签名）为分卷集提供 Reed-Solomon 奇偶卷，可重建
缺失卷（`rar rc`）：

```text
[签名] "Rar!\x1aRev"（8 字节）
[头部 CRC32] 4 字节 LE，覆盖 [头部大小 + body]
[头部大小] 4 字节 LE
[body]:
  [版本] 1 字节 = 1
  [数据卷数] u16 LE
  [恢复卷数] u16 LE
  [本卷序号] u16 LE（= 数据卷数 + 偏移）
  [负载 CRC32] 4 字节 LE
  [每数据卷: 卷大小 u64 LE + 卷 CRC32 u32 LE]
[奇偶负载]……
```

- 数量：`NR = max(1, ceil(pct × ND / 100))`，上限 ND。
- 奇偶按同一 GF(2^16) Cauchy 编码；分片补齐到最大卷大小（偶数化）。

---

## 10. Solid 固态归档

- 主头带 `SOLID` 标志；文件头压缩信息带 solid 位。
- **编码器状态**（EncoderState）跨成员保持：lookbehind 窗口尾部、距离缓存、
  last-length、Huffman 表——连续压缩成员共享一个 LZ 窗口。
- **解码器状态**（DecoderState）同理跨成员保持。
- 目录/STORE/空文件**不参与** LZ 窗口（写端重置链）。
- solid 分卷同样支持（状态跨卷保持）。
- rar-rs：solid 与多线程压缩互斥（solid 保持串行）；WinRAR 无 rarfiles.lst
  时按名字/扩展名启发式排序，rar-rs 按参数顺序（已知小差异，互操作无碍）。

---

## 11. RAR7（v70）

### 11.1 关系

RAR7 不是独立容器：同一 `Rar!\x1a\x07\x01\x00` 签名、同一块信封/加密/恢复。
逐成员由压缩信息 version = 1 区分。> 4 GiB 的字典请求自动选择 v70；
2× 文件大小裁剪落回 ≤ 4 GiB 时自动降级 v50。

### 11.2 大字典编码

```text
字典 = (32 + dict_frac) << (dict_power + 12)
     = (128 KiB << dict_power) + (128 KiB << dict_power) / 32 × dict_frac
```

- dict_power 5 位（bits 10–14），dict_frac 5 位（bits 15–19）。
- 允许非 2 的幂；WinRAR `-md` 上限 64 GiB，但 1/32 增量位可把实际声明值
  推到略高于 64 GiB（rar-rs 编码端以 n ≤ 19 为界，即 ≤ ~126 GiB）。
- 编码端将请求字节数映射为 `n = floor(log2(bytes / 128 KiB))`（≤ 19）、
  `inc = min(31, (bytes − base) × 32 / base)`。

### 11.3 DCX 距离表

DC 表从 64 码扩到 **80 码**（HUFF_DCX）：距离槽上限 79 → DBits 38 → 距离
可达 ~1 TB。解码用 u64 运算（6.5 的公式不变，slot 上限放宽）。

### 11.4 与 rars 的差异（本实现为准）

- rars 解析压缩信息 bit 20（`rar5_compat`：v70 成员字典在 RAR5 范围内时置位，
  兼容旧解码器）。**rar-rs 不写该位，读取时忽略**（WinRAR 7.23 双向验证通过，
  不需要它）。
- 其余（v0/v1 字典公式、算法版本上限、solid/方法位）两实现一致。

---

## 12. 与 rars / rar-research 的差异清单

| # | 主题 | rars / rar-research | rar-rs（本文为准） |
|---|---|---|---|
| 1 | RAR7 压缩信息 bit 20 | 解析 `rar5_compat` 标志 | 不写、读取忽略（WinRAR 实测无碍） |
| 2 | RAR4 兼容 | 支持 RAR1.3–RAR4 | **明确拒绝**，报 unsupported（有测试锁定） |
| 3 | 过滤器类型 | 0–3 | 同 0–3；类型 4–7 显式报错而非跳过 |
| 4 | 块头 CRC | 覆盖原始 vint 字节 | 相同（非规范定宽 vint 必须原样参与） |
| 5 | 压缩块校验 | `0x5A ^ flags ^ size` | 相同 |
| 6 | 结束头块标志 | — | 写 `SKIP_IF_UNKNOWN`（7-Zip 兼容，WinRAR 同） |
| 7 | RAR7 字典公式 | `(frac+32) << (power+12)` | 相同（两式代数等价） |
| 8 | 距离表 | RAR5 64 / RAR7 80 | 相同 |
| 9 | 加密填充 | — | **zero-fill**（非 PKCS7；7-Zip 校验全零） |
| 10 | KDF | 链式 HMAC-SHA256 单链 | 相同（2^s / +16 / +32 三切点） |
| 11 | 内联 RR 分片 | 200 上限（WinRAR 6.02 兼容） | 相同 |
| 12 | SFX | 签名扫描 | 相同（首个 `Rar!…\x07\x01\x00`） |
| 13 | 分卷命名 | `name.partN.rar` | 相同 |

---

## 附录 A. 常量速查

```text
签名        Rar!\x1a\x07\x01\x00
REV5 签名   Rar!\x1aRev
块类型      0x01 归档 / 0x02 文件 / 0x03 服务 / 0x04 加密 / 0x05 结束
块标志      0x0001 extra / 0x0002 data / 0x0004 skip / 0x0008 cont
            0x0010 cont-to / 0x0020 depends-prev / 0x0040 preserve-child
归档标志    0x0001 volume / 0x0002 vol-num / 0x0004 solid
            0x0008 recovery / 0x0010 locked
文件标志    0x0001 dir / 0x0002 time / 0x0004 crc32 / 0x0008 unknown-size
压缩信息    0–5 version / 6 solid / 7–9 method / 10–14 dict / 15–19 frac
方法        0 store / 1–5 级别
OS          0 windows / 1 unix
extra 记录  0x01 加密 / 0x02 哈希 / 0x03 时间 / 0x04 版本 / 0x05 重定向
            0x06 owner / 0x07 服务数据
重定向类型  0x01 unix-link / 0x02 win-link / 0x03 junction / 0x04 hardlink
            0x05 file-copy
Huffman      NC 306 / DC 64 / DCX 80 / LDC 16 / RC 44 / BC 20；码长 ≤ 15
压缩块      0x5A 校验种子；编码端分块上限 128 KiB（0x20000，格式本身允许更大）
过滤器      0 Delta / 1 E8 / 2 E8E9 / 3 ARM；区域 ≤ 0x3FFFF
加密        AES-256-CBC；盐 16 / IV 16 / 校验值 12；KDF 2^15 默认
GF(2^16)    本原多项式 0x1100b；Cauchy 矩阵 M[i][j] = inv((i+ND) ^ j)
```

## 附录 B. 参考来源

- **权威**：rar-rs 实现（`crates/rar/src/rar50/`、`codec/rar50.rs`、
  `crypto/rar50.rs`、`recovery/{rar5,rev5}.rs`），WinRAR 7.23 双向互操作验证。
- 交叉核对：bitplane/rars（`crates/rars/src/rar50*.rs`）与
  [rar-research](https://github.com/bitplane/rar-research) 规格书；
  冲突处以本文（rar-rs）为准。
- 位流格式源自对 libarchive `archive_read_support_format_rar5.c`
  （Grzegorz Antoniak, 2018, BSD-2-Clause）的独立分析。
