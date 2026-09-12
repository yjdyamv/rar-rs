# rar-rs 计划

完成一项勾掉一项。本文件只留**结论**与**警告**；验证过程和工程细节见 git 历史（旧版详单：`git show d9201cf:PLAN.md`）。已落地的加固记在文末「加固记录」，不再单独维护 CHANGELOG。

## 现状

- RAR5 创建/读取全功能对齐 WinRAR 7.23：压缩、`-hp` 头加密、分卷、solid、内联恢复记录、`.rev` 恢复卷、quick-open、NTFS ADS、三时间戳、owner；另含 RAR7 (v70) 读写、`-mt` 多线程压缩、长距离匹配。
- **最优解析**（m2-m5 全部）：rars 移植的 forward shortest-path DP（每块收集一次匹配、按上一 pass 的 Huffman 表重新定价 3 次、LZMA BT4 tree finder 全程 + 历史种子、BlockSplitter 按字节分布切 64-128 KiB 块、每块独立表）。m3（默认）vs WinRAR 7.23：生成代码 -52%、XML -24%、文本 -13%、DLL +2-6%（此前 +3-23%）；m2 在 DLL 上已反超 WinRAR。所有输出 WinRAR 字节级可解。
- **MT 低步数解析（2026-09，issue 13 定论）**：`-mt` worker slice 走独立 hash-chain 低步数搜索（`mt_slice_symbols_low_step`，链预算 16，WinRAR m3 式），不再跑 seq 的最优解析——砍每位置步数 ~5x 以兑现 mt8 带宽悬崖，MT 输出进一步偏离 seq（文档化接受的分歧）。mt8 m3 实测：tsc x86 2791→1569 ms（ratio +2.43pp）、repeated text 1075→238 ms（+0.49pp）；random ~0.34x（STORE 兜底）。seq 与最优解析字节级契约不动。
- **自适应发射块大小（2026-09）**：解剖 WinRAR 7.23 符号流（analyze_stream 工具）定位到 text64 压缩率差（12681 vs 8769 B）纯粹是每块表开销——WinRAR 在分布稳定数据上整成员一块，我们硬限 128 KiB。发射块现在合并到 4 MiB，符号流在 64 KiB 子跨度间的局部字面量/距离/长度分布漂移时提前闭合（异构二进制——DLL 节、XML——保持小块，对齐 WinRAR 的 ~64 KiB DLL 块）。解析本身不变（仍 64-128 KiB，DP 内存有界）。实测 m3 seq：text64 12607→6058（-52%，赢 WinRAR 8696）、dll -0.5%、mixed -640 B、xml +157 B（+0.18%，记录在案）、sparse/random 不变
- **持久树跨 chunk 损坏修复（2026-09）**：验证新块大小时发现既有静默损坏——窗口跨 chunk 增长时 `grow_to` 用新零数组替换 son（head 表幸存），而 0 是到位置 0 的合法链接；`rebase` 同类缺陷（链接只改值不迁槽）。密集 x86 成员产生抄 MZ 头的假匹配，unrar 与 WinRAR 双双报 checksum error。修复：grow_to 复制旧链接、rebase 把链接迁移到新环槽、收集器在解析定价前逐字节验证每个树报告（未来任何不变量破坏的安全网）。回归：129 KB 真实内核镜像前缀（旧代码在 129,334 字节处损坏，dict 2^3 + 64 KiB chunk）现字节级回环。DLL 自动 x86 过滤器产物 5.75 MB（43.90%），赢 WinRAR 7.23 的 5.87 MB（44.81%），双向完整性校验通过
- **自动 x86 过滤器**：rars 移植的结构扫描（E8/E8E9 簇/跨度检测）自动应用于内存路径成员；过滤器成员按非 solid 写出。修复了 solid 链中过滤器位置的流式绝对/成员相对语义（unrar `WrittenFileSize` 为成员相对、区域定位为流绝对、E8 偏移按 16 MiB 取模）。
- 命令面：官方 rar 全部命令（含 `rv` 补恢复卷、`lb/lt/vb/vt` 列表变体）。
- **老容器族读取（RAR 1.5–4.x，2026-09 起）**：`Rar!\x1a\x07\x00` 容器族（RAR 1.5–4.x）读取——块扫描/文件头（unicode 名、DOS 时间、salt、字典位）、STORE 直通 + **三代解码器全覆盖**：RAR3/4（unp_ver ≥ 29）LZSS+Huffman + PPMd 变体 H + 五大标准 VM 过滤器（E8/E8E9/Itanium/Delta/RGB/Audio）、RAR 2.x（20/26，LZSS+Huffman + 音频块）、RAR 1.5（15，自适应 Huffman 老 LZ）——（`codec/legacy/rar29.rs`+`codec/legacy/ppmd.rs`+`codec/legacy/rar20.rs`+`codec/legacy/rar15.rs`，rars 解码半移植）含 RAR3/4 solid 链共享窗口 + **分卷（`.partN.rar` 新命名 + `.r00/.r01` 老命名，任意卷入口，split 成员合并为多 chunk）** + **`-hp` 头加密（主头 MHD_PASSWORD 后每块 `[8B salt][align16 密文]`，列表即需口令）** + RAR15/20/30 全代数据解密。对 WinRAR 5.91 `-ma4` 夹具与 rars 语料（真 RAR 3.0/2.0/2.5/1.5.4：PPMd、六种过滤器、solid-PPMd、PPMd 内嵌过滤器、音频 WAV、17 文件 doc 集、RAR20/RAR15 老密码、2–5 卷 split 集、-hp 含中文名/分卷）字节级通过（CRC 门）。WinRAR 5.x/7.x 的 RAR4 写器不再产 PPMd，老格式夹具只能来自 rars 语料（现代 WinRAR 连 RAR2.x/1.5 也不能产）。非标准（通用）VM 程序仍不支持。生成夹具工具（人工验证）：WinRAR 5.91（历史夹具）/6.23（最后的 -ma4，与 5.91 产物等价）；7.23 无 -ma4 不能产 RAR4。
- **RAR4 写侧 Tier 2 全闭（2026-09）**：`-hp` 头加密写侧（主头 MHD_PASSWORD + 每块 `[8B salt][AES-128-CBC]`，64e280e）；PPMd 编码（rars 编码半：order-8/25 MiB 模型 + hybrid LZ-escape tokeniser，m4/m5 文本比 LZ 小 46%，9631d18）；NEWSUB 0x7a 恢复记录写+修（逐字段复刻 6.23，双向互修字节一致，0e65b44）；六大标准 VM 过滤器写侧（E8/E8E9/Delta/Audio 自动探测 + RGB/Itanium 编码能力；复用 RAR5 filters.rs 变换，fa4c038/f16c8b4）；**solid 链 PPMd 模型延续**（LZ levels 与 PPMd model 独立链状态、赢者推进、PPMd 赢回滚 LZ 表；ppmd 字段 Box 化避免 1 MiB 主线程栈溢出；-s 文本树自动建/续模型，cce4e15）。每项均有 WinRAR 6.23 双向逐字节互操作测试（7.23 无 `-ma4`，不能自产或修复 RAR4，只能作读取校验）。已知边界：solid 链内 filter（窗口=变换字节语义,我们 LZ 上收益有限,留后续）、solid 归档 MT。
- **RAR 1.5/2.x 写侧（2026-09 Phase 1）**：版本表收敛只读轴后把 rars `Unpack15Encoder`/`Unpack20Encoder` 原样移植（`codec/legacy/rar15_encoder.rs`/`rar20_encoder.rs`；错误映射 `RarError::Format`/`Cancelled`，MatchFinder 按 rar29_encoder 模式适配；rar20 保留自含 `Rar20MatchFinder` + 音频块编码，rar15 保留自含 `Rar13MatchFinder` + stmode）。`is_writable()` 扩为 `{v15, v20, v29, v50, v70}`；v26（同 v20 codec）/v36（同 v29 codec）仍只读。写管线新 `encode_rar4_member` 按 `WriteState.rar4_unp_ver` 分派（15/20/29），成员头 `unp_ver` 字段参数化（单卷/多卷两条写路径），repack/repair 寄居路径仍 v29-only。选件梯与 rars `write.rs` 逐字一致（rar20：candidates 16/64/256/512/1024 + lazy + lookahead2 + 最优解析 m4/m5 + 音频 m2–m5；rar15：old_distance/stmode/maxdist 4–24 KiB 五档，全 lazy off）。**v15/v20 写侧（2026-09 全能力）**：solid 链（`build_legacy_solid_encoder` 首成员 level 建持久编码器，跨成员续表/续窗，归档级 MHD_SOLID、STORE 断链）与 `-p`/`-hp` 加密（`rar4_member_encrypt` 按代分派：15=RAR15 流 XOR 无盐无 pad、20=RAR20 块密码 16 对齐无盐、29=RAR30+盐；仅 29 置 `FHD_SALT`，v15/v20 只置 `FHD_PASSWORD`；`-hp` 头加密读写两侧统一 AES-128 `Rar30Cipher`，与成员版本无关）均已支持；官方 unrar 7.23 双侧 `t` 通过（solid 文本档 `x` 字节一致、v20-pw/v15-hp 均 All OK）。CLI `-ma2`→v20、`-ma15`→v15（默认 `-ma4` 保持 v29），napi format `rar2`/`rar15`。验证：roundtrip 全级别（v15/v20 × m1–m5 × 文本/随机/音频扫频，音频成员校验触发 try_audio）、solid 与加密多卷 CLI 回归、`SA_OFFICIAL_UNRAR` 门控 `unrar t`。
- 工程：fmt/clippy `-D warnings` 双门（本地，含测试目标）、五目标 fuzz（`fuzz/`）、取消钩子、QO 快路径 `open_quick`、流式修复 `repair_archive_path`、零填充分卷集支持。
- 架构：workspace `crates/rar`（库 crate `rar-rs`）+ `crates/rar-cli`（rar/unrar）+ `crates/rar-napi`（native/WASI binding），按 rars 分层——词汇见 `CONTEXT.md`，格式细节见 `docs/FORMAT_RAR5_RAR7.html`。
- **已废弃 API 面移除（2026-09，Phase 6 收尾）**：全仓库不再有 `#[deprecated]` 标注；`RarArchive` 写侧方法（`create_with_options`/`add`/`add_as`/`add_bytes`/`add_directory_only`/`add_batch`/`close`）降 `pub(crate)`，读侧 `list`/`get_entry`/`namelist`/`read`/`extract`/`extract_all` 与事务方法/`lock` 已删除。所有期货已迁到角色门面：`ArchiveWriter`（`create_with`/`append`/`add_path`/`add_bytes`/`add_directory`/`add_redirect`/`add_batch`/`finish()`，消费 self）+ `ArchiveReader`（`unique_entry`/`entries[_named]`/`read_entry[_with_options]`/`copy_entry_to[_with_options]`/`extract_entry`）+ `ArchiveEditor`（`delete_entries`/`rename_entries`/`apply(EditPlan)`/`set_comment`/`set_recovery`/`lock`）。**测试共 31 文件 2600+/2760− 行改**，examples/fuzz 同步；`RarArchive` 保留构造器（`open`/`open_quick`/`open_with_password`/`open_quick_with_password`/`open_append`/`open_append_with_password`）、配置（`set_password`/`set_dictionary`/`set_compression_threads`）与钩子（`set_cancel_flag`/`set_progress_callback`/`set_progress_total`）供有绑定兼容需求的调用方；读侧便捷方法都在 `ArchiveReader` 上。**迁移中两处语义回归已修**：① 旧 `read(name)` 对重名成员取第一个，新 `unique_entry` 报 `AmbiguousMember`——重名场景改用 `entries_named(name).next()`；② v70 非 2 的幂小字典已解锁（见「已完成」首个条目）；此前非幂 1/32 增量位覆盖仅在 >4 GiB（`-md8g`/writer 单测）。**验证**：`cargo check --workspace --all-features --all-targets` 零 error、`cargo clippy --workspace --all-features --all-targets -- -D warnings` 全绿（含全部测试 target）、`cargo test --workspace --all-features` 全过（rar-rs 247 lib + 17 集成 + rar-cli 58+37 incl. 本机 WinRAR 互操作 + rar-napi 3）、fuzz 独立 workspace `cargo check` 通过。15 个测试文件的 `#![allow(deprecated)]` 已全部移除（库内已无 deprecated 项，属惰性标记）。逐文件迁移记录见 git 历史。

