# rar-rs 计划

完成一项勾掉一项。本文件只留**结论**与**警告**；验证过程和工程细节见 git 历史（旧版详单：`git show d9201cf:PLAN.md`）。

## 现状

- RAR5 创建/读取全功能对齐 WinRAR 7.23：压缩、`-hp` 头加密、分卷、solid、内联恢复记录、`.rev` 恢复卷、quick-open、NTFS ADS、三时间戳、owner；另含 RAR7 (v70) 读写、`-mt` 多线程压缩、长距离匹配。
- **最优解析**（m2-m5 全部）：rars 移植的 forward shortest-path DP（每块收集一次匹配、按上一 pass 的 Huffman 表重新定价 3 次、LZMA BT4 tree finder 全程 + 历史种子、BlockSplitter 按字节分布切 64-128 KiB 块、每块独立表）。m3（默认）vs WinRAR 7.23：生成代码 -52%、XML -24%、文本 -13%、DLL +2-6%（此前 +3-23%）；m2 在 DLL 上已反超 WinRAR。所有输出 WinRAR 字节级可解。
- **自适应发射块大小（2026-09）**：解剖 WinRAR 7.23 符号流（analyze_stream 工具）定位到 text64 压缩率差（12681 vs 8769 B）纯粹是每块表开销——WinRAR 在分布稳定数据上整成员一块，我们硬限 128 KiB。发射块现在合并到 4 MiB，符号流在 64 KiB 子跨度间的局部字面量/距离/长度分布漂移时提前闭合（异构二进制——DLL 节、XML——保持小块，对齐 WinRAR 的 ~64 KiB DLL 块）。解析本身不变（仍 64-128 KiB，DP 内存有界）。实测 m3 seq：text64 12607→6058（-52%，赢 WinRAR 8696）、dll -0.5%、mixed -640 B、xml +157 B（+0.18%，记录在案）、sparse/random 不变
- **持久树跨 chunk 损坏修复（2026-09）**：验证新块大小时发现既有静默损坏——窗口跨 chunk 增长时 `grow_to` 用新零数组替换 son（head 表幸存），而 0 是到位置 0 的合法链接；`rebase` 同类缺陷（链接只改值不迁槽）。密集 x86 成员产生抄 MZ 头的假匹配，unrar 与 WinRAR 双双报 checksum error。修复：grow_to 复制旧链接、rebase 把链接迁移到新环槽、收集器在解析定价前逐字节验证每个树报告（未来任何不变量破坏的安全网）。回归：129 KB 真实内核镜像前缀（旧代码在 129,334 字节处损坏，dict 2^3 + 64 KiB chunk）现字节级回环。DLL 自动 x86 过滤器产物 5.75 MB（43.90%），赢 WinRAR 7.23 的 5.87 MB（44.81%），双向完整性校验通过
- **自动 x86 过滤器**：rars 移植的结构扫描（E8/E8E9 簇/跨度检测）自动应用于内存路径成员；过滤器成员按非 solid 写出。修复了 solid 链中过滤器位置的流式绝对/成员相对语义（unrar `WrittenFileSize` 为成员相对、区域定位为流绝对、E8 偏移按 16 MiB 取模）。
- 命令面：官方 rar 全部命令（含 `rv` 补恢复卷、`lb/lt/vb/vt` 列表变体）。
- **老版本 RAR 只读（2026-09 起）**：`Rar!\x1a\x07\x00` 容器族（RAR 1.5–4.x）读取——块扫描/文件头（unicode 名、DOS 时间、salt、字典位）、STORE 直通 + **三代解码器全覆盖**：RAR3/4（unp_ver ≥ 29）LZSS+Huffman + PPMd 变体 H + 五大标准 VM 过滤器（E8/E8E9/Itanium/Delta/RGB/Audio）、RAR 2.x（20/26，LZSS+Huffman + 音频块）、RAR 1.5（15，自适应 Huffman 老 LZ）——（`codec/rar29.rs`+`codec/ppmd.rs`+`codec/rar20.rs`+`codec/rar15.rs`，rars 解码半移植）含 RAR3/4 solid 链共享窗口 + **分卷（`.partN.rar` 新命名 + `.r00/.r01` 老命名，任意卷入口，split 成员合并为多 chunk）** + **`-hp` 头加密（主头 MHD_PASSWORD 后每块 `[8B salt][align16 密文]`，列表即需口令）** + RAR15/20/30 全代数据解密。对 WinRAR 5.91 `-ma4` 夹具与 rars 语料（真 RAR 3.0/2.0/2.5/1.5.4：PPMd、六种过滤器、solid-PPMd、PPMd 内嵌过滤器、音频 WAV、17 文件 doc 集、RAR20/RAR15 老密码、2–5 卷 split 集、-hp 含中文名/分卷）字节级通过（CRC 门）。WinRAR 5.x/7.x 的 RAR4 写器不再产 PPMd，老格式夹具只能来自 rars 语料（现代 WinRAR 连 RAR2.x/1.5 也不能产）。非标准（通用）VM 程序、solid RAR2.x/1.5 链仍不支持。生成夹具工具（人工验证）：WinRAR 5.91（历史夹具）/6.23（最后的 -ma4，与 5.91 产物等价）；7.23 无 -ma4 不能产 RAR4。
- **RAR4 写侧 Tier 2 全闭（2026-09，`feat/rar4-support`）**：`-hp` 头加密写侧（主头 MHD_PASSWORD + 每块 `[8B salt][AES-128-CBC]`，64e280e）；PPMd 编码（rars 编码半：order-8/25 MiB 模型 + hybrid LZ-escape tokeniser，m4/m5 文本比 LZ 小 46%，9631d18）；NEWSUB 0x7a 恢复记录写+修（逐字段复刻 6.23，双向互修字节一致，0e65b44）；六大标准 VM 过滤器写侧（E8/E8E9/Delta/Audio 自动探测 + RGB/Itanium 编码能力；复用 RAR5 filters.rs 变换，fa4c038/f16c8b4）；**solid 链 PPMd 模型延续**（LZ levels 与 PPMd model 独立链状态、赢者推进、PPMd 赢回滚 LZ 表；ppmd 字段 Box 化避免 1 MiB 主线程栈溢出；-s 文本树自动建/续模型，cce4e15）。每项均有 WinRAR 6.23/7.23 逐字节互操作测试。已知边界：solid 链内 filter（窗口=变换字节语义,我们 LZ 上收益有限,留后续）、solid 归档 MT。
- 工程：fmt/clippy `-D warnings` 双门（本地，含测试目标）、五目标 fuzz（`fuzz/`）、取消钩子、QO 快路径 `open_quick`、流式修复 `repair_archive_path`、零填充分卷集支持。
- 架构：workspace `crates/rar`（库 crate `rar5`）+ `crates/rar-cli`（rar/unrar），按 rars 分层——词汇见 `CONTEXT.md`，格式细节见 `docs/FORMAT_RAR5_RAR7.html`。

