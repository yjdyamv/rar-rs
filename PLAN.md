# rar-rs 后续计划（TODO）

随意记录，想到哪写到哪。完成一项就划掉。

## 已完成 ✅

- **ENDARC block flags 修复**（dd5b456）：分卷集的 7-Zip 互读（"data after the end of archive" 根因）
- **加密分卷每块加密记录**（dd5b456）：非末块 flags=1（密文 crc32）、末块 flags=3（MAC），与 WinRAR 字节级一致
- **分卷 + header 加密（-hp）**（d8f0f63）：每卷开头明文加密头 + [IV][AES-CBC] 块；精确卷大小（目录项配额记账修复）
- 多卷 + 密码不再静默失效（binding 侧透传，smart-archive-rar 0.3.3/0.3.4）

## P4：>4 GiB 单文件创建 RAR（最大的一块）

现状：`add_file` 压缩路径把整个文件读进内存（binding 硬限制 4 GiB/文件、32 GiB/总输入）。RAR 分卷的典型场景恰恰是大文件，4 GiB 卡脖子。

要做的事：
- `write_file_entry` 压缩路径流式化：分块读 → 压缩（encoder state 跨块保持）→ 加密（CBC 跨块续链）→ 按 volume_size 切分
- 参考 `write_stored_file` 的流式骨架，但它是 STORE（明文/不压缩）；压缩态需要 LZ 窗口状态跨块管理（solid 链已有 encoder_state 可以复用思路）
- 加密态：`encrypt_payload_with` 是整段 buffer 加密，需要流式 CBC（`Aes256Cbc` 能就地分块续链，iv 会更新，应该可行）
- 分块 CRC：非末块 = 密文块 crc32（已有约定），流式时边写边算
- 内存边界：`max_unpacked_bytes` 之类的防护要重新梳理；binding 的 `MAX_FILE_BYTES` 限制可以放开（或改成可配置）
- 测试：>4 GiB 稀疏文件 round-trip + 7zz/WinRAR 验证（注意 CI 磁盘空间）

## 其他已知缺口（按优先级）

- **读取端支持 -hp 分卷**：`scan_all_volumes` 遇到加密头还报 "not yet supported"（line ~1993）。写入端已经能产出 -hp 分卷集（7zz/WinRAR 验证过），读取端只有自己读自己才需要——rar5-modify 流程暂不涉及分卷，优先级低，但做了之后 rar-rs 的 -hp 分卷可以自 round-trip
- **solid + 分卷**：create 时拒绝（`opts.solid && volume_size`）。WinRAR 支持 solid 分卷；需要 encoder_state 跨卷保持（P4 的流式化做完后会更顺）
- **分卷 append**：拒绝（官方 rar 也拒绝，保持现状即可）
- **分卷 lock / 注释**：不支持，官方 rar 对分卷 lock 也有限制，保持现状
- **interop 测试**：`tests/interop.rs` 是 unix-only，Windows 本地编译不过（CI 只在 Linux 跑）。可以考虑加 `#[cfg(target_os = ...)]` 拆分或补一个 Windows 能跑的互操作测试

## 备注

- 分卷 + 内联恢复记录（-rr）WinRAR 本身不支持（分卷只能用 .rev），rar-rs 的拒绝是对的，别"修"掉
- 卷大小精确性：WinRAR 卷 = 精确 volume_size；rar-rs 现在也精确（-hp 修复时顺手修了目录项记账）。以后加新块类型（QO 等）时注意配额要一起算
- 加密块 padding 是 **zero-fill** 不是 PKCS7，7-Zip 会检查 padding 区全零，别改