## 独立审计 2026-09-11（不看 backlog 的优先级）

一次“假设没有 PLAN/issues”的健康与风险审计的结论。做法：全量测试 + 真跑五个 fuzz 目标 + 逐行读三条最危险路径（写、读/解、恢复/加密）。全量测试绿（lib 266、CLI 62、WinRAR 互操作 32），但发现的问题大多不在本文件里，而且比压缩性能更该先做。**[读码确认]** = 逐行读过源码；**[待复现]** = 尚未写成失败测试。**P0/P1 已于 2026-09-11 修复（见各条“已修”）；P2：1/3/4/7 已修，2/5/6 记录理由。**

### P0（现在做，成本 S，影响大）

- **fuzz 工程缺 `raw` → 五目标全部失效、CI 的 fuzz check 必红** **[读码确认，已实测]**：`fuzz/Cargo.toml` 依赖 `rar-rs = { features = ["parallel","simd"] }`；`raw` 门控（见「为什么要有 `raw` feature」那次改动）只给 `crates/rar` 的 dev-dep 补了 `raw`，漏了 fuzz。`cargo check --manifest-path fuzz/Cargo.toml` 报 9 个 E0433/E0425/E0603，即 `.github/workflows/CI.yml` 第 58 行必红。补 `raw` 后 5 个目标（parse/crypto/recovery 各 20k、write/rewrite 各 2k）全部无 panic 通过。修：1 行 + CI 加真跑。 **已修（2026-09-11）**：`fuzz/Cargo.toml` 补 `raw`；CI 增 `Fuzz smoke` 步骤（parse/crypto/recovery 5k + write/rewrite 500）；`cargo check --manifest-path fuzz/Cargo.toml --all-targets --locked` 与五目标短跑均通过。
- **两处恶意归档可触发的 panic（进程 abort；napi/WASM 绑定同样暴露）** **[读码确认，待复现]**：
  - `crypto/rar50.rs:619`：`let rec_end = offset + rec_size as usize;` 未检查；`rec_size` 是 vint（可达 `u64::MAX`），release 回绕后 `&extra_data[offset + tn..rec_end]` 出现 start>end → panic。任何带 extra 的文件头都会进 `read_packed`。
  - `recovery/legacy.rs:173-175`：只用 `header.get(tail..tail+8) == Some(b"Protect+")` 证明 8 字节存在，随即索引 `tail+8..tail+16`；`name_size = header[26..28]` 攻击者可控（`head_size=54, name_size=14` 即越界）。`rar r` 与 RAR4 create/edit 的 `scan_protect*` 都可达。
  修：`checked_add`/`get` + 两个单测。 **已修（2026-09-11）**：加上边界检查；复现测试 `crypto::rar50::tests::hostile_extra_record_size_does_not_panic`、`recovery::legacy::tests::crafted_rr_name_size_does_not_panic`。
- **缓冲读 / `t` / 并行提取缺字典上限（分配型 DoS）** **[读码确认，待复现]**：`format/rar5/extract.rs:1410 decode_file_at`（及并行 worker）没调 `member_dict_window`（`decode_file_to:1483` 调了）；`codec/modern/lzss_huff/decoder.rs:210 checked_dict_size` 对 `dict_size_bytes` 无上界 → `codec/common/window.rs:20 vec![0u8; size]`。伪造 v70 头声明 ~2^48 B 即可让 `test`/`read` 分配失败 abort（streaming 路径有 4 GiB `-mdx` 上限，此处没有）。修：两处补 `member_dict_window` + 测试。 **已修（2026-09-11）**：抽出 `capped_dict_bytes`，`decode_file_at` 与并行 worker 共用；测试 `rar50_roundtrip::buffered_read_enforces_the_dictionary_cap`（1 字节 cap 拒绝、默认 cap 可读）。

### P1（静默产出坏档案 / 回归）

- **RAR4 >4 GiB 成员被 `as u32` 静默截断进头** **[读码确认]**：`format/rar5/write/mod.rs:901,987`（单/多卷）与 `:3829,3904`（并行）把 `packed_size`/`unpacked_size`/`chunk_size` 直接 `as u32`；`add_file_rar4` 与 `validate_rar4_only` 都没有大小守卫。RAR4 尺寸字段本就是 32 位，正确行为是**拒绝**而不是写出头尺寸与载荷不符的归档。 **已修（2026-09-11）**：`format/rar4/create.rs::ensure_member_size` 在 `add_rar4_data` 入口拒绝 >u32::MAX，带单测。
- **`-v` 过小 → 卷循环零进展、无限建文件** **[读码确认]**：RAR5 `write/mod.rs:2520-2524`（`bytes_for_data == 0` 时 `start_next_volume(); continue;` 且不推进 offset）、RAR4 `:958-969`（剩余 ≤7 时同样死循环）。目前只拒绝 `volume_size == 0`，`rar a -v50 big.rar` 会挂住并写满磁盘。修：最小卷大小校验。 **已修（2026-09-11）**：两处 split 循环加“一次 roll 无进展即报错”守卫（RAR5 `rolled` / RAR4 同样）；测试 `archive_writer::tiny_volume_size_is_rejected_instead_of_looping`（`volume_size(16)`）。
- **流式 RAR5 修复校验弱于其缓冲孪生** **[读码确认]**：`recovery/rar50/`（`stream.rs`）的交叉校验少了 `data_shard_states` 项（缓冲版 `repair.rs` 有），且流式解完不按 `first.data_shard_states` 校验 CRC64 → 含两代 RR 块的文件可能解出“貌似合理但错误”的字节，写进 `fixed.*` 并报 Repaired（CLI 只用 `RarArchive::open` 验头）。修：2 行 + 解后校验。 **已修（2026-09-11）**：流式路径补上 `data_shard_states` 交叉项，并在返回前按 `first.data_shard_states` 校验每个解出的 shard；单测 `rar5_inline_recovery_rejects_mismatched_generations`。
- **RAR4 多卷不预留 FILE_HEAD** **[读码确认]**：`write/mod.rs:966-967` 只减 7（EOA），`emit_segment` 写头+数据 → 每卷超出 `-v` 约一个头长，`-v` 契约失效。 **已修（2026-09-11）**：split 预算改为 `7 + FILE_HEAD（32+名+盐+exttime，`-hp` 含加密块）`；测试 `rar4_create::rar4_multivolume_volumes_do_not_exceed_the_requested_size`。
- **截断/损坏 `.rev` 使重建 panic** **[读码确认]**：`recovery/rev50.rs:266`（`data[16 + hsize..]`）与 `:298`（`&payload[start..start+want]`）无边界检查；`rar rc`、`rebuild_missing_volumes`、napi 均可达。 **已修（2026-09-11）**：`data[16+hsize..]` 与 `payload[start..start+want]` 改为 `get` + 报错；测试 `rar50_roundtrip::truncated_recovery_volume_errors_instead_of_panicking`。

### P2（健壮性 / 覆盖率 / 发布）

