# 官方 Rar.exe 的采样剖析

Windows 上没有管理员权限就用不了 ETW / `xperf`（`xperf: error: NT Kernel Logger: 拒绝访问。(0x5)`），
所以这里是一个自建的穷人版采样器：挂起主线程、读 `Rip`、减模块基址，得到 RVA 直方图，
再用 capstone 反汇编把采样归到具体指令。

```powershell
powershell -NoProfile -File scripts/winrar-profile/sample.ps1 `
  -Exe 'C:\path\to\Rar.exe' -Args 'a -m3 -md32m -mt1 -y out.rar <file>' -Out samples.txt
python scripts/winrar-profile/profile.py -Exe 'C:\path\to\Rar.exe' -Samples samples.txt
```

`map.ps1` 把 RVA 归到函数（要 ghlab 的 `functions --json` 输出）：先用 `ghlab functions` 导出
`funcs.json`，再 `powershell -File scripts/winrar-profile/map.ps1 -Funcs funcs.json -Samples samples.txt`。

采样器对被测进程是侵入式的（挂起/恢复每 ~1 ms 一次），绝对值会比 `time rar` 高一档，
但**函数之间的占比**是可信的：这就是 ADR 0011「官方的时间到底花在哪」那张表的来源。
