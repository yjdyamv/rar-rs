# ADR 0001: RAR4 Creation Architecture

- Status: accepted (Phase 1 landed 2026-09; amended 2026-09)
- Date: 2026-09-05 (original), 2026-09 (amendment)
- Related: [ADR 0005](0005-rar4-edit-architecture.md) (edit side)

rar-rs 将增加 RAR3/4（unp_ver=29）归档创建功能。核心决策：

1. **独立写模块（原 `rar40/write.rs`，现 `format/rar4/write.rs`）**：不抽象 trait，RAR4 和 RAR5 写逻辑差异太大（固定宽度头 vs vint、16-bit CRC vs 32-bit、不同的多卷标志），独立模块更清晰，与 rars 架构一致。

2. **编码器写入 `Vec<u8>` 缓冲区**：编码器输出压缩数据到内存缓冲，写管线统一处理加密 + 多卷切分 + 写盘。RAR4 加密在成员级别（每个成员独立 salt + IV），需要在编码完成后、写盘之前加密。

3. **成员边界切分多卷**：RAR4 多卷用 `FHD_SPLIT_BEFORE` / `FHD_SPLIT_AFTER` 标志，切分点在成员边界（一个成员不能跨卷）。剩余卷空间填零，下一卷开头写 `FHD_SPLIT_BEFORE`。

4. **从 rars 移植 Unpack29Encoder**：编码器是确定性算法，格式必须与 WinRAR 字节级兼容。rars 的实现已过充分测试。

5. **分阶段实现**：Phase 1 做 STORE + LZSS（m1-m5），Phase 2 加 PPMd。注意 `-m0` 是 STORE（WinRAR 定义），PPMd 在 `-m4/-m5` 参与候选竞争，见 `docs/rar4-creation-spec.md`。

## Considered Options

- **Trait 抽象** (`WritePipeline` trait)：被拒，因为 RAR4/RAR5 写逻辑差异太大，trait 会变成最小公倍数接口。
- **复用 RAR5 写管线**：被拒，因为头格式完全不同（vint vs 固定宽度），强行复用会引入大量条件分支。

## Consequences

- `rar40/` 目录从只读扩展为读写。
- `archive/create.rs` 按 `rar4` 标志分发到不同的写路径。
- `CreateOptions` 新增 `format_version` 字段。
- CLI `-ma4` 开关从拒绝改为接受。
- ~~RAR4 不支持：QO、RR、BLAKE2sp、头加密（-hp 后续迭代）~~ **Amended 2026-09**：RR 与 `-hp` 后续均已实现（见 `PLAN.md`「RAR4 写侧 Tier 2 全闭」与 [ADR 0005](0005-rar4-edit-architecture.md)）；仍不支持的只有 QO、BLAKE2sp 与 `.rev` 恢复卷。