- create 路径 quick-open 只缓存文件头（`write/mod.rs:2334,2800`），目录/重定向不缓存，而 append（`archive/mod.rs:783`）与 rewrite（`archive/transaction.rs:1195`）全缓存 → `open_quick`/`list_entries_quick` 少列成员。**[待复现]** **已修（2026-09-11）**：redirect/dir_only/dir 三个直接写头处补 QO 缓存；测试 `quick_open_listing::open_quick_lists_directories_like_the_full_scan`。
- STORE 成员先 `hash_file` 再重读同一路径（`write/mod.rs:321` vs `:334,2810`），只比字节数 → 同尺寸改写真会写出旧 CRC/BLAKE2。**[待复现]** **未修（接受）**：单遍 STORE 必须先写头再流式，回填头需要 patching（`-hp` 还要重加密），影响面大于收益；记为已接受的竞态。
- Windows STM 流名（`extract.rs:1020`）是唯一没过 `sanitize_archive_path` 的命名记录（是否真能逃出目标目录未在 Windows 实测）。**[待复现]** **已修（2026-09-11）**：`valid_stream_name`（只允许单个前导 `:`，禁分隔符/保留字符）+ 单测；`write_windows_stream` 拒绝非法名。
- 未被真实夹具覆盖的 crypto 分支：RAR3 慢 KDF 的单测是同义反复（`crypto/rar30.rs:288-301`，长度 `<64` 时 `update_password_data_sha1` 分支根本不跑）、RAR20 >16 字节口令链只有 8 字节口令覆盖。**[待复现]** **已补覆盖（2026-09-11）**：用本机 Rar 6.23 生成 49 字符口令的 `-ma4 -p` / `-ma4 -hp` 夹具（`rar40/encrypted/rar4_longpw_{p,hp}.rar`）+ `rar4_read` 两测试，外部验证 RAR30 慢 KDF 分支；`-ma2 -p<20 字节>` 由我们写、由 UnRAR 读出（`winrar_interop::we_create_rar2_long_password_members_winrar_valid`），覆盖 RAR20 >16B 密钥链。
- legacy 修复对“最后不满 512 B 的扇区”报 "All OK" 却不修（`recovery/legacy.rs:252-265`）。**[读码确认]** **未修（记录）**：尾扇区写侧对零填充算 tag，本就无法校验真实字节，无法检测其损坏；只能改措辞或接受该未保护区。
- 恒真/自比校验（可顺手删）：`recovery/rar50/`（恒真/自比比较）。**[读码确认]** **未修（低价值）**：纯恒真比较，不影响行为。
- **发布就绪**：`rar-cli` 因 workspace path 依赖缺 version 无法打包（`cargo package -p rar-cli` 实测报 “does not specify a version”）；三个 crate 都没有 `readme`/`keywords`/`documentation`；SPDX/逐文件来源审计仍未闭环。 **部分已修（2026-09-11）**：workspace 依赖补 `version`，`rar-rs` 增 `readme`/`documentation`/`keywords`/`categories`，`cargo package -p rar-rs` 打包并验证通过（含 README）；`rar-cli` 现在只因 `rar-rs` 未发布而无法解析，属发布顺序。SPDX 仍需法务。**逐文件出处清单已完成（2026-09-11）**：`THIRD_PARTY_LICENSES.md` 新增「File-level `rars` port notices」表（每个 rars 移植文件的 in-file 声明），并给缺失声明的 `crypto/rar50.rs`、`recovery/rar50.rs` 补了出处头（NOTICE 同步）。剩下明确为两项法律裁定：① rars workspace metadata（MIT OR Apache-2.0）vs 后来 COPYING（WTFPL）的冲突（即解码侧 WTFPL / 编码侧 MIT OR Apache-2.0 的来源）；② `recovery/legacy.rs` 声明了 rars 移植但无许可行。

**与 backlog 的差异**：以上 P0/P1 基本都不在本文件原有条目里——文档把我引向 BT4 压缩性能深挖（已证否），而真正的风险是“CI 已红 + 两处 panic + 字典 DoS + 几个静默产坏档的守卫”。建议先按本节 P0/P1 排序推进，压缩性能线（issue 09/04）暂缓。

## 技术债（2026-09 审查的未闭环项）

`docs/CODE_AUDIT_2026-09-05.md` 已删除（一次性基线，结论归到这里）。仍未闭环的：

### 为什么要有 `raw` feature

它存在的唯一目的：**让内部实现可以自由重构，而不背 SemVer 破坏性变更的包袱。**

Rust 的 SemVer 规则是硬的 —— 只要一个 `pub` 项能从 crate 根到达，改它的路径 / 名字 /
签名就是破坏性变更。`#[doc(hidden)]` 不算数：它只是不显示在文档里，项照样可达。
`raw` 把 `format` / `recovery` / `crypto` 三棵树从「默认可达」变成「默认不可达」：

- 默认用户（CLI、napi、下游）只能依赖受支持的那一层 → 内部结构可以随便改；
- 需要 wire 级访问的人（**目前只有我们自己的测试**）显式开 `raw`，自己承担破坏风险。

代价（都只在 dev 图里，不污染下游）：`crates/rar` dev-depend 自己来给测试图开 `raw`
（是个 workaround，为的是 `tests/support` 能继续跨 `read_block` 这个 seam 而不去
重新实现块信封）；以及 `format` / `recovery` / `crypto` 三处
`allow(dead_code, unused_imports)` —— 默认配置下那批代码是死的，只为 `raw` 存在。

**什么时候该删掉它**：如果确定这个库永远只服务我们自己的 CLI 和 napi、不会有外部代码
`use rar_rs::format::…`，那 `raw` 就是纯仪式，可以把三个模块直接降 `pub(crate)` 并删除
feature。代价是 `tests/support::scan_blocks` 不能再跨 seam —— 要么自己实现块信封解析
（会漂移），要么 11 个依赖它的测试目标另想办法。

- **热点文件拆分（2026-09 完成）**：`format/rar5/write/mod.rs` 已拆为 `write/{mod,add,emit,stream,batch,engine,layout}.rs`
  （只含 RAR5，mod.rs 22 行门面），RAR4 编排回到 `format/rar4/write/{mod,pipeline,cbc}.rs`，格式中性写机制在
  `format/shared/{write_ops,engine,stream}.rs`；`codec/modern/lzss_huff/{encoder,decoder}.rs`（3940/1736）拆为
  `encoder/{mod,chunked,parse,emit,filter,tests}.rs` 与 `decoder/{mod,engine,analysis,tables,tests}.rs`
  （mod.rs 共享词汇 + 角色模块，公开路径与输出不变）；`archive/rar4_edit.rs`（2924）拆为
  `archive/rar4_edit/{mod,layout,headers,comment,engine,repack}.rs` + `tests/`（五个用例文件）；
  `recovery/rar50.rs`（2141）拆为 `recovery/rar50/{mod,plan,gf16,encode,repair,stream}.rs`（rars 移植核心保留在 `gf16`）。
  剩余大文件：`codec/legacy/rar29_encoder.rs` 2747（按 CONTEXT 是 rars 移植的
  逐文件隔离，拆分收益低）。
- **双 options 面（2026-09 收敛，ADR 0006）**：`WriterOptions`（私有字段 builder）是唯一公开构造器；
  `CreateOptions` 已降为 `pub(crate)` 并从 crate 根移除（此前是零公开入口的死类型）。组合规则仍由
  `options::validate_combinations` 共用，`CreateOptions` 不静默丢弃/钳制（quick-open×分卷或 -hp、
  内联 RR×分卷、rv 无分卷、recovery>100、`volume_size=0`、-hp 无口令同样报 `InvalidOption`）。
- **双 API 收敛（2026-09，ADR 0006）**：`ArchiveReader`/`ArchiveWriter`/`ArchiveEditor` 是唯一受支持
  公开面。`RarArchive` 从 crate 根移除并 `#[doc(hidden)]`，仅留 `rar_rs::archive::RarArchive` 作为
  字节比对测试语料/内部委托的兼容路径（完整删除留待下一个破坏性版本）。CLI/N-API/examples/fuzz 已全部
  迁到角色面；补齐 `ArchiveReader::comment()`（原只有 `RarArchive::get_comment`）使读侧无缺口。
- **老编码器去重（2026-09）**：`rar20_encoder`/`rar15_encoder` 各自私有的 BitWriter 删除，统一
  `codec::common::bitstream::BitWriter`（同为 MSB-first，`finish()`→`into_bytes()`，rar15 的 usize
  位宽调用点改 u8）；新增 `codec/legacy/tables.rs` 共享 RAR20/29 完全相同的 LENGTH 槽表，OFFSET 表因
  槽数不同（48 vs 60）各留副本。解码器与 match finder 按 rars 逐文件隔离设计不动。验证：全量测试 +
  本机 WinRAR 互操作（35/6）字节不变。
- **审查修复（2026-09，三项正确性 + 一项契约）**：① RAR4 目录头滚动循环补 `rolled` 守卫
  （`format/rar4/write/pipeline.rs`，`-v` 过小不再死循环，报 `InvalidOption`）；② RAR5 目录/重定向
  头新增 `ensure_rar5_volume_space`（预留 EOA、必要时滚卷、装不下报错），并补上重定向头缺失的
  `volume_bytes_written` 记账；③ 删除 `ArchiveEditor::ensure_rewritable` 死检查，修正 RAR4/RAR5 编辑
  文档的矛盾描述；④ RAR4 队列注释在目录成员前落盘（`write_rar4_dir_entry` 调
  `emit_pending_rar4_comment`）。回归测试：`rar5_directory_header_rolls_to_a_fresh_volume`、
  `tiny_volume_size_rejects_directory_and_redirect_members`、
  `rar4_writer_comment_precedes_a_directory_first_member`（三者均验证过撤掉修复即失败）。
