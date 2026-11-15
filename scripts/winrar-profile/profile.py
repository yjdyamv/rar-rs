"""Instruction-level profile of a sampled Windows binary.

Combines the RIP samples from ``tmp/sample.ps1`` with capstone disassembly of
the functions they land in, so each sample is attributed to the instruction
that was executing. The PE is parsed by hand (no pefile in the venv).

    ~/re/.venv/Scripts/python.exe tmp/profile.py \
        --pe ~/re/samples/Rar723.exe \
        --samples tmp/samples_m3.txt \
        --funcs tmp/funcs.json \
        --fn FUN_1400468d4 --top 20
"""

import argparse
import json
import struct
import sys
from collections import Counter

import capstone


def read_pe(path):
    """Return (image_base, [(rva, raw_offset, size)]) for a PE file."""
    data = open(path, "rb").read()
    if data[:2] != b"MZ":
        raise SystemExit("not a PE")
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe : pe + 4] != b"PE\0\0":
        raise SystemExit("no PE signature")
    sections = struct.unpack_from("<H", data, pe + 6)[0]
    image_base = struct.unpack_from("<Q", data, pe + 0x18 + 0x18)[0]  # PE32+ optional header
    table = pe + 0x18 + struct.unpack_from("<H", data, pe + 0x14)[0]
    out = []
    for index in range(sections):
        entry = table + index * 40
        vsize, vaddr, rawsize, rawptr = struct.unpack_from("<IIII", data, entry + 8)
        out.append((vaddr, rawptr, max(vsize, rawsize)))
    return image_base, out, data


def bytes_at(sections, data, rva, size):
    for vaddr, rawptr, length in sections:
        if vaddr <= rva < vaddr + length:
            offset = rawptr + (rva - vaddr)
            return data[offset : offset + size]
    return b""


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--pe", required=True)
    parser.add_argument("--samples", required=True)
    parser.add_argument("--funcs", required=True)
    parser.add_argument("--fn", action="append", default=[])
    parser.add_argument("--top", type=int, default=15)
    args = parser.parse_args()

    image_base, sections, data = read_pe(args.pe)
    functions = {
        f["name"]: (int(f["addr"], 16) - image_base, f["size"])
        for f in json.load(open(args.funcs))["functions"]
    }

    samples = []
    for line in open(args.samples):
        parts = line.split()
        if len(parts) != 2:
            continue
        samples.append((int(parts[0], 16), int(parts[1])))
    total = sum(count for _, count in samples)

    dis = capstone.Cs(capstone.CS_ARCH_X86, capstone.CS_MODE_64)
    dis.detail = False

    for name in args.fn:
        rva, size = functions[name]
        code = bytes_at(sections, data, rva, size)
        instructions = []
        for insn in dis.disasm(code, rva):
            instructions.append((insn.address, insn.address + insn.size, insn.mnemonic, insn.op_str))
        print(f"\n== {name} @ {rva:#x}  {size} B  {len(instructions)} instructions")
        tally = Counter()
        inside = 0
        for address, count in samples:
            if not (rva <= address < rva + size):
                continue
            inside += count
            for start, end, mnemonic, ops in instructions:
                if start <= address < end:
                    tally[f"{start - rva:#06x}  {mnemonic} {ops}"] += count
                    break
            else:
                tally[f"{address - rva:#06x}  <no instruction>"] += count
        print(
            f"   {inside} samples ({100.0 * inside / total:.1f}% of all) "
            f"= {100.0 * inside / max(total, 1):.1f}% of total"
        )
        for text, count in tally.most_common(args.top):
            share = 100.0 * count / max(inside, 1)
            print(f"   {text:<58} {count:6}  {share:5.1f}% of fn")


if __name__ == "__main__":
    sys.exit(main())
