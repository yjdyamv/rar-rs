# Issue tracker

Work for this repo is tracked as **markdown files committed under `docs/issues/<feature>/`**.

- No GitHub/GitLab remote (origin is `git@codeberg.org:yjdyamv/rar-rs`), so tracker CLIs do not apply.
- Each ticket is a markdown file; `triage`, `to-tickets` and `to-spec` read from and write to `docs/issues/<feature>/`.
- PRs as a request surface: off.

## `.scratch/` is not the tracker

`.scratch/` is gitignored and holds throwaway artifacts (scratch scripts, one-off
review dumps). It is **not** the issue source: anything `PLAN.md` or a doc depends
on must live under `docs/issues/`, otherwise external readers cannot see it.
The compression-performance tickets were moved out of `.scratch/` in 2026-09 for
exactly this reason; see [`../issues/compression-perf/map.md`](../issues/compression-perf/map.md).