- **审查四修（2026-09，候选选择/重命名映射去重 + 老编码器单测）**：RAR29 非 solid 候选选择
  （LZ/自动过滤器/PPMd/STORE 回退）抽为 `best_rar29_member`，缓冲编码与并行准备两处共用（返回
  `None` 让拥有缓冲的调用方零拷贝回退 STORE）；`archive/rename.rs::build_rename_map` 统一 RAR4/RAR5
  编辑引擎逐字相同的两份实现；`write_progress` 的 `WriteOperation`/`WriteProgressEvent`/
  `WriteProgress` 降为 `pub(crate)`（模块私有且无 re-export）；`rar15_encoder`/`rar20_encoder` 首次
  补直接 roundtrip 单测（文本/二进制/音频/空 × 各主要选项档）。
- **审查三修（2026-09，solid/entry 组装去重）**：RAR4 三处 solid 记账收敛为
  `track_rar4_solid_member`（并删掉 `add_rar4_data` 里重复的 `maybe_reset_solid_for_extension`
  调用）；五处 `ArchiveEntry` 组装收敛为 `push_rar4_entry`，顺带修正两处多卷 push 丢失 `mtime_ns`
  的不一致（回归测试 `rar4_multivolume_entry_keeps_nanosecond_mtime`，验证过撤掉修复即失败）。
- **审查续修（2026-09，结构 + 文档 + 死代码）**：RAR4 三份重复的分卷切分循环（流式/缓冲/并行）收敛为
  单一驱动 `emit_rar4_split` + `Rar4SplitParams`（`Cow` 源闭包；流式路径在闭包内报进度），
  `pipeline.rs` 1616→1535 行且漂移源归一；修 `safe_path.rs` 错挂文档、`rar5/mod.rs` 孤儿
  `stream_mut` 文档、`codec/mod.rs` 许可声明挂错（改 `//!`）；补 12 个模块的 `//!`；清理过期注释
  （rar15/rar20/rar29/ppmd/writer.rs 的 "later phase"/"not yet wired"/deprecation 措辞）；删除
  `format/rar4/write/mod.rs` 六个被 `build_*` 取代的写流 helper（`dictionary_flags` /
  `DIRECTORY_WINDOW_BITS` 保留为 `#[cfg(test)]`）；rar15/rar20 编码器的 blanket `dead_code` allow
  补上真实理由（rars 参考 API 保留，非"未接线"）；`architecture_boundaries` 去掉重复 forbidden 项。
- **CLI 两个二进制（2026-09，主体去重）**：`selector.rs` / `password.rs` 已有；新增共享
  `ops.rs`（`#[path]` 双二进制共用）：`open_reader`、`extract_members`（整档/选成员）、
  `extract_to_stdout`（`-so`）、`print_members`（`p`）、列表三态（`list_entries` /
  `list_bare` / `list_technical`）与时间格式化。两个二进制只留开关面与消息措辞（`rar l`
  多一行 totals，`t` 措辞不同）。剩余：`t` 的编排可再抽（仅文案不同）、`extract_dest`
  的 base 解析近似。
## 待办（下一批；未关闭 issue：04、09，见 `docs/issues/compression-perf/`）

- **未关闭议题**：04（MT 随机数据的窗口级不可压缩跳过）、09（DLL 单线程解析速度）。
  issue 文件在 `docs/issues/compression-perf/issues/`，判决与基线在 `map.md`。

### 老容器族读取（RAR 1.5–4.x，继续）
- [x] solid RAR2.x/1.5 链（2026-09）：unp_ver<29 的链按归档级 MHD_SOLID+位置判定（该代编码器从不写 FHD_SOLID；rars crafted fixture 第二成员清除标志仍须共享窗口），`rar4_solid_archive` 标志接线；RAR3+ 维持 FHD_SOLID 语义。验证：solid_flag_cleared_rar15（46B→2700B 续窗）、rar250 SOLID.RAR（CRC 0x97668cf2/0x28833332 精确）b3d19a7
- [x] EXTTIME mtime 亚秒（9845c34；RAR4 无 ctime/atime 秒基字段，亚秒无从附着——记录为格式事实）；FHD_COMMENT 读取已做（`format/rar4/mod.rs::parse_file_comment` 解析 COMM_HEAD 0x75 嵌套块，STORE 载荷按 UTF-8/UTF-16LE 解码），接到 `FileHeader::comment` 并经 `ArchiveEntry::comment()` 暴露，CLI `l`/`v` 显示成员注释（2026-09）；FHD_COMMENT 写侧已做（`format/rar4/write/mod.rs::build_file_comment_block` 构造 COMM_HEAD 0x75 子块，`add_rar4_data`/`emit_segment` 追加并置 FHD_COMMENT、按 `file_header_crc_end` 重算头 CRC；编辑器新增 `EditOp::SetMemberComment`/`EditPlan::set_member_comment`，非 solid 走字节级 `rebuild_rar4_header`（rename + comment strip/set，其余字段原样保留），solid repack 由 `kept` 元组携带覆盖；RAR5 明确拒绝；CLI `cf` 设置/清除成员注释；2026-09）
- [x] store-in-solid 窗口语义（2026-09 实测）：WinRAR 6.23 RAR4 solid 强制压缩不产 store 成员（随机也压）；我们的写侧 STORE 断链、读侧对 store 冻结窗口（WinRAR 自产无此类，互操作无碍）
- [x] rar154 老命名 split 集（2026-09）：random.rar+.r00+.r01 三卷 2 MiB 成员，头 CRC 0xFFFF 哨兵容忍，CRC 0x1c9eb697 精确
- [x] 大成员流式提取（2026-09，8588302）：提取到 writer 不再整驻留——STORE 明文成员按 1 MiB 分块直拷（零整缓冲）；压缩成员（RAR29/20/15）packed 小流整读后解码器每 1 MiB flush + 窗口裁剪（峰值=窗口+一块，与成员大小无关）；VM-filter 成员整解码后还原；solid 链保持共享窗口单遍；流式 CRC 校验。验证：CLI 解 250 MB store + 67 MB m5 文本字节一致、extract 96 MiB store+压缩成员测试。错误口令提示已映射 WrongPassword（9845c34）


### RAR4（创建面·速度，后续）
- [x] 多文件并行 batch（2026-09，7c07e77）：非 solid 独立成员在线程池压缩（每成员独立引擎 + 全候选 LZ/PPMd/filter/STORE），顺序 emit（`emit_rar4_prepared` 镜像 add_file_rar4 发射半）；solid/目录/超大成员落回顺序原位。字节与顺序一致（双 feature 模式测试锁定）。实测 8×2.7 MB m5 文本 2.45s vs 单线程 14.2s（~5.8x）
- [x] 单大成员块级并行（2026-09，字节同等 MT）：>64 MiB 单文件 `-ma4` 成员级并行已做、块级未做 → 现在按 **字节同等** 原则做：64 KiB LZ 块拆解为 `analyze_block`（每块独立：token 解析 + 频率统计 + 建表，与串行逐块一致）+ `serialize_block`（顺序：keep/差分表决策推进 `previous_levels` + 位写入，MT/串行逐字节相同）。每块 history 窗口 = 串行 `local_history` 在该块的精确值（≤ `MAX_ENCODER_MATCH_OFFSET`，跨缓冲区用 `Cow::Owned`）；wave = 线程数（上限 64）限峰值内存，结果入 slot 数组按序序列化。阈值 `RAR29_PARALLEL_MEMBER_THRESHOLD` = 64 MiB；PPMd（m4/m5）不做块级。测试锁定：`mt_matches_sequential_bytes`（多 level/prior-history 逐字节 + carried levels 相等）、`block_history_matches_sequential_window`、`mt_dispatch_large_member_roundtrips`（64 MiB+ 生产路径触发 + 解码全量比对）


### RAR4（编辑面，方向已定 2026-09，实施见 ADR 0005）

- **已存在 RAR4 的编辑全补（2026-09 方向定稿，分阶段实施）**：对齐官方 rar/WinRAR。机制分层：头/块级操作（`rr` 原地补/换、`k`、`rn`/`ch`、`c`/`cw` 注释读写与展示）走结构补丁——不碰压缩数据，solid 同样可做；非 solid `d/u/f/a` 走块拷贝 + RAR4 写侧重发新成员（不重压）；**solid `d/u/f/a` 走整档 repack（解码→重编）**，对齐官方 7.21+——官方 7.20 曾做 surgical 部分重处理，在 RAR4 产坏档后 7.21 回退 full repack，surgical 仅存 RAR5，不仿 7.20。v1 边界（清晰报错拒绝）：分卷、已锁定档（`-hp` 已解除，见下）。阶段 A 头级操作 → B 非 solid 成员操作 → C solid repack。验证锁定：A = WinRAR 6.23 双向 + repair 往返；B = 6.23 `t` + 与官方操作比对；C = repack 前后解出字节一致 + 6.23 `t`。缺口现状、官方依据与代码位置见 `docs/adr/0005-rar4-edit-architecture.md`。阶段 A 进度（2026-09）：`rr` 原地补/换、`k` 锁、`rn`/`ch`、`c`/`cw` 注释读写与展示已全部落地（`archive/rar4_edit/`；6.23 双向验证：`UnRAR t`、`Rar.exe r` 消费我们的 NEWSUB 记录且修复字节一致、6.23 `cw` 逐字节还原我们写入的注释、我们读 6.23 注释夹具一致）。注释存储约定已逆向：NEWSUB `CMT` 块、载荷 STORE 或 RAR29-LZSS、`attr` 位 0 = UTF-16LE（6.23 实证）。阶段 A 完。另记：RAR4 创建面 `add_bytes` 曾无 RAR4 分支（非 ASCII 名混入 RAR5 块产出坏档）——已修：抽 `add_rar4_data` 共享管线，bytes 走同路径（含压缩/加密/分卷），6.23 `UnRAR t` 通过（`rar4_writer_add_bytes_handles_unicode_and_ascii_names` 锁定）

