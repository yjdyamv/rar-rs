# 开关矩阵 — rar-rs vs WinRAR 7.23

逐项对照官方 RAR 7.23 控制台 `rar` 的输出维护（本机 `C:\Program Files\WinRAR\Rar.exe`）。
状态：✅ 实现 · ◐ 部分/仅接受（no-op） · ❌ 未实现 · — 不适用（命令形态而非开关）。

## 命令

| 命令 | 状态 | 备注 |
|---|---|---|
| a / c / cw / d / e / f / k / m / p / r / rc / rn / rr / s / s- / t / u / x | ✅ | |
| i[par]=<str> 查找 | ✅ | `i`/`ic`/`ih` 变体 |
| ch 修改参数 | ◐ | 仅 `-cl`/`-cu`；-tl/-tk/-ed/-c- 未做 |
| l[t[a],b] / v[t[a],b] 列表变体 | ✅ | `rar` 二进制 lb/lt/vb/vt 已补（unrar 早已有） |
| m[f] 移动 | ◐ | `mf`（仅文件）变体缺 |
| rv[N] 补恢复卷 | ✅ | 对已有分卷集补 .rev；计数/百分比/默认 10%/封顶 10×ND，官方实测；`.rev` 命名跟随卷集零填充 |

## 开关

| 开关 | 状态 | 备注 |
|---|---|---|
| -m0..5 | ✅ | 压缩级别 |
| -md<size> | ✅ | 含 RAR7 >4 GiB（v70） |
| -mdx<size> | ✅ | 解压字典上限（默认 4 GiB） |
| -mc<par> / -me<par> | ◐ | 接受 |
| -ms<list> | ✅ | 指定类型不压缩 |
| -mt<threads> | ✅ | 压缩/解压线程 |
| -mcl | ✅ | 长距离匹配（WinRAR 隐藏开关） |
| -s[=<par>] / -ds | ✅ | solid / 关闭排序 |
| -p<password> / -p- | ✅ | 密码 / 清密码 |
| -hp<password> | ✅ | 头加密（含分卷逐卷明文加密头） |
| -htb / -htc | ✅ / ◐ | BLAKE2sp / CRC32（默认，接受） |
| -v<size> / -vd / -vp | ✅ / ◐ / ◐ | 分卷 / 擦盘(收) / 暂停(收) |
| -rr[N] / -rv[N] | ✅ | 内联恢复记录 / 恢复卷 |
| -qo[-|+] | ✅ | quick-open |
| -r / -r0 / -r- | ✅ | 递归（`-r-` 走 --no-recurse） |
| -ep / -ep1 / -ep2 / -ep3 / -ep4<path> | ✅ | 路径剥离 |
| -ap<path> | ✅ | 归档内路径前缀 |
| -x<file> / -x@ / -x@<list> | ✅ | 排除 |
| -n<file> / -n@ / -n@<list> | ✅ | 包含 |
| -ed / -as / -ad / -am | ✅ / ✅ / ✅ / ◐ | |
| -ol / -oh | ✅ | 符号/硬链接存为链接 |
| -op<path> / -or | ✅ | 输出路径 / 自动改名 |
| -os / -ow | ✅ | NTFS 流 / owner（Windows/Unix 各平台生效） |
| -df / -t / -kb / -si<name> | ✅ | 删源 / 测后 / 保留损坏 / 标准输入 |
| -ta/-tb/-tn/-to | ✅ | 时间过滤 |
| -tl / -tk | ✅ | 归档时间=最新 / 保持 |
| -ts[m,c,a][±,1] | ✅ | 三时间戳存取 |
| -tsp | ✅ | 保留源访问时间 |
| -ver[n] | ✅ | 版本控制 |
| -ag[format] | ✅ | 自动命名 |
| -z<file> / -c- | ✅ | 注释文件 / 禁注释 |
| -w<path> | ✅ | 工作目录 |
| -y / -o± | ✅ | 全 Yes / 覆盖模式 |
| -idc/d/n/p/q / -inul | ◐ | 仅 -idq 有效，其余接受 |
| -ierr / -ilog / -iver | ✅ | |
| -cfg- / -sc<charset> | ✅ / ◐ | |
| -ac / -ai / -e<attr> / -ao | ◐ | Windows 专属，接受 |
| -dh / -dr / -dw | ◐ | 共享/回收站/擦除，接受 |
| -ieml / -ioff / -isnd | ◐ | 邮件/关机/声音，接受 |
| -ri<P>[:<S>] | ◐ | 优先级/休眠，接受 |
| -mlp / -oc / -oni / -oi | ◐ | 大页/压缩属性/不兼容名/去重，接受 |
| -log[f][=name] | ❌ | 文件名日志，未实现 |
| -om[-|1][=lst] | ❌ | Mark of the Web，未实现 |
| -k（开关形态） | ❌ | 锁定（命令形态已实现） |
| -（停止扫描） / @[+] | ❌ | 边缘交互 |
| -ad1/2 / -ts…p | — | 与 -ad / -tsp 等价或收 |

## 格式能力

| 能力 | 状态 |
|---|---|
| RAR5 创建/读取/修改 | ✅ |
| RAR7 (v70) 读+写（>4 GiB 字典、DCX=80、u64 距离） | ✅ |
| 单文件多线程压缩（实测超 WinRAR 7.23） | ✅ |
| solid（单卷+分卷、状态跨卷保持） | ✅ |
| 分卷 + -hp 逐卷明文加密头 | ✅ |
| 内联 RR + .rev（GF(2^16) Cauchy） | ✅ |
| quick-open / BLAKE2sp / NTFS ADS / 三时间戳 / owner / 重定向 | ✅ |
| 过滤器 0–3（Delta/E8/E8E9/ARM） | ✅（4–7 显式拒绝） |
| RAR4 / PPMd | ❌ 明确拒绝（设计决定） |

## 明确不做

- RAR4 全部（创建/提取/加密/PPMd）：RAR5-only 定位，遇 RAR4 报 unsupported（测试锁定）
- GUI、Windows 右键菜单、可移动介质自动切卷、`-en`（7.23 无此开关）