## 待办（下一批，issue 见 `.scratch/compression-perf/`）

### 老版本 RAR 只读（继续）
- solid RAR2.x/1.5 链：链引擎按 unp_ver 泛化（rar15 已带 solid 参/64 KiB 窗，只差接线；rar20 需加链窗）
- EXTTIME 亚秒时间（mtime_ns/ctime/atime）解析、FHD_COMMENT 注释展示
- store-in-solid 链内成员的窗口语义（对照 6.23 实测；现断链保守处理）
- rar154 拆 2 MiB 老命名 split 集（random.rar；头 CRC 0xFFFFFFFF 哨兵语义）
- 大成员流式（现整成员驻留内存）与错误口令提示（RAR3/4 现为解码乱码错）


### RAR5（压缩面）

- **流式路径自动过滤器（05）**：delta/x86 过滤器只走内存路径（<64 MiB 成员）；大音频/裸盘镜像 >64 MiB 走 spill 流式路径无过滤器，ratio 远差于 WinRAR——需调研 delta 可否按窗口应用、区域保持成员相对
- **solid 归档 MT（06）**：solid 强制串行，备份类负载无多线程收益；MT 对 seq 的既有分歧（x86 +8.2%，重复距离缓存按片重置）会带进 solid，需先评估可接受性
- 随机数据 ~800 ms 未记账开销（fast-path 循环、splitter、播种、LR 建表、片组装）尚未逐项归因
- **dll 单线程解析速度**（map 追踪）：WinRAR m3 1.8s vs 我们 ~6s；ratio 已反超（43.90% vs 44.81%），速度仍 3x 落后