阶段 B 进度（2026-09）：非 solid `d/u/f/a` 全部落地（见下）

阶段 C 进度（2026-09）：solid `d` 与 `a/u/f` 全部落地——`repack_solid_archive` 整档 repack（逐成员带链解码 → 新 solid 档按原 level/mtime 重发 → 注释/rr 结构补 → 原子替换），对齐官方 7.21+ full repack。solid `a/u/f` 走**延迟 repack**：open_append 对 solid 置 deferred 标记（不截断/不暂存），add 阶段缓冲新成员，close 时整档 repack（既有成员 + 新成员），注释保留、rr 按原强度重建；已修 parallel 分支路由（append 档 write_ctx.solid_mode=false 曾误走并行写 → 写只读源档 os5）。6.23 `UnRAR t` 通过（d/a/u/f），CLI `rar d/rn/a/u/f` solid 冒烟通过。剩余边界：pre-RAR3 codec 成员的 solid repack 拒绝（清晰报错）；含目录成员的 solid repack 已支持（`add_rar4_data` 按尾部 `/` 判定目录并置 attr 0x10，repack 循环对目录成员传空数据而非解码前序运行，2026-09）。另记：排查「分节文本 solid 链第二成员解码失败」时发现并修复**既有 codec bug**（`rar29_encoder::encode_solid_member` 在 LZ 胜出时也把 levels 回滚到成员前，而解码器保留成员末表 → 下一成员 keep/delta 表头套错基准）——LZ 胜保留、PPMd 胜回滚，两臂对调；回归测试 `solid_sectioned_content_decode_regression` 转正（自家与 6.23 双向 `t` 通过）

阶段 B+C 落地内容：`d`（`edit_rar4` 支持 delete：块拷贝跳过成员 + 既有 rr 按原强度重建；删光=抹档，与 RAR5 一致；solid 走整档 repack；rename+delete 同成员冲突拒绝）；`a/u/f`（RAR4 append：`prepare_append` rar4 分支门禁 + 截断尾部 NEWSUB/ENDARC 暂存前缀，关闭时按原强度重建 rr；`rar a/u/f` CLI 编排即 editor 删 + append 追加，格式随容器自动保持 RAR4）。6.23 双向 `UnRAR t` 通过（含 6.23 自建档删/追加/repack）；**追加后重建的 rr 可被 6.23 `Rar.exe r` 修复追加成员（逐字节还原）**

**`-hp` 头加密编辑全补（2026-09）**：v1 的「`-hp` 拒绝」边界解除——主头是带 MHD_PASSWORD 的明文标记，`format/rar4/mod.rs::decrypt_encrypted_header` 提供内存态逐块头解密，`archive/rar4_edit/` 的布局扫描/注释读/`edit_rar4`/`append_prelude` 全部按口令工作：未改动块整段（含密文与盐）原样拷贝，重建/插入的块（重命名后的 FILE_HEAD、CMT、RR）用新盐重加密，**只加密头**（CMT 35 B、RR 54 B），载荷/标签表/奇偶区保持明文。solid repack 用同一口令 + `encrypt_headers` 重建保护（`-hp` 同时含数据加密，与官方一致），注释改由写侧队列 `emit_pending_rar4_comment` 在首成员前发射（`-hp` 下同样只加密 35 B 头）。恢复记录侧新增 `scan_protect_with_password` / `repair_legacy_archive_path_with_password`（CLI `rar r` 已接线），`-hp` 档的 rr 可定位、可重建、可修复。缺口令报 `RarError::Encrypted`。另修创建面 bug：`-hp` 下 NEWSUB 恢复记录曾整块加密（应为只加密 54 B 头，否则奇偶区失效）。测试锁定 `archive/rar4_edit/tests/hp.rs` 八例（rename/delete/comment/rr+修复/lock/append/solid repack/缺口令）

### RAR5（压缩面）

- [x] **流式路径自动过滤器（05，已完成）**：~~delta/x86 过滤器只走内存路径（<64 MiB 成员）；大音频/裸盘镜像 >64 MiB 走 spill 流式路径无过滤器，ratio 远差于 WinRAR——需调研 delta 可否按窗口应用、区域保持成员相对~~ 已完成：流式路径按窗口应用 delta（区域按绝对成员坐标、上限 `MAX_FILTER_BLOCK_LENGTH`，`delta_stream_window`）与 x86（`x86_stream_window`，样本检测的 E8/E8E9 区域按窗口裁剪并切块、`merge_ranges` 去重），E8/E8E9 变体及 delta 频道按 64 KiB 样本压缩尺寸选择；过滤成员独占 solid 链
- [x] **solid 归档 MT（06，定论）**（2026-09 评估）：chunk 级并行（成员内 64 KiB 块跨线程波次）已工作——64 MiB 语料实测：text 2443→593 ms（4.1x）、dll-like 20678→5187 ms（4.0x）、x86syn 3303→847 ms（3.9x）、mixed 1119→399 ms（2.8x）（`--features parallel --solid-mt`，每个成员 16 MiB）。**剩余差距是结构性的**：non-solid 的 `add_batch_parallel` 可并行处理 4 个成员（各自独立 EncoderState），而 solid 必须串行处理成员（共享窗口语义）。64 MiB 4 成员实测：non-solid archive 343 ms (text) / 2871 ms (dll-like) vs solid MT8 593 ms / 5187 ms——差距 1.7-1.8x，来自成员级并行 vs 成员内 chunk 并行的本质差异。成员级 solid 并行需要跨成员窗口共享，无法简单引入 batch 路径；唯一可能的优化是"预合并小成员为一个大逻辑块再 chunk 并行"，但对已超 12 MiB 的成员无效。**结论**：chunk 级并行已兑现，成员级差距是 solid 格式的固有代价；不建议为此引入 seq/MT 分支分歧（已有 x86 +8.2% 先例）。`--solid-mt` 扫描已加到 perfbench（`--features parallel`）
- [x] **随机数据不可压缩开销归因+STORE 预检（2026-09，A+B+C）**：原「~800 ms 未记账」被低估。用 `perfbench --only random --dict-log N` 扫描：64 MiB 随机 m3 在 dict 128KiB→32MiB 仅 2.3x（2289→5262 ms），最小字典仍 2.3s，说明瓶颈**不是匹配树**，而是**不可压缩数据上的最优解析 DP**。修复：①**已实现（B）**：放宽 matchless 快路径——`collect_block_matches` 只报 ≥4 字节匹配，把触发条件从「`runs` 为空」放宽到「最长匹配 ≤ 4 且前 2 个重复距离均为 0」。效果：64 MiB 随机 m3 @32MiB 字典 5262→3260 ms（~38%）；残余 ~1.5s 来自「最长 5+ 字节偶发碰撞块」仍走 DP；②**已实现（C）**：`encode_chunked` 入口已有 `sample_is_incompressible` 预检（`encode.rs:118-134`），64 MiB 随机 m3 codec = 46 ms（probe ~192 KiB 采样 → STORE 返回），连字面量熵编码地板也省掉——本项已完全关闭
- **dll 单线程解析速度**（map 追踪）：WinRAR m3 -mt1 522 ms、MT 171 ms（同机 smartscreen.dll 5.7 MiB）；我们 codec（裸 encode，无过滤）~3.5 s，archive（含 E8E9 过滤，真实产品路径）~3.6 s；ratio 40.72% vs WinRAR 38.64%（archive 列已于 2026-09 修正为走 `add_file` 路径，此前用 `add_bytes` 无过滤，ratio 误报为 43.21%）。速度差距 ~5.9x -mt1，~6.8x MT，瓶颈定位在 BT4 树下降步数（20M 步，每步 ~50 ns 延迟绑定），结构锁定（已验证：HASH_BITS 20→22、dict-log、commit 阈值、近/远带宽四个旋钮均弹回）
- [x] **napi 编辑操作接线 + EntryInfo 扩展（2026-09）**：binding 补齐库侧 `ArchiveEditor` 全编辑面——`renameEntries`（名字解析镜像 `rar rn`，目录展开由库处理）、`setComment`（null/空删除，`rar c`）、`setRecovery`（0–100，`rar rr`）、`lockArchive`（`rar k`），均支持 `-hp` 密码与 RAR4 头编辑；`EntryInfo` 从 6 字段扩到 16（crc32/ctime/atime/hostOs/attributes/compVersion/version/dictSizeBytes/comment/solid）。napi `Option` 在 JS 侧序列化为 `undefined` 而非 `null`（d.ts 为可选属性）。验证：native 构建 + node --test 38/38。另修 CLI.md 文档漂移（`-ma4` 已实现、补 `cf` 命令）
- [x] **napi 格式选择 + 单成员提取 + 解压冲突策略（2026-09）**：`CreateArchiveOptions.format`（"rar5" 默认 / "rar7" / "rar4"，见写侧可写子集 {v15,v20,v29,v50,v70}）——"rar4" 走 RAR4 写管线并拒绝 RAR5-only 选项（dictionary 显式报错，其余经 `validate_rar4_only`），"rar7" 强制 v70 member（仅 LZSS 路径；STORE 回退不写 comp_version，与容器语义一致，字典被 2x 文件大小上限钳到 128 KiB 地板）；`extractMember(archivePath, name, destDir, password?, signal?)` 流式单成员落地（`ArchiveReader::unique_entry` + `extract_entry_with_options`，无大小上限、可取消，返回解析后路径）；`ExtractArchiveOptions` 补 `skipExisting`/`autoRename`/`keepBroken`/`setCreationTime`/`setAccessTime`。验证：native 构建 + node --test 41/41（38+3）、workspace clippy -D warnings、napi 单测 2/2
- [x] **可复现基准 `crates/rar/examples/perfbench.rs`（2026-09）**：上述三项都没有可复现的基线（`bench.rs` 语料临时合成无指纹，`collectbench`/`mtbench` 需外部语料文件 + `parallel`）。新基准：语料全部由固定种子生成并打印 CRC32（跨机逐字节可校验）、`--repeats` 取 min/中位数、字典按 `min(32 MiB, 2*floor_pow2(size))` 裁剪（与归档路径一致，否则裸 codec 为 2 MiB 输入建 32 MiB 匹配树，`random` 出现负开销）。三口径：`codec`（裸 `rar_rs::encode`，无过滤）、`archive`（完整 create+add_file on 临时输入文件+close，RAR5 v50，**delta/x86 过滤器活跃**——与产品 `rar a` 路径一致）、`solid`（同数据切 4 成员）。语料 `text`/`mixed`/`x86syn` 沿用 `bench.rs` 的 lorem/xorshift/假 x86 定义以便与 PLAN 数字对齐，另加 `wordtext`（20 词随机散文，比 lorem 难得多）、`xml`、`dll-like`（PE 形：MZ/PE 头 + 密集 E8/E9 的 .text + 字符串 .rdata + 不可压缩块 + 对齐零填充）；`--file <path>` 可测真实文件。
  复现：`cargo run --release --example perfbench -- --size-mb 8 --repeats 3`（`--only <kind>`/`--level`/`--no-solid`/`--dict-log N` 固定字典）。不可压缩开销归因：64 MiB 随机加 `--dict-log 0..8` 扫描（见 A 项）。
  实测（8 MiB、m3、3 次；ms 依机器，ratio 与 crc32 可移植）：text 198/304/393、wordtext 7669/7748/8476、xml 1794/2356/4404、random 466/49/140、mixed 448/492/224、x86syn 284/348/475、dll-like 1894/2220/3135（codec/archive/solid 中位数）；ratio 0.01%/16.85%/7.91%/100.02%/50.01%/1.89%/32.83%。（archive 列于 2026-09 改走 `add_file` 路径；上述 ratio 为旧 `add_bytes` 口径，`x86syn`/`dll-like` 在新口径下更小——实际 `rar a` 产出的 ratio 才是产品数字。）
  结论与纠偏：① `text`/`mixed`/`x86syn` 与 `bench.rs` 及 PLAN 的 30/14/19 MiB/s 吻合，基准可信；② **`random` 的 `delta` 恒为负不是异常**——归档路径有 STORE 回退预检（`format/rar5/write/layout.rs`），64 MiB 随机下 codec 5546 ms 而 archive 仅 101 ms（纯拷贝），故「随机数据 ~800 ms 未记账开销」应改为追**真正尝试压缩的 codec 路径**（64 MiB 随机 5.5 s），而非归档层开销；③ PLAN「WinRAR 1.8s vs 我们 ~6s」出自 5.75 MB 真实 DLL，合成 `dll-like`（8 MiB codec 1894 ms / 4.2 MiB/s）只是代理，无法证实该数字，须用 `--file <真实 DLL>` 复测；④ **8 MiB/4 成员下 solid 比非 solid 明显慢**（xml 4404 vs 2356、wordtext 8476 vs 7748）——这是 solid MT（06）的起点基线。
  附注：clippy 已全量清零（见下述「已废弃 API 面移除」），本条不再适用

## 已取消 / 不做

- RAR 1.3/1.4（`RE~^` 族）：rars 支持但本实现不追（夹具稀少、DOS 时代）
- unrar `s`（转 SFX）：官方 UnRAR 7.23 无此命令，非差距

## 加固记录（原 CHANGELOG，2026-09 迁入本文件）

`CHANGELOG.md` 已删除：它的版本序列与 crate 版本不同源，且与本文件重复维护、容易漂移。
改动记录归位到**本文件的这一节（结论级）+ git 历史（过程级）**。

- **不可压缩采样预检**：不可压缩输入（媒体 / 归档 / 随机数据）采样编码后直接落 STORE，不再跑完整
  匹配查找；长距离重复兜底保证「看似随机、彼此却是远端副本」的文件仍被压缩。
- **solid 链跨成员 / 跨窗口加固**：成员边界丢弃逐帧 hash-chain 树，保留窗口尾、重复缓存与长距离
  历史；修掉 16 MiB 字典下第 3 个及以后成员丢失共享窗口的问题。
- **重定向目标包含性**：symlink / junction 的 target 是归档可控数据，按安全路径策略做词法包含性
  校验（拒绝绝对路径、盘符 / UNC、爬出根目录的 `..`）；Windows 侧改用 `symlink_file` /
  `symlink_dir` 实际创建，不再一律 `Unsupported`。
- **服务块声明大小上限**：CMT 注释 / STM 交替数据流 / QO 记录共用 64 MiB 上限，`as usize` 换
  `usize::try_from`；`ExtractOptions::max_metadata_bytes`（napi `maxMetadataBytes`）可上调或取消。
  背景：头里的 `data_size` 只被 CRC 保护，而 CRC 是打包方自算的，不是防篡改。
- **目标目录包含性先于建目录**：被拒绝的成员名不再留下空目录，判定也不再依赖刚创建的目录。
- **Windows 歧义名**：拒绝尾点 / 尾空格分量与保留设备名（CON / PRN / AUX / NUL / COM1-9 /
  LPT1-9，带扩展名也算），仅 Windows 编译进。
- **分块读取限流**：`VolumeReaders` 改为边读边长 + `take`，与另两个 `ChunkReader` 实现一致。

## 已完成（要点）

- **多卷提交事务全闭环（2026-09）**：单卷是 staging + `replace_file` 原子替换；多卷提交走
  `fs::atomic::commit_files`（park 已存在目标卷 → 安装暂存集 → 失败整体回滚 → 成功删旁路），
  更短的覆盖 retire 旧集残留分卷，旧集 `.rev` 也一并 retire。提交写
  `.{base}.rar5commit.journal` + committed 标记，进程在 rename 中途被 kill 后，下次写打开
  （`open_write`/`open_write_rar4`/`prepare_append`/`rewrite_multivolume`）会回滚未完成提交
  或收尾已完成提交；恢复只在写路径触发（读打开不动盘）。单测覆盖 prepared 回滚与
  committed 收尾两条路径。

- **低层公开面已收敛（2026-09）**：`format` / `recovery` / `crypto` 三棵树 + `rar40`/`rar50` 别名
  已 `raw` 门控；`codec` 子树里 `common` / `legacy` / `modern` 全是 `pub(crate)`，唯一公开的
  `codec::lzss_huff`（及根上的 `encode` / `decode` / `EncodeOptions` / `EncoderState` /
  `encode_chunked*`）是 ADR 0003 决策 3 明确「stable enough」要保留的，examples 依赖它。
  剩下的低层公开面只有 `detect`（`sfx_offset_of` 是正经 API，留着）。
  若将来要把 `lzss_huff` 也门控，前置是把 `examples/`（analyze_stream、clioverhead、mtbench 等）
  改用根重导出或加 `required-features` —— 目前按 ADR 不做。

- [x] **RAR7 非 2 的幂小字典解锁（2026-09）**：`DictionarySize` ≤4 GiB 锁死 2 的幂的既有判定解除（`TryFrom` 只做范围校验），`rar5_log()` 加幂守卫（此前 NON-pow2 ≤4 GiB 会给出错误 log），非幂字节 `None`-log 路由回归正确；≤4 GiB 非幂（如 6 MiB）为 **v70-only** 声明（serializer 5 位基+1/32 增量位本就精确编码，读侧窗口幂等取整不受影响）；v50 路径对同请求保持纯 log 语义（6 MiB→8 MiB 向上取整 log，仍 v50）。CLI `-ma7 -md6m` 经新 `parse_dict_bytes` 兜底放行（`-ma7` 是我们扩展，WinRAR 无此开关故无对齐约束）；`-md6m` 不带 `-ma7` 仍报 Unknown option（对齐 WinRAR）。napi `format:'rar7'` + `dictSize:'6m'` 同样放行，`rar5` 仍严格拒绝。测试：`v70_forced_non_power_of_two_dictionary`（6MiB 精确回环 + v50 对照）、CLI `-ma7 -md6m` 精确声明、napi rar7/6m 回环、`parse_dict_bytes` 单测

- [x] **napi WASI 加载器 API 面对齐（2026-09）**：`a07b999`/`b582bdb` 新增的 5 个原生 API（`extractMember`/`renameEntries`/`setComment`/`setRecovery`/`lockArchive`）在 wasm loader 里此前只有裸导出、无 host 路径映射——`extractMember` 的新旧 `destDir`、四个编辑操作的首个归档路径在 WASI+Windows 上会原样传 guest。修：`wasi-path-map.cjs` 加 5 个命名映射器（extractMember/rename/comment/recovery/lock 均映射首参归档路径，extractMember 还映射 destDir），`patch-wasi-loader.mjs` 加 5 个导出行 + 包装模板（extractMember 的返回路径再 `toHostPath` 映射回 host），模板契约测试断言 5 个包装器存在。本地重建 `wasm32-wasip1-threads` bundle + 重跑 patch（产物 gitignored，CI `build-wasm`/`test-bindings` 两条 job 均构建后重跑 patch 自动生效），`node --test` 44/44（+3 映射器单测 + 契约断言）