## 已取消 / 不做

- RAR 1.3/1.4（`RE~^` 族）：rars 支持但本实现不追（夹具稀少、DOS 时代）
- unrar `s`（转 SFX）：官方 UnRAR 7.23 无此命令，非差距

## 已完成（要点）

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
      solid 归档保持串行

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
- [x] fuzz：`fuzz/` 独立 crate 五目标（parse/crypto/recovery 读侧 + write/rewrite 写侧），standalone 变异循环 + libFuzzer 双模式；种子语料嵌入真实 WinRAR fixture
- [x] CI 移除（`.woodpecker/ci.yml` 删除，2026-08）：回归验证回归本地 fmt/clippy + fuzz，官方互操作测试继续由 `SA_OFFICIAL_*`/本机 WinRAR 门控手动跑
- [x] 取消钩子 `RarArchive::set_cancel_flag(Arc<AtomicBool>)`：创建/提取/重写/分卷全检查点，`RarError::Cancelled`；binding 的 AbortSignal 接上
- [x] QO 快路径 `RarArchive::open_quick`：只读主头 + QO 记录即可列出（无 QO 回退全扫）；binding `listEntriesQuick`
- [x] 零填充分卷集：写侧对齐——≥10 卷时 writer 直接输出 `part01..partNN`（与 WinRAR 一致，close 收尾 rename）；`discover_volumes` 增加从基名探测 padded 首卷；`.rev` 命名跟随卷集填充；`rec_count > data_count` 误拒已修

- [x] atomic create/append：temp sibling 暂存，close 原子提交
- [x] 加密分卷每块加密记录（flags=1/3）+ `-hp` 分卷读取 + ENDARC flags 修复
- [x] deep-module 九项重构 → 仿 rars workspace 架构迁移（15636d8）

## 一致拒绝（别"修"）

- 分卷 + 内联恢复记录（`-rr`）——WinRAR 分卷只能用 `.rev`
- 分卷 append；分卷 lock / 注释——官方 rar 同样拒绝或受限

## 已知小差异（记录，互操作无碍）

- solid 且无 rarfiles.lst 时：WinRAR 按扩展名/名字启发式排序，我们按参数顺序
- 目录条目名带尾斜杠
- **WinRAR 6.23/7.23 的 RAR4 修复对周期数据的缺陷（2026-09 实测）**：当归档的恢复记录块本身落入其保护的最后部分扇区（必然如此——RR 在归档尾，`total_blocks` 覆盖到 RR 头）且成员数据是短周期重复（如 64B pattern）时，WinRAR 自己的 `rar r` 会把 RR 尾部修坏（产物 `Unexpected end of archive`），无论记录是其自产还是我们产——实测 6.23 与 7.23 行为一致。我们的写侧把部分尾扇区排除出 parity 组、读侧只重建完整扇区，故我们能正确修复同样损坏（字节级回环）。互操作测试因此用伪随机成员数据；这是 WinRAR 侧缺陷，不追平。

## 备注（改代码前必读）

- 卷大小必须精确：新增块类型（如 QO）记得同步配额记账
- 加密块 padding 是 **zero-fill 不是 PKCS7**——7-Zip 会校验 padding 区全零