- [x] **老版本 RAR 创建（`-ma4`）Phase 1.2：STORE + 成员边界多卷（2026-09）**：`options.rs` 增 `CreateOptions::format_version`（默认 Rar50，`Rar40` 拒绝 RAR5-only 特性：quick-open/BLAKE2/恢复记录/恢复卷/-hp/owner/streams/RAR7 字典）；`rar40/write.rs` 固定宽度头序列化（签名/主头/文件头/endarc、CRC16、DOS 时间、ext-time、文件名编码、字典位）；`archive/create.rs` 独立 RAR4 写管线（`open_write_rar4`/`finish_writing_rar4`/`start_next_volume_rar4`）；`rar50/write/mod.rs::add_file_rar4` STORE 成员写器（单卷 + 分卷切分）。跨卷规范对齐 WinRAR 7.23（`unrar t` All OK + `unrar x` SHA256 字节级一致）：主头首卷 `MHD_FIRSTVOLUME|MHD_VOLUME`；分块头 **非末块 CR C32=本段数据**、**末块 CR C32=整文件**，packed_size=段、unpacked_size=每头整文件长；分卷用**老命名** `x.rar`/`x.r00`/`.rNN`（每百卷升字母 r→z，`volume_path_rar4`）。读回经自身 RAR4 reader 分卷合并 roundtrip 通过。**CLI `-ma4`（Phase 1.3 a）：`rar-cli/src/rar.rs` `archive_format_force_v70` 扩展返回 `format_version`，`-ma4` → `Rar40`；修 `add_batch` 并行路径对 RAR4 落回顺序写（并行压缩管线只产 RAR5，否则 RAR4 主头+ RAR5 成员混排被 WinRAR 判 Unexpected end of archive）；RAR4 不兼容开关（`-hp` 等）报错**。LZSS/PPMd 压缩留待下一阶段。设计见 `docs/rar4-creation-spec.md` + `docs/adr/0001-rar4-creation-architecture.md`
- [x] **delta 自动过滤器候选通道扩到帧尺寸 + 预门采样（2026-08）**：修 `picks_correct_channel_count_for_interleaved_streams`（自 be7a254 引入即失败，simd feature 下 CI 未跑到）暴露的两个真实缺陷——①候选通道 [1,2,3,4] 到不了多字节采样帧尺寸（16 位立体声需 4、32 位立体声需 8、24 位三声道需 9、32 位四声道需 16），扩为 [1,2,3,4,6,8,9,12,16]；实测 32 位立体声 11%（原限 ch4 时 ~18%）、24 位三声道 14%、16 位四声道 22% vs plain 84%；②预门 `auto_delta_filter_channels` 全量扫描成员（63 MiB 成员 ~300ms+，只为保护一次 64 KiB 采样编码）改为 64 KiB 头部采样；接受判据从 min-mag 通道的 near-zero 改为跨通道最大 near-zero（0/255 回绕会误导 mag 选粗通道而误拒，如 8 位单声道回绕 walk 曾拒收）。尺寸选择提取为 `pick_delta_channel` 供测试直接验证（9 种帧布局全对）。测试重写：预门只断言开关（Some/None），新增 `delta_selection_prefers_frame_size`（9 布局 × 帧尺寸）、roundtrip 扩到 2/3/4 字节采样、text 拒绝断言。全部 120 lib 测试绿

- [x] **不可压缩数据压缩提速（2026-08）**：collect 快模式阈值 4096→256 且命中判据 `longest<16`→`longest==0`（文本 4-15 字节匹配不再误触发快模式，text ratio 保持 15.32% 与基线同）；无匹配块 DP 快路径（空候选 + 缓存距离探针验证 → 直接全字面量，跳过 3 次定价 pass，输出字节级一致，测试 `matchless_fast_path_is_byte_identical` 锁定）。64 MiB 随机 m3：mt1 5044→1751、mt8 1253→486 ms，ratio 100.02%→100.01%；text/mixed/xml/sparse ratio 与基线完全一致、速度持平或更快

- [x] **MT 扩展性（2026-08）**：`compression_pool` OnceLock 永久缓存首用线程数（mtbench 里 mt4/mt8 实际跑 2 线程）→ 改为配置变化时重建池（RwLock<Arc>，在飞任务持 Arc 安全）；`extraction_pool` 同步修。不可压缩 slice 跳过片内 2 MiB 树播种（`mt_tail_is_incompressible`：stride-16 采样 256 KiB、4 字节窗去重率 ≥95% 判随机；head 仍清、chunk 自身插入与共享 LR 不变）。实测 64 MiB mixed m3：seq 17.5、mt2 36.8、mt4 62.1、mt8 84.0 MiB/s（此前 mt2=mt8≈20）；text mt8 179 MiB/s；ratio mixed 与 seq 字节级同、x86 +8.2%（既有分歧）。另修持久树 `resolve` 下溢（`rebase` 的环绕链接在非环绕减法下 panic）——`long_range_respects_dictionary_window` 测试由此从失败转绿
- [x] **napi/wasm binding 迁入 workspace（2026-08）**：`smart-archive-rar`（napi-rs，native 8 平台 + wasm32-wasip1-threads）整体迁入 `crates/rar-napi`——依赖从 git rev pin（5376c80，缺 15 提交含编码器 lock-in 修复）改为 path 依赖，rev 漂移根除；补 `..Default::default()` 修 force_v70 编译。不发布 npm 包：CI（`.github/workflows/CI.yml`，tag `v*`）矩阵构建 `.node` + wasm bundle → GitHub Release assets（含 `SHA-256SUMS` 清单），vscode 消费方 SHA-256 pin 直连下载。release profile（lto+strip）上移 workspace 根。本地验证：native + wasm 双 target 构建、node --test 29/29 通过。2026-08 CI 激活：工作流从 `crates/rar-napi/.github/`（GitHub 不执行子目录工作流）迁至仓库根；`origin` 指向 GitHub（Actions 跑 CI），codeberg 留作镜像
- [x] **batch 与 seq 字节统一（2026-08）**：`prepare_data_entry(file_origin=true)` 完整镜像 `add_file`——补上 x86 过滤器尝试（胜 STORE 时用之）与末 chunk finality 语义（`processed+len >= total`，整 4 MiB 末块也标终）。`batch_archive_matches_sequential_bytes` 全绿，seq/batch 输出 4130==4130 字节级一致
- [x] **最优解析速度/等级梯子（2026-08）**：m2/m3 降为 2 次 pass、m4 3 次、m5 4 次（此前全 3 次，梯子是假的）；定价预算 `MAX_PARSE_STEPS_PER_POSITION=12`/位置（最长 run 恒完整定价以保住 committed_through 跳步——砍掉它会让 text 变慢 44x）；快速提交阈值 NICE 512→64（x86 案例 m3 提速 37x 且压缩率 4.06%→2.85%）；缓存距离探测仅前两个。实测 m3：text 32 MiB/s、mixed 12.7 MiB/s、x86 合成 19 MiB/s、真实 DLL ~3.5 MiB/s（原 2.1）
- [x] **大文件多 chunk 退化修复（2026-08）**：插桩定位三个根因——每 chunk 重建 32 MiB 树（memset+页错误）、随机段每位置 LR 探测（12 MB 表缓存 miss）、随机段每位置树下降（32 MiB son 全 miss）。修复：树跨 chunk 持久化（只清 head 表）、LR 探测失败 4096 次后降频 1/128、树 fast mode（4096 位置无匹配跳过搜索、每 128 恢复一次）。实测 16 MiB mixed 6300→1245ms（5.1x），32 MiB 线性 10.1 MiB/s，全部解码字节级一致，压缩率 50.04%→50.02%
- [x] **多线程路径切换最优解析（2026-08）**：`encode_mt_slice` 由贪心+lazy 换为 `find_matches_optimal`（每片：新树 + budget-4 播种 2 MiB 近窗 + 共享只读 LR 绝对锚点查询；worker 状态跨 wave 复用保 son 数组暖）。实测 64 MiB mixed m3：seq 17 MiB/s、mt8 21 MiB/s，压缩率与 seq 字节级同（50.02%）；x86 合成 mt8 50 MiB/s。所有 MT 输出解码字节级一致。导出 `encode_chunked_mt`/`EncoderState`（lib.rs，parallel feature）供基准
- [x] **顺序路径大字典提速（2026-08）**：树跨 chunk 真持久化——链接按帧滑动量重定基（`TreeMatchFinder::grow_to`/`rebase`），不再每 chunk 重建树 + 重播种 8 MiB 尾部（重播种在稠密桶数据上每 chunk 3.4s）。实测 64 MiB mixed（32 MiB dict）：26.6s→3.8s（16.6 MiB/s，7x），32 MiB 线性 10 MiB/s+；压缩率不变，解码字节级一致。多线程 worker（新树）保留 budget 4 播种
- [x] 过滤器只实现 0–3（Delta/E8/E8E9/ARM），未知类型显式报错——类型 4–7 现实归档中不存在
- [x] **自动 x86 (E8/E8E9) 过滤器**：rars `x86_filter_scan` 移植（簇/跨度聚类 + 填充边界）；`encode_with_auto_x86_filter` 试 E8E9 与 E8 取小；内存路径成员（<64 MiB）自动应用；过滤器成员按非 solid 写出。实测 m5 DLL 差距从 21-23% 降到 6-14%（后由最优解析进一步收窄）
- [x] **自动 delta (multimedia) 过滤器**：内存路径成员先试 delta 再试 x86（真实 x86 代码非多通道相关，廉价 delta 扫描快速返回 None 落入 x86；音频/原始数据由 delta 胜出，避免无用的 x86 扫描）。通道选择按**压缩后尺寸**（采样前 64 KiB 各候选通道试压取最小），对字节回绕稳健——原始幅度启发式会被 0/255 边界回绕产生的大 delta 误导选错通道；且仅当严格优于 plain LZSS 才保留（结构化但非多通道数据如文本绝不劣化）。实测 8-bit walk 800000→25868、16-bit 立体声 1600000→48591，随机数据不触发（留给 plain LZSS）。`unrar` 读我们 delta 输出、`rar-rs` 读 WinRAR delta WAV 均字节级一致；顺序/批量字节级一致
- [x] **最优解析（m2-m5）**：rars `optimal_tokens`/`TokenPrices`/`BlockSplitter`/BT4 tree finder 移植；每块收集一次 + 3 次定价（首次估计、后两次用上一 pass 的真实表）；不可编码匹配（距离 bonus 使 raw < 2）在定价期拒绝；过滤器路径同样启用。实测 m3 默认级：code -52%、xml -24%、text -13%、DLL +2-6%
- [x] 解码器修复：RAR5 过滤器位置为流绝对（solid 链），但 E8/ARM 变换偏移为成员相对（unrar `WrittenFileSize`），且 x86 偏移按 16 MiB 取模——此前 solid+filter 归档跨成员引用会 CRC 错（已用真实 WinRAR 归档验证）
- [x] 大字典内存防护：读侧上限改为可配置 `ExtractOptions::max_dict_size`（默认 4 GiB，`-mdx` 语义；`None` = 不限），RAR7 v70 >4 GiB 字典按上限拒绝
- [x] 字典：`-md` 全量语义（非法值报 Unknown option 与 WinRAR 一致）；默认 32 MiB；按 `min(-md, 2×floor_pow2(文件大小))` 裁剪；读侧上限 4 GiB（RAR5 格式上限）
- [x] RAR7 (v70) 读+写：>4 GiB 字典（5 位+1/32 非幂编码）、DCX=80 扩展距离表、u64 距离；裁剪落回 ≤4 GiB 自动降级 v50；`CreateOptions::force_v70` 测试缝 + CLI `-ma7`（扩展：任意字典大小强制 v70，WinRAR 7.23 无此开关）——小字典 v70 已由真 WinRAR 验证可解（含修复 read() 路径丢失 dict_size_bytes/DCX 的 bug）
- [x] 长距离匹配（`-mcl` 语义）：采样哈希表覆盖 ≤ min(128 MiB, 字典) 历史；匹配距离受字典窗口限制（与 WinRAR 一致——实测默认/`-md32m` 下 32 MiB 远端副本双方都不压缩，`-md128m` 才压缩）；内存按实际数据惰性增长（历史 + 表，不再按字典预分配 ~2×）；128 MiB 副本实测压缩率差 0.3%、速度持平
- [x] solid + 分卷创建（encoder state 跨成员/跨卷保持，双向字节级验证）
- [x] >4 GiB 单文件创建（spill 流式管线：分块压缩 + 链式 CBC + 精确切卷，内存与文件大小无关）
- [x] **单文件多线程压缩（`-mt` 全速生效）**（2026-08）：流式路径按窗口缓冲，`codec/rar50.rs::encode_chunked_mt`
      将窗口切成 4 MiB 细粒度切片并行编码——每片带前文 tail 上下文 + 共享长距离表
      （`LongRange::find_from` 绝对锚点查询），重复距离缓存按片重置。实测 308.8 MiB 混合文件：
      `-mt8` 4.11s → **1.45–1.51s（≈2.7×）**，已超 WinRAR 7.23 同参 1.80s；压缩率 16.2%→16.7%
      （+3%，缓存重置代价）；`-mt1` 回退旧路径字节级一致；字典 128K/1M/8M/v70 标志全回环验证；
      RAR5 solid 链同样走 chunk 级 MT（窗口经共享 tail/长距离表延续）；RAR4 老编码器 solid 链保持串行

### 命令

- [x] `ch`（大小写转换）、`p`（打印）、SFX 转换 `s`/`s-`、列表变体 `lt/lb/vt/vb`（`rar` 与 `unrar` 均已支持）
- [x] `rv[N]` 补恢复卷：对**已存在**分卷集生成 .rev；计数/百分比/默认 10%、封顶 10×ND，官方语义逐项实测；`.rev` 命名跟随卷集零填充
- [x] `r` 修复改流式（`repair_archive_path`）：只驻留恢复数据，超大归档可修复；完好不写输出

### 开关（批次 1–3 完成，均与 WinRAR 实测对照）

| 类别 | 开关 |
|---|---|
| 路径/掩码 | `-r/-r0` `-ep1..4` `-x@/-n@` `-ed` `-as` `-ad/-am` `-ver[n]` `-ms` |
| 时间 | `-ta/-tb/-tl/-tk` `-tn/-to` 过滤；`-ts[m,c,a][+,-,1]` 三时间戳存取；`-tsp` |
| 文件系统 | `-ol/-oh` 链接 redirect；`-os` NTFS ADS 双向；`-ow` owner |
| 归档组织 | `-z` 注释 `-ag` 命名 `-sl/-sm` 大小过滤 `-df` `-t` `-kb` `-op` `-or` |
| 配置体系 | rar.ini / .rarrc / RARINISWITCHES；rarfiles.lst solid 排序（子集规则） |
| 交互/消息 | `-y -o± -idq -ierr -ilog -iver -cfg-` 等；系统动作类（-ieml/-ioff/-isnd）只接受不执行 |

### 工程里程碑

- [x] fmt/clippy 双门：workspace 全量 `cargo fmt --check` + `cargo clippy --all-features -- -D warnings`（codec 热路径的 `too_many_arguments` 用针对性 allow，不做风险重构）
- [x] fuzz：`fuzz/` 独立 crate 五目标（parse/crypto/recovery 读侧 + write/rewrite 写侧），standalone 变异循环 + libFuzzer 双模式；种子语料嵌入真实 WinRAR fixture。**2026-09-11 审计：`raw` 门控后 fuzz 依赖未补 `raw`，工程编译失败、五目标失效、CI 的 fuzz check 红——见「独立审计 2026-09-11」**
- [x] 根 CI 恢复（2026-08）：`.github/workflows/CI.yml` 运行 workspace fmt/check/clippy/test、独立 fuzz workspace check、native/WASI binding 构建测试与 tag release；官方互操作测试继续由 `SA_OFFICIAL_*`/本机 WinRAR 门控手动跑
- [x] 取消钩子 `RarArchive::set_cancel_flag(Arc<AtomicBool>)`：创建/提取/重写/分卷全检查点，`RarError::Cancelled`；binding 的 AbortSignal 接上
- [x] QO 快路径 `RarArchive::open_quick`：只读主头 + QO 记录即可列出（无 QO 回退全扫）；binding `listEntriesQuick`
- [x] 零填充分卷集：写侧对齐——≥10 卷时 writer 直接输出 `part01..partNN`（与 WinRAR 一致，close 收尾 rename）；`discover_volumes` 增加从基名探测 padded 首卷；`.rev` 命名跟随卷集填充；`rec_count > data_count` 误拒已修

- [x] atomic create/append：temp sibling 暂存，close 原子提交
- [x] 加密分卷每块加密记录（flags=1/3）+ `-hp` 分卷读取 + ENDARC flags 修复
- [x] deep-module 九项重构 → 仿 rars workspace 架构迁移（15636d8）

## 一致拒绝（别"修"）

- 分卷 + 内联恢复记录（`-rr`）——WinRAR 分卷只能用 `.rev`；官方 6.23 自己的 `rar r` 在带内联 RR 的分卷集上挂死（产出 0 字节 fixed），因此分卷 RR 编辑也保持拒绝。
- 分卷 append、分卷删除：官方 `rar` 同样拒绝（"Cannot modify volume"）。分卷的 `rn`/`ch`、`k` 与归档注释**不是**拒绝项：官方支持，我们也已支持（逐卷重写 / 注释插在首卷主头后）。

## 已知小差异（记录，互操作无碍）

- **RAR4 成员注释（`cf`）不是互操作保证（2026-09-11 实测）**：WinRAR 6.23 命令行没有成员注释命令（`cf` 输出用法即退）；我们写出的 RAR4（unp_ver 29）成员注释，UnRAR/Rar 6.23 与 7.23 的 `t` 均报 `Total errors: 2`（exit 3，数据仍可解出）。真实的成员注释只有 RAR2 时代样本（`rar40/rar2/comment_nopsw.rar`，UnRAR `t` 通过）。据此：格式层按该样本修正了 COMM_HEAD（13 字节固定头：HEAD_SIZE + UNP_SIZE + UNP_VER + METHOD + COMM_CRC）与三条 CRC 规则（FILE_HEAD 到 name(+salt+exttime)，COMM_HEAD 到 11 字节子块头，COMM_CRC = crc32(payload)&0xffff），读取侧新增测试能正确解出 `file1comment`/`file2comment`；但 RAR4 写侧 `cf` 属自洽扩展，**多卷成员注释因此继续拒绝**。

- solid 且无 rarfiles.lst 时：WinRAR 按扩展名/名字启发式排序，我们按参数顺序
- 目录条目名带尾斜杠
- **WinRAR 6.23/7.23 的 RAR4 修复对周期数据的缺陷（2026-09 实测）**：当归档的恢复记录块本身落入其保护的最后部分扇区（必然如此——RR 在归档尾，`total_blocks` 覆盖到 RR 头）且成员数据是短周期重复（如 64B pattern）时，WinRAR 自己的 `rar r` 会把 RR 尾部修坏（产物 `Unexpected end of archive`），无论记录是其自产还是我们产——实测 6.23 与 7.23 行为一致。我们的写侧把部分尾扇区排除出 parity 组、读侧只重建完整扇区，故我们能正确修复同样损坏（字节级回环）。互操作测试因此用伪随机成员数据；这是 WinRAR 侧缺陷，不追平。

## 备注（改代码前必读）

- 卷大小必须精确：新增块类型（如 QO）记得同步配额记账
- 加密块 padding 是 **zero-fill 不是 PKCS7**——7-Zip 会校验 padding 区全零
