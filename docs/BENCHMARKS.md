# Zig Ghostty vs the Rust port: benchmarks

This compares the original Zig Ghostty with this Rust port using the
micro-benchmark CLI both sides ship (`ghostty-bench`), with the same inputs
and the same flags on the same machine. `tools/bench_compare.sh` reproduces
everything on this page: it builds both sides, generates the data, runs
hyperfine and prints the tables.

## Environment

| | |
|---|---|
| CPU | Intel Xeon Processor @ 2.10 GHz, 4 vCPU (KVM guest), AVX2 + AVX-512F/BW, 8 MiB L2, 260 MiB L3 |
| Memory / OS | 15 GB, Linux 6.18.44 (x86_64), glibc |
| Zig side | Ghostty `93967b7f9478e94dcbb42125c8a90f34feb40d75`, Zig 0.17.0 |
| Rust side | ghostty-rs `5a81a32` (after both rounds of optimizations below; the [before](#before-optimization) numbers are from `30a4945`), rustc 1.97.0 (2d8144b78 2026-07-07) |
| Timer | hyperfine 2.0.0 (`-N`, no shell) |

The VM is noisy even when otherwise idle: most results have a 5–10 %
standard deviation, and runs under 100 ms vary by up to 15 %. Absolute times
also drift between VM sessions by about 10 % on both sides (compare the Zig
columns of the two tables), so compare ratios within one table.

## Builds

| | Zig | Rust |
|---|---|---|
| Command | `zig build -Doptimize=ReleaseFast -Demit-bench -Dapp-runtime=none` | `cargo build --release --no-default-features --bin ghostty-bench --bin ghostty-gen` |
| Target CPU | **native** (Zig's default: AVX-512 here) | **x86-64 baseline** (rustc's default) |
| SIMD | `-Dsimd=true` (default): C++ Highway and simdutf, which pick AVX2 at runtime | pure Rust `std::arch`/`memchr`: SSE2 (baseline) code, plus AVX2 base64 and UTF-8 decoding, AVX2 instances of the VT stream loop and `printSliceFill`, and SSE4.2 CRC32C, selected at runtime |
| Codegen | single LLVM module, no safety checks | default release profile: opt-level 3, 16 codegen units, thin-local LTO, overflow checks off, bounds checks on |

On Linux the default `-Dapp-runtime=gtk` pulls the GTK app runtime into the
bench build, and that fails here (no `gtk4-layer-shell` headers and no
`blueprint-compiler`). `-Dapp-runtime=none` builds the same benchmark code
without the app runtime.

The two defaults use different instruction sets, so two extra builds separate
ISA effects from code quality:

* **Zig baseline**: `-Dcpu=baseline`. It uses the same ISA as the default Rust
  build, so **Rust/Zig baseline is the like-for-like code comparison**.
  Highway and simdutf still pick AVX2 at runtime in this build.
* **Rust native**: `RUSTFLAGS="-C target-cpu=native"`.

A release profile closer to Zig's ReleaseFast (`lto = "fat"`,
`codegen-units = 1`) was also tried. It changed no case by more than the
noise (`stream/ascii` -4 %, `stream/utf8` -3 %, `terminal-resize --mode=cols`
+10 %, the rest within ±4 %) and made the release build 1.6× slower
(2m13s instead of 1m23s here), so the release profile stays the default.
`panic = "abort"` was not considered: the GTK runtime relies on
`catch_unwind`.

## Method

* The inputs are generated once by the **Zig** `ghostty-gen` and read by all
  binaries from the same files under `$WORK/data`:
  * `ascii.vt`, `ascii-lines.vt` (`--line-max=160`), `utf8.txt`, `styled.vt`,
    `kitty.apc`: 64 MiB each.
  * `osc.vt` (streaming OSC): 8 MiB. `osc.bin` (length-prefixed, parser
    style): 32 MiB.
  * `seq.txt`: `seq 1 N`, 128 MiB. This is a large `cat`-style log.
  * Small `styled` and `ascii` files for the screen-clone, formatter and
    snapshot cases.
* hyperfine runs every case with the same `ghostty-bench +<bench> <flags>`
  on each binary: 2 warmups and 8 runs. The short or noisy cases were re-run
  with 3 warmups and 20 runs, replacing the first pass, and the startup case
  uses 80 (re-run: 200) runs.
  The number reported is the **median wall-clock time of the whole
  process**, as Ghostty's own `src/benchmark/AGENTS.md` prescribes. It
  includes reading the data file and any setup. The `noop` modes show that
  fixed cost.
* `loops` values were picked so that each run takes about 0.1 to 2 s. Some
  benchmarks scale `loops` internally. For example, `screen-clone --mode=render`
  does 50 000 × `loops` updates, and `terminal-resize` does 50 M, 2.5 M or 25
  resizes × `loops`, depending on the mode.
* The profiles come from `valgrind --tool=callgrind` (Ir counts) on 2–8 MiB
  inputs, with the Rust side built with
  `CARGO_PROFILE_RELEASE_DEBUG=line-tables-only` for line-level counts.
  `perf` is not available in this VM. Valgrind cannot run the AVX-512 Zig
  build, so the Zig profiles are from the `-Dcpu=baseline` build.
* Callgrind counts glibc's `memset` (`rep stosb` for large sizes) at one
  instruction per byte, while Zig links its own vector-store `memset` loop.
  Both sides zero the same recycled pages in `PageList::grow`, so the
  `memset` Ir difference in the stream profiles is a counting artefact, not
  a cost difference.

## Results

Ratios above 1 mean Rust is slower. "Zig / Rust peak RSS" is the median
maximum resident set size.

| benchmark | Zig ms (median ± sd) | Rust ms | Rust/Zig | Rust native ms | Rust native/Zig | Zig baseline ms | Zig baseline/Zig | Rust/Zig baseline | Zig / Rust peak RSS MiB |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| stream/ascii | 78.5 ± 8.4 | 82.9 ± 4.4 | 1.06× | 105.9 ± 6.3 | 1.35× | 102.5 ± 7.3 | 1.31× | 0.81× | 5 / 6 |
| stream/ascii-lines | 140.0 ± 11.5 | 155.1 ± 6.4 | 1.11× | 183.3 ± 10.8 | 1.31× | 159.5 ± 12.2 | 1.14× | 0.97× | 5 / 6 |
| stream/seq | 1,224.9 ± 33.3 | 1,391.8 ± 40.7 | 1.14× | 1,438.3 ± 59.3 | 1.17× | 1,232.7 ± 73.4 | 1.01× | 1.13× | 5 / 7 |
| stream/utf8 | 1,350.4 ± 52.4 | 1,466.4 ± 50.4 | 1.09× | 1,402.3 ± 42.8 | 1.04× | 1,449.5 ± 41.5 | 1.07× | 1.01× | 6 / 7 |
| stream/styled | 151.0 ± 8.7 | 173.6 ± 12.9 | 1.15× | 187.1 ± 9.1 | 1.24× | 182.5 ± 14.1 | 1.21× | 0.95× | 5 / 6 |
| stream/osc | 1,096.5 ± 59.0 | 846.5 ± 31.7 | 0.77× | 794.4 ± 48.9 | 0.72× | 1,243.7 ± 44.8 | 1.13× | 0.68× | 7 / 8 |
| stream/kitty | 83.7 ± 11.7 | 44.2 ± 6.5 | 0.53× | 40.5 ± 9.3 | 0.48× | 85.0 ± 13.4 | 1.02× | 0.52× | 5 / 6 |
| parser/ascii | 1,365.8 ± 40.0 | 846.3 ± 30.2 | 0.62× | 853.5 ± 24.6 | 0.62× | 1,185.4 ± 260.6 | 0.87× | 0.71× | 4 / 6 |
| parser/utf8 | 1,030.5 ± 258.2 | 881.0 ± 18.3 | 0.85× | 893.0 ± 25.6 | 0.87× | 969.1 ± 174.7 | 0.94× | 0.91× | 4 / 6 |
| parser/styled | 1,370.4 ± 39.9 | 847.3 ± 139.6 | 0.62× | 858.4 ± 43.0 | 0.63× | 1,158.7 ± 31.8 | 0.85× | 0.73× | 4 / 6 |
| osc-parser | 69.9 ± 5.7 | 63.3 ± 9.2 | 0.91× | 60.1 ± 2.6 | 0.86× | 69.1 ± 4.7 | 0.99× | 0.92× | 4 / 6 |
| apc-parser/kitty | 25.8 ± 2.9 | 31.1 ± 3.2 | 1.21× | 30.0 ± 7.4 | 1.17× | 27.1 ± 3.4 | 1.05× | 1.15× | 4 / 6 |
| cp-width/noop | 1.8 ± 0.2 | 2.6 ± 0.1 | 1.46× | 2.7 ± 0.1 | 1.49× | 1.7 ± 0.2 | 0.97× | 1.50× | 4 / 6 |
| cp-width/wcwidth | 508.6 ± 19.2 | 473.0 ± 20.6 | 0.93× | 488.6 ± 14.9 | 0.96× | 505.3 ± 11.1 | 0.99× | 0.94× | 4 / 6 |
| cp-width/simd | 807.3 ± 35.9 | 433.8 ± 17.3 | 0.54× | 441.5 ± 19.2 | 0.55× | 896.2 ± 104.8 | 1.11× | 0.48× | 4 / 6 |
| cp-width/table | 450.9 ± 27.3 | 411.0 ± 21.9 | 0.91× | 444.3 ± 30.4 | 0.99× | 455.2 ± 19.9 | 1.01× | 0.90× | 4 / 6 |
| grapheme/noop | 398.2 ± 20.3 | 413.9 ± 21.1 | 1.04× | 411.0 ± 16.9 | 1.03× | 377.1 ± 17.0 | 0.95× | 1.10× | 4 / 6 |
| grapheme/table | 461.8 ± 28.4 | 463.9 ± 18.8 | 1.00× | 484.4 ± 15.9 | 1.05× | 452.0 ± 21.4 | 0.98× | 1.03× | 4 / 6 |
| is-symbol/uucode | 455.6 ± 20.0 | 410.7 ± 21.6 | 0.90× | 469.0 ± 20.1 | 1.03× | 439.5 ± 21.8 | 0.96× | 0.93× | 4 / 6 |
| is-symbol/table | 464.9 ± 15.4 | 435.4 ± 18.6 | 0.94× | 427.8 ± 16.5 | 0.92× | 436.4 ± 25.2 | 0.94× | 1.00× | 4 / 6 |
| screen-clone/noop | 51.4 ± 3.4 | 32.9 ± 4.1 | 0.64× | 37.5 ± 4.3 | 0.73× | 50.0 ± 3.7 | 0.97× | 0.66× | 6 / 6 |
| screen-clone/clone | 170.6 ± 12.4 | 162.5 ± 9.1 | 0.95× | 170.3 ± 10.8 | 1.00× | 176.8 ± 12.9 | 1.04× | 0.92× | 97 / 97 |
| screen-clone/render | 403.8 ± 23.4 | 376.0 ± 15.2 | 0.93× | 383.3 ± 18.2 | 0.95× | 385.1 ± 14.9 | 0.95× | 0.98× | 6 / 7 |
| screen-clone/render-clean | 298.5 ± 17.0 | 166.9 ± 10.0 | 0.56× | 187.9 ± 15.1 | 0.63× | 278.7 ± 18.0 | 0.93× | 0.60× | 6 / 7 |
| screen-clone/render-partial | 363.6 ± 20.0 | 285.7 ± 18.8 | 0.79× | 280.9 ± 25.9 | 0.77× | 370.8 ± 20.3 | 1.02× | 0.77× | 6 / 7 |
| screen-clone/render-scroll | 979.9 ± 27.2 | 960.2 ± 56.7 | 0.98× | 959.2 ± 22.2 | 0.98× | 993.9 ± 55.0 | 1.01× | 0.97× | 72 / 73 |
| screen-clone/render-output | 416.6 ± 17.3 | 453.8 ± 30.1 | 1.09× | 411.8 ± 10.7 | 0.99× | 411.7 ± 26.1 | 0.99× | 1.10× | 6 / 7 |
| hyperlink-map/lookup | 657.0 ± 43.5 | 382.8 ± 16.4 | 0.58× | 670.2 ± 140.4 | 1.02× | 365.3 ± 19.4 | 0.56× | 1.05× | 4 / 6 |
| hyperlink-map/churn | 407.7 ± 7.7 | 317.9 ± 20.5 | 0.78× | 415.3 ± 17.2 | 1.02× | 322.8 ± 24.5 | 0.79× | 0.98× | 4 / 6 |
| page-compression/compress | 980.7 ± 35.5 | 1,030.1 ± 137.5 | 1.05× | 1,009.9 ± 20.3 | 1.03× | 1,043.6 ± 64.4 | 1.06× | 0.99× | 69 / 70 |
| page-compression/store | 1,003.2 ± 103.3 | 995.7 ± 38.4 | 0.99× | 981.4 ± 32.9 | 0.98× | 1,054.7 ± 43.6 | 1.05× | 0.94× | 69 / 70 |
| page-compression/decompress | 599.5 ± 21.5 | 581.5 ± 53.3 | 0.97× | 647.7 ± 49.5 | 1.08× | 618.0 ± 33.4 | 1.03× | 0.94× | 134 / 135 |
| scrollback-compression/noop | 469.2 ± 22.1 | 455.0 ± 11.3 | 0.97× | 507.5 ± 42.4 | 1.08× | 534.2 ± 43.2 | 1.14× | 0.85× | 196 / 197 |
| scrollback-compression/compress | 710.4 ± 37.8 | 675.6 ± 20.9 | 0.95× | 744.9 ± 30.2 | 1.05× | 767.5 ± 35.6 | 1.08× | 0.88× | 196 / 197 |
| scrollback-compression/incremental | 678.5 ± 34.8 | 678.1 ± 28.1 | 1.00× | 763.9 ± 28.9 | 1.13× | 745.1 ± 50.3 | 1.10× | 0.91× | 196 / 197 |
| scrollback-compression/restore | 924.7 ± 38.1 | 905.1 ± 90.1 | 0.98× | 927.4 ± 49.2 | 1.00× | 917.7 ± 21.5 | 0.99× | 0.99× | 230 / 231 |
| formatter/noop | 11.4 ± 2.5 | 11.1 ± 0.4 | 0.97× | 12.5 ± 4.7 | 1.09× | 10.5 ± 1.5 | 0.92× | 1.05× | 11 / 12 |
| formatter/plain | 844.9 ± 45.3 | 901.4 ± 32.1 | 1.07× | 874.9 ± 43.9 | 1.04× | 867.1 ± 25.1 | 1.03× | 1.04× | 12 / 13 |
| formatter/vt | 525.2 ± 20.0 | 561.5 ± 31.0 | 1.07× | 671.3 ± 29.7 | 1.28× | 569.2 ± 35.5 | 1.08× | 0.99× | 12 / 13 |
| formatter/html | 1,238.7 ± 44.9 | 1,166.7 ± 45.0 | 0.94× | 1,219.8 ± 41.9 | 0.98× | 1,188.4 ± 50.4 | 0.96× | 0.98× | 12 / 13 |
| formatter/terminal-roundtrip | 25.7 ± 4.4 | 30.7 ± 8.7 | 1.19× | 42.4 ± 22.1 | 1.65× | 28.0 ± 6.6 | 1.09× | 1.10× | 19 / 20 |
| resize/noop | 373.7 ± 31.6 | 239.9 ± 14.8 | 0.64× | 245.9 ± 17.7 | 0.66× | 381.5 ± 13.6 | 1.02× | 0.63× | 22 / 23 |
| resize/cols | 199.8 ± 111.2 | 227.8 ± 14.0 | 1.14× | 197.2 ± 14.7 | 0.99× | 215.8 ± 21.3 | 1.08× | 1.06× | 23 / 24 |
| resize/cols-no-reflow | 1,487.6 ± 57.2 | 1,410.7 ± 53.9 | 0.95× | 1,411.6 ± 66.8 | 0.95× | 1,397.1 ± 63.1 | 0.94× | 1.01× | 22 / 23 |
| resize/rows | 1,466.0 ± 35.5 | 1,430.9 ± 89.6 | 0.98× | 1,416.1 ± 67.0 | 0.97× | 1,479.7 ± 54.7 | 1.01× | 0.97× | 22 / 23 |
| resize/both | 202.7 ± 18.0 | 220.4 ± 16.6 | 1.09× | 198.2 ± 10.3 | 0.98× | 221.5 ± 18.3 | 1.09× | 0.99× | 23 / 24 |
| snapshot/noop | 12.6 ± 1.8 | 15.1 ± 2.4 | 1.19× | 16.0 ± 1.8 | 1.26× | 14.4 ± 2.4 | 1.14× | 1.05× | 14 / 15 |
| snapshot/encode | 429.8 ± 29.4 | 412.4 ± 18.5 | 0.96× | 358.7 ± 22.1 | 0.83× | 641.4 ± 29.5 | 1.49× | 0.64× | 14 / 15 |
| snapshot/decode | 432.1 ± 23.2 | 455.1 ± 29.3 | 1.05× | 412.8 ± 16.3 | 0.96× | 480.1 ± 26.2 | 1.11× | 0.95× | 21 / 22 |
| snapshot/decode-ready | 163.8 ± 15.1 | 169.0 ± 8.9 | 1.03× | 164.7 ± 15.3 | 1.01× | 163.4 ± 11.6 | 1.00× | 1.03× | 14 / 15 |
| startup/bench-no-data | 1.8 ± 0.8 | 2.7 ± 0.6 | 1.49× | 2.7 ± 1.0 | 1.49× | 1.8 ± 0.3 | 1.01× | 1.47× | 4 / 5 |
| snapshot/encode-kitty | 316.3 ± 9.5 | 285.4 ± 18.3 | 0.90× | 279.6 ± 11.6 | 0.88× | 507.3 ± 23.7 | 1.60× | 0.56× | 23 / 25 |

The exact flags for each case are in the `CASES` array of
`tools/bench_compare.sh`.

**Summary.** The summary covers the 44 non-`noop`, non-startup cases. The
geometric mean of Rust/Zig is **0.92×** (0.94× after the first round of
optimizations, 1.19× before any). The geometric mean of Rust/Zig baseline
(same ISA) is **0.90×** (0.93×, 1.15×). Rust native/Zig is 0.96×. Rust is
faster than Zig in 19 cases, within ±5 % in 11, and slower in 14. Most of
those 14 are within this VM's run-to-run noise (see the analysis); the
ones that are not are explained there. Peak memory is the same on both
sides, within about 1–2 MiB of fixed overhead.

### Binary sizes

| binary | Zig bytes | Rust bytes | Zig stripped | Rust stripped |
|---|---:|---:|---:|---:|
| ghostty-bench | 67,925,960 | 2,987,216 | 9,620,344 | 2,419,144 |
| ghostty-gen | 6,475,896 | 923,016 | 570,424 | 637,896 |

The Zig ReleaseFast bench tools ship with DWARF debug info, so the stripped
sizes are the ones to compare. The Zig `ghostty-bench` also statically links
Highway, simdutf, libc++ and every benchmark's dependencies. Cargo's release
profile already strips debug info but keeps the symbol table.

### Startup

The Zig `ghostty` executable cannot be built in this environment, because its
app runtime on Linux is GTK (see Builds). So `ghostty +version` could not be
compared. Instead, `startup/bench-no-data` runs `ghostty-bench
+terminal-parser` without `--data`. That measures process start, `global.init`
and argument parsing, and the benchmark step returns immediately. Zig takes
1.8 ms and Rust 2.7 ms. Both are close to the timer's resolution; the
difference is dynamic loading (see the analysis).

## Optimizations

Each change keeps the data layout, the algorithm and the results of the
port; most make the Rust code do what the Zig code (or the C++ library it
calls) already does. Every existing test passes unchanged, and the changes
with new logic have randomized equivalence tests against the previous
scalar code. Ratios are Rust/Zig (native) and Rust/Zig baseline, before →
after, from the two tables on this page.

| change | cases | before | after |
|---|---|---:|---:|
| **Snapshot grid codec** (`terminal/snapshot/grid.rs`): `EncodedWidth::write_value`/`read_value` copied a runtime-length slice, one libc `memcpy` call per cell. They now do one fixed-size store/load per width, and the narrow encode and widen loops are monomorphized per width with a const generic (Zig: `comptime width`). | snapshot/encode | 4.11× / 2.66× | 1.04× / 0.67× |
| | snapshot/decode | 2.70× / 2.76× | 1.06× / 0.99× |
| | snapshot/decode-ready | 3.27× / 3.25× | 1.15× / 1.21× |
| **OSC capture inline** (`terminal/osc.rs`): `Parser::next` and `Capture::write_byte` were out-of-line calls per byte. The capture append is now inline and the prefix state machine and the rare growth/failure paths are separate functions (the Zig benchmark calls `next` with `.always_inline`). | osc-parser | 2.86× / 2.99× | 0.91× / 0.92× |
| **AVX2 base64** (`simd/base64.rs`): Zig decodes Kitty graphics payloads with simdutf's AVX2 kernel; the port used the scalar decoder. A runtime-detected AVX2 decoder (the Muła/Lemire algorithm simdutf uses) now handles inputs that are only alphabet characters plus padding; anything else (whitespace, invalid input, odd lengths) takes the unchanged scalar path, so results are byte-identical. | apc-parser/kitty | 2.43× / 2.00× | 1.17× / 1.09× |
| | stream/kitty | 1.42× / 1.47× | 0.51× / 0.52× |
| **Formatter specialization** (`terminal/formatter.rs`): Zig instantiates `formatWithStateEmit` per `comptime emit` and `writeCellRun` per `comptime emit` and `comptime hyperlinks`, with `encodeCodepoint` inlined. The port passed both at runtime; they are now const generics. | formatter/plain | 1.32× / 1.33× | 0.88× / 1.05× |
| | formatter/vt | 1.85× / 1.89× | 1.18× / 1.14× |
| | formatter/html | 1.30× / 1.32× | 0.98× / 1.01× |
| **printSliceFill vectorization** (`terminal/terminal.rs`): the narrow run scan now checks 8 codepoints per step like Zig's `simd.lanes(u32)` loop, and the cell store loop bounds-checks once so it vectorizes like Zig's `simd.lanes(u64)` loop. `print_slice_fill` went from 160 M to 105 M Ir on 8 MiB of ASCII (Zig baseline 98 M). | stream/ascii | 1.66× / 1.39× | 1.39× / 0.98× |
| | stream/ascii-lines | 1.47× / 1.36× | 1.09× / 0.98× |
| | stream/styled | 1.85× / 1.25× | 1.27× / 1.05× |
| **LZ4 match search** (`terminal/compress/lz4.rs`): the per-byte search loop bounds-checked reads that its loop condition already proves in range (Zig's ReleaseFast has no checks). They are now unchecked with debug assertions: 447 M → 342 M Ir (Zig baseline 371 M). | page-compression/compress | 1.33× / 1.36× | 0.96× / 0.93× |
| | page-compression/store | 1.33× / 1.28× | 0.94× / 0.90× |
| | page-compression/decompress | 1.19× / 1.13× | 0.97× / 0.97× |
| **CRC32C at runtime** (`crc32c.rs`): the backend was chosen at compile time only, so the baseline build used software CRC while native Zig gets the SSE4.2 instruction. Baseline x86_64 builds now detect SSE4.2 at runtime. | snapshot/encode-kitty | 1.16× / 0.70× | 0.88× / 0.58× |
| **AVX2 UTF-8 decoding** (`simd/vt.rs`): Zig hands non-ASCII stream text to simdutf's AVX2 UTF-8 → UTF-32 converter; the port decoded it one codepoint at a time. A runtime-detected AVX2 block decoder now validates and converts blocks of 16 sequence starts that are entirely well-formed; ill-formed or incomplete input keeps the scalar maximal-subpart decoder. | stream/utf8 | 1.18× / 1.23× | 1.00× / 1.07× |

A second round (commits `3d101e0`..`5a81a32`) targeted the cases that
were still above 1.05×. Its before/after ratios are focused hyperfine
measurements (10–40 runs, both binaries in the same hyperfine call,
listed in each commit message); the [Results](#results) table is the
full run afterwards.

| change | cases | before | after |
|---|---|---:|---:|
| **Unchecked table lookups** (`unicode/lut.rs`, `unicode/grapheme.rs`): the static property/symbol tables are `CheckedTables`, whose stage2/stage3 indices are validated when the static is built (at compile time), so a lookup bounds-checks only the codepoint range; the grapheme-break table index is a 13-bit packed key (Zig: `packed struct(u13)`) and needs no check; `grapheme_break` is inlined as in Zig. | grapheme/table | 1.18× / 1.18× | 1.01× / 1.02× |
| **Style fill from registers** (`terminal/render.rs`): Zig's `fillStyles` writes each 28-byte `Style` as two overlapping 16-byte stores held in registers; the port's `slice::fill` reloaded the value from the stack for every cell. `fill_styles` now does what Zig does (with `MaybeUninit` chunks, so padding is copied soundly). | screen-clone/render-scroll | 1.22× / 1.18× | 1.00× / 0.97× |
| **AVX2 print path** (`terminal/terminal.rs`, `terminal/stream.rs`, `simd/vt.rs`): Zig's native build compiles the print path's portable `@Vector` loops for AVX-512, and Highway picks AVX2 for its decoder at runtime. On x86_64 CPUs with AVX2 (when the compile target lacks it) the stream chunk loop and `printSliceFill` now run instances compiled for AVX2: 32-byte ASCII widening, a 16-lane printable-run scan, and explicit AVX2 narrow-run and simple-cell scans (the 4-cell `eqlPrefix` loop had stayed scalar). The run scans locate the stop lane with a ctz like Zig's `@ctz`. The instance dispatch is inlined into `printSlice`, so either instance is one call away; the AVX2 simple-cell scan tests the first cell before its vector loop. Differential tests compare each AVX2 loop with the portable one, and an end-to-end test feeds the same random VT input through both instances (a test-only switch forces the portable one) and compares every cell. | stream/ascii | 1.38× / 0.99× | 1.00× / 0.76× |
| | stream/ascii-lines | 1.19× / 1.05× | 1.07× / 0.96× |
| | stream/styled | 1.26× / 1.02× | 1.12× / 0.90× |
| | stream/seq | 1.18× / 1.15× | 1.11× / 1.09× |
| **Integer item capacity** (`terminal/ref_counted_set.rs`): `MetaLayout.init` dominates `terminal-resize --mode=rows` on both sides with identical instruction counts, but rustc emitted a `cvtsi2sd` (int → float, no dependency-breaking xor) on its critical path for `load_factor * floatFromInt(table_cap)`. The load factor is exactly 13/16 (checked at compile time) and `table_cap` is a power of two far below 2^53, so the float product is exact and equals `table_cap * 13 / 16`, which the layout now computes. | resize/rows | 1.14× / 1.10× | 0.98× / 0.95× |
| **Codepoint-first blank test** (`terminal/formatter.rs`): rustc evaluated `cp == 0 and wide == .narrow and run_style_id == 0` branch-free for every cell; a `cold_path` hint in the `cp == 0` arm keeps text cells on one branch, as Zig's code does (439 M → 402 M Ir; Zig baseline 391 M). | formatter/vt | 1.15× / 1.14× | 1.03× / 1.02× |
| | formatter/html | 1.03× / 1.05× | 0.99× / 1.00× |
| **Store-forwarding stall in resize** (`terminal/terminal.rs`): `Terminal.resize` saves `width_px`/`height_px` for its error rollback, and rustc merged the two 4-byte reads into one 8-byte load, which cannot be forwarded from the previous call's two 4-byte stores and stalled every call. They are now two 4-byte (volatile) loads, as in Zig's code. | resize/noop | 1.09× / 1.10× | 0.66× / 0.67× |

## Analysis

### Where the Rust port is still slower

Run-to-run noise in this VM is about ±5–10 % per case, so a single row
above 1.05× is not by itself a gap: `formatter/plain` (1.07×),
`page-compression/compress` (1.05×), `snapshot/decode` (1.05×),
`stream/utf8` (1.09×) and `screen-clone/render-output` (1.09×) are within
1.10× of Zig baseline here, and focused runs of the same binaries measured
them at 0.95–1.05× against native Zig (render-output: 0.99×). The cases
with a known or suspected cause:

* **`resize/cols` (1.14× / 1.06×) and `resize/both` (1.09× / 0.99×).**
  At parity against the same ISA, and unchanged by this round (the
  pre-round binary measures the same in a shared hyperfine run). The
  remainder is native Zig's code generation; not investigated further.
* **`stream/seq` (1.14× / 1.13×).** One ~8-byte line per print call, so it
  measures the per-line cost of the stream loop, `printSlice`,
  `printSliceFill` and the linefeed/scroll path. Rust executes about 1.13×
  Zig baseline's instructions here (246 M vs 218 M on 2 MiB, leaving out the
  `memset` counting artefact), spread over many small differences: about
  40 instructions per call in `printSliceFill` (bounds checks on slices
  and `previous_char`, the `pageAssumeResident` variant check, setup), 30
  in the scroll path (one more call level, a `Limits` copy in `grow`) and
  a few in the `vt` handler, which is one function over the action enum
  where Zig instantiates one per action. Several targeted attempts
  (inlining `linefeed`/`cursorDownScroll`, field-wise pin stores, a 4-cell
  AVX2 scan tail, the wrapping range check in the scalar narrow loop)
  reduced instruction counts by 1–4 % but did not move the wall time
  beyond noise, or made it worse, and were reverted. Removing the
  remaining checks would need `unreachable_unchecked`-style changes that
  turn the port's checked invariants into undefined behaviour.
* **`stream/styled` (1.15× / 0.95×) and `stream/ascii-lines` (1.11× /
  0.97×).** At parity or better against the same ISA; the rest is native
  Zig's AVX-512 code, including its own `memset` (`quirks_memset.zig`,
  2 × 64-byte stores per iteration), which zeroes the recycled pages: 140
  MB per 8 MiB of styled input. The port uses glibc's `rep stosb`; forcing
  glibc's vector loop with `GLIBC_TUNABLES` was within noise here and
  slower on `stream/seq`. `stream/ascii` is at 1.06×.
* **Rust native is slower than Rust baseline on the print-heavy stream
  cases** (e.g. `stream/ascii` 1.35× vs 1.06×). With `-C target-cpu=native`
  the runtime AVX2 instances are compiled out (the compile target already
  has AVX2) and the portable loops are used, which LLVM vectorizes less
  well than the explicit AVX2 loops.
* **`formatter/vt` (1.07× / 0.99×) is code-layout sensitive.** Two builds
  whose `write_cell_run` is instruction-for-instruction identical (401.8 M
  Ir on 10 loops; it moved by 544 bytes because of an unrelated change in
  `resize`) measured 570 ms and 680 ms (1.07× and 1.28×) in the same
  hyperfine run. Its instruction count is now 1.03× Zig baseline's.
  Without stable function alignment control (or changing the build flags)
  this cannot be pinned down.
* **`apc-parser/kitty` (1.21× / 1.15×) and `formatter/terminal-roundtrip`
  (1.19× / 1.10×)**, 26–31 ms runs. About 1 ms of each is process start:
  the Rust `ghostty-bench` links 15 shared libraries (harfbuzz, fontconfig,
  freetype, oniguruma, glib, …; `global_init` logs their versions as Zig's
  does) that the Zig build links statically, and full RELRO binds every
  symbol at start (`startup/bench-no-data`: 2.7 ms vs 1.8 ms; `do_lookup_x`
  alone is 0.48 M Ir). The rest of `apc-parser` is the decode buffer: Zig
  decodes base64 in place, while the port allocates and zeroes a separate
  buffer (Rust slices cannot alias) — 34.3 M vs 25.6 M Ir on 8 MiB, of
  which 6.4 M is the zeroing `memset`. In-place decoding would need the
  AVX2 decoder's fallback (which restarts on the scalar path) to work on
  clobbered input. Neither was changed: static linking or lazy binding is a
  build configuration change.
* **Startup and `noop` modes (1.0–1.5×, ~1 ms).** The same dynamic loading
  cost.

### Where the Rust port is faster

* **`terminal-parser`: 0.6–0.9×.** Zig's `Parser.next` returns its action
  array (`[3]?Action`) by value. compiler_rt `memcpy` is 50 % of the Zig run
  (722 M of 1 445 M Ir). The Rust `Parser::next` avoids that copy. In the
  full stream benchmarks this parser is bypassed for printable runs, so the
  advantage does not carry over.
* **`stream/kitty`: about 0.5×.** Both sides decode base64 with AVX2 now,
  and the rest of the Rust APC path is cheaper.
* **`codepoint-width --mode=simd`: about 0.55×.** Zig calls the C++
  function `ghostty_simd_codepoint_width` (from `vt.cpp`) once per codepoint,
  so it cannot be inlined. The port's pure-Rust `simd::codepoint_width` inlines.
* **Render-state updates: `render-clean` 0.6×, `render-partial` 0.8×.** Also
  `screen-clone/noop` (0.6–0.7×) and the full-stream OSC path (`stream/osc`,
  0.68–0.77×).
* **`resize/noop`: 0.64×.** Since the store-forwarding fix, the Rust resize
  no-op path executes 64 instructions against Zig's 103.
* **Snapshot encode at baseline ISA (0.58–0.67× against Zig baseline)**,
  because the Rust build now uses the CRC32C instruction at runtime while
  Zig baseline uses its software CRC.
* **`hyperlink-map/lookup`: 0.58× against native Zig.** It is 1.05× against
  Zig baseline, so this is a Zig native code-generation effect, not a Rust
  advantage.

## Before optimization

The table below is the first comparison, before the changes listed under
[Optimizations](#optimizations), measured in an earlier session on the same
VM type with the same method (two full passes, the second shown, with
re-runs merged in), Rust side ghostty-rs `30a4945`. Its causes, found with
callgrind, were: a libc `memcpy` per cell in the snapshot codec, per-byte
out-of-line calls in the OSC parser, scalar base64, runtime-dispatched
format handling in the formatter, scalar run scans and bounds-checked
stores in `print_slice_fill`, bounds checks in the LZ4 match search, and
compile-time-only CRC32C selection.

| benchmark | Zig ms (median ± sd) | Rust ms | Rust/Zig | Rust native ms | Rust native/Zig | Zig baseline ms | Zig baseline/Zig | Rust/Zig baseline | Zig / Rust peak RSS MiB |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| stream/ascii | 87.4 ± 15.0 | 144.7 ± 17.0 | 1.66× | 135.2 ± 16.5 | 1.55× | 104.0 ± 15.0 | 1.19× | 1.39× | 6 / 6 |
| stream/ascii-lines | 151.4 ± 16.2 | 222.8 ± 22.4 | 1.47× | 218.9 ± 16.5 | 1.45× | 164.2 ± 14.6 | 1.08× | 1.36× | 6 / 6 |
| stream/seq | 1,000.5 ± 32.0 | 1,143.1 ± 55.6 | 1.14× | 1,298.1 ± 48.5 | 1.30× | 1,015.1 ± 105.8 | 1.01× | 1.13× | 6 / 7 |
| stream/utf8 | 1,267.3 ± 55.7 | 1,491.0 ± 86.3 | 1.18× | 1,423.7 ± 40.3 | 1.12× | 1,207.6 ± 53.6 | 0.95× | 1.23× | 6 / 7 |
| stream/styled | 123.8 ± 13.5 | 228.7 ± 17.4 | 1.85× | 216.0 ± 26.1 | 1.75× | 182.8 ± 26.3 | 1.48× | 1.25× | 6 / 6 |
| stream/osc | 945.2 ± 102.7 | 717.2 ± 102.3 | 0.76× | 700.2 ± 30.1 | 0.74× | 1,024.6 ± 43.7 | 1.08× | 0.70× | 7 / 8 |
| stream/kitty | 66.5 ± 11.7 | 94.6 ± 14.0 | 1.42× | 91.9 ± 14.5 | 1.38× | 64.3 ± 12.2 | 0.97× | 1.47× | 5 / 6 |
| parser/ascii | 1,304.0 ± 210.6 | 806.2 ± 58.5 | 0.62× | 805.2 ± 43.5 | 0.62× | 1,090.2 ± 52.3 | 0.84× | 0.74× | 4 / 6 |
| parser/utf8 | 1,046.5 ± 233.5 | 832.5 ± 76.8 | 0.80× | 878.2 ± 57.6 | 0.84× | 899.4 ± 149.3 | 0.86× | 0.93× | 5 / 6 |
| parser/styled | 1,271.3 ± 147.6 | 797.8 ± 66.0 | 0.63× | 791.1 ± 55.1 | 0.62× | 1,038.2 ± 53.4 | 0.82× | 0.77× | 4 / 6 |
| osc-parser | 52.7 ± 6.5 | 150.8 ± 16.9 | 2.86× | 138.2 ± 10.0 | 2.62× | 50.4 ± 4.9 | 0.96× | 2.99× | 4 / 6 |
| apc-parser/kitty | 30.1 ± 2.7 | 73.1 ± 4.5 | 2.43× | 77.3 ± 11.2 | 2.57× | 36.6 ± 5.9 | 1.22× | 2.00× | 5 / 6 |
| cp-width/noop | 2.2 ± 0.6 | 3.4 ± 0.2 | 1.54× | 3.4 ± 0.1 | 1.53× | 1.9 ± 0.3 | 0.85× | 1.81× | 4 / 6 |
| cp-width/wcwidth | 443.7 ± 24.8 | 423.3 ± 13.7 | 0.95× | 415.8 ± 36.3 | 0.94× | 464.2 ± 27.6 | 1.05× | 0.91× | 4 / 6 |
| cp-width/simd | 695.8 ± 34.9 | 389.0 ± 18.9 | 0.56× | 395.2 ± 22.8 | 0.57× | 754.4 ± 23.3 | 1.08× | 0.52× | 4 / 6 |
| cp-width/table | 399.7 ± 13.2 | 362.7 ± 25.3 | 0.91× | 398.7 ± 29.1 | 1.00× | 390.3 ± 57.7 | 0.98× | 0.93× | 4 / 6 |
| grapheme/noop | 343.5 ± 21.6 | 357.0 ± 16.0 | 1.04× | 360.7 ± 14.0 | 1.05× | 331.3 ± 19.5 | 0.96× | 1.08× | 4 / 5 |
| grapheme/table | 386.2 ± 21.3 | 459.3 ± 43.1 | 1.19× | 499.9 ± 26.4 | 1.29× | 405.5 ± 23.5 | 1.05× | 1.13× | 4 / 6 |
| is-symbol/uucode | 414.9 ± 11.2 | 395.5 ± 52.1 | 0.95× | 382.8 ± 35.6 | 0.92× | 407.1 ± 30.2 | 0.98× | 0.97× | 4 / 6 |
| is-symbol/table | 398.3 ± 29.0 | 391.3 ± 19.7 | 0.98× | 395.3 ± 18.0 | 0.99× | 388.6 ± 22.6 | 0.98× | 1.01× | 4 / 6 |
| screen-clone/noop | 48.6 ± 8.4 | 35.6 ± 11.4 | 0.73× | 32.0 ± 5.6 | 0.66× | 48.6 ± 4.0 | 1.00× | 0.73× | 6 / 7 |
| screen-clone/clone | 151.8 ± 12.4 | 150.0 ± 13.9 | 0.99× | 142.0 ± 6.9 | 0.93× | 150.6 ± 6.8 | 0.99× | 1.00× | 97 / 89 |
| screen-clone/render | 335.8 ± 27.9 | 307.7 ± 22.3 | 0.92× | 302.4 ± 33.3 | 0.90× | 327.9 ± 30.4 | 0.98× | 0.94× | 7 / 7 |
| screen-clone/render-clean | 265.8 ± 25.6 | 158.7 ± 10.1 | 0.60× | 143.1 ± 17.8 | 0.54× | 263.5 ± 20.5 | 0.99× | 0.60× | 7 / 7 |
| screen-clone/render-partial | 315.5 ± 23.9 | 238.8 ± 30.4 | 0.76× | 255.7 ± 29.8 | 0.81× | 307.4 ± 14.2 | 0.97× | 0.78× | 7 / 7 |
| screen-clone/render-scroll | 747.3 ± 25.9 | 728.9 ± 40.9 | 0.98× | 756.7 ± 66.4 | 1.01× | 772.2 ± 25.8 | 1.03× | 0.94× | 73 / 73 |
| screen-clone/render-output | 348.3 ± 19.1 | 383.5 ± 30.9 | 1.10× | 351.7 ± 14.8 | 1.01× | 378.4 ± 22.7 | 1.09× | 1.01× | 7 / 7 |
| hyperlink-map/lookup | 467.5 ± 26.2 | 341.4 ± 17.6 | 0.73× | 332.9 ± 30.9 | 0.71× | 324.6 ± 39.9 | 0.69× | 1.05× | 5 / 6 |
| hyperlink-map/churn | 343.4 ± 13.5 | 344.5 ± 16.3 | 1.00× | 337.6 ± 31.3 | 0.98× | 274.6 ± 25.1 | 0.80× | 1.25× | 5 / 6 |
| page-compression/compress | 852.8 ± 47.1 | 1,136.2 ± 102.0 | 1.33× | 1,076.3 ± 52.5 | 1.26× | 836.2 ± 24.0 | 0.98× | 1.36× | 69 / 70 |
| page-compression/store | 824.2 ± 58.2 | 1,097.9 ± 90.0 | 1.33× | 1,062.8 ± 72.6 | 1.29× | 856.0 ± 58.2 | 1.04× | 1.28× | 69 / 70 |
| page-compression/decompress | 406.7 ± 33.3 | 484.9 ± 25.8 | 1.19× | 528.6 ± 38.2 | 1.30× | 428.3 ± 29.3 | 1.05× | 1.13× | 134 / 135 |
| scrollback-compression/noop | 457.8 ± 42.2 | 513.6 ± 33.1 | 1.12× | 487.9 ± 31.8 | 1.07× | 469.6 ± 20.2 | 1.03× | 1.09× | 196 / 197 |
| scrollback-compression/compress | 623.0 ± 54.0 | 699.1 ± 34.6 | 1.12× | 731.6 ± 33.6 | 1.17× | 661.4 ± 22.7 | 1.06× | 1.06× | 196 / 197 |
| scrollback-compression/incremental | 594.5 ± 22.7 | 711.2 ± 30.1 | 1.20× | 700.9 ± 39.1 | 1.18× | 663.5 ± 31.7 | 1.12× | 1.07× | 196 / 197 |
| scrollback-compression/restore | 836.1 ± 32.5 | 865.0 ± 26.7 | 1.03× | 864.4 ± 56.4 | 1.03× | 843.4 ± 42.6 | 1.01× | 1.03× | 230 / 231 |
| formatter/noop | 8.3 ± 1.9 | 14.5 ± 1.1 | 1.74× | 11.6 ± 5.7 | 1.39× | 9.9 ± 2.1 | 1.19× | 1.46× | 11 / 12 |
| formatter/plain | 755.6 ± 29.9 | 994.2 ± 94.3 | 1.32× | 1,026.6 ± 64.3 | 1.36× | 750.3 ± 44.2 | 0.99× | 1.33× | 12 / 13 |
| formatter/vt | 431.7 ± 29.4 | 799.7 ± 39.9 | 1.85× | 828.1 ± 43.1 | 1.92× | 423.0 ± 22.9 | 0.98× | 1.89× | 12 / 13 |
| formatter/html | 1,055.4 ± 56.3 | 1,371.6 ± 68.9 | 1.30× | 1,392.0 ± 39.3 | 1.32× | 1,040.4 ± 70.0 | 0.99× | 1.32× | 12 / 13 |
| formatter/terminal-roundtrip | 19.6 ± 0.8 | 27.2 ± 3.9 | 1.39× | 32.9 ± 4.5 | 1.68× | 26.7 ± 3.2 | 1.37× | 1.02× | 20 / 20 |
| resize/noop | 344.9 ± 21.8 | 363.4 ± 35.1 | 1.05× | 372.5 ± 18.6 | 1.08× | 350.0 ± 15.8 | 1.01× | 1.04× | 22 / 23 |
| resize/cols | 178.3 ± 26.0 | 209.8 ± 19.5 | 1.18× | 168.1 ± 18.6 | 0.94× | 192.8 ± 9.6 | 1.08× | 1.09× | 24 / 24 |
| resize/cols-no-reflow | 1,778.2 ± 138.2 | 1,646.7 ± 159.7 | 0.93× | 1,635.6 ± 122.4 | 0.92× | 1,784.0 ± 77.4 | 1.00× | 0.92× | 22 / 23 |
| resize/rows | 1,213.6 ± 89.3 | 1,348.7 ± 151.2 | 1.11× | 1,202.9 ± 59.5 | 0.99× | 1,320.3 ± 106.8 | 1.09× | 1.02× | 22 / 23 |
| resize/both | 197.3 ± 12.5 | 230.5 ± 21.3 | 1.17× | 185.2 ± 13.2 | 0.94× | 203.9 ± 32.3 | 1.03× | 1.13× | 24 / 24 |
| snapshot/noop | 13.4 ± 1.9 | 15.2 ± 3.9 | 1.13× | 18.9 ± 1.7 | 1.41× | 15.1 ± 1.1 | 1.13× | 1.01× | 14 / 15 |
| snapshot/encode | 439.8 ± 43.8 | 1,808.4 ± 82.8 | 4.11× | 1,751.3 ± 92.3 | 3.98× | 678.7 ± 40.5 | 1.54× | 2.66× | 14 / 15 |
| snapshot/decode | 435.3 ± 49.3 | 1,173.3 ± 49.0 | 2.70× | 1,206.5 ± 87.1 | 2.77× | 425.6 ± 31.7 | 0.98× | 2.76× | 22 / 22 |
| snapshot/decode-ready | 143.4 ± 16.1 | 469.0 ± 30.0 | 3.27× | 444.6 ± 36.4 | 3.10× | 144.3 ± 11.0 | 1.01× | 3.25× | 14 / 15 |
| snapshot/encode-kitty | 314.7 ± 19.7 | 365.8 ± 18.2 | 1.16× | 280.3 ± 30.3 | 0.89× | 521.8 ± 37.6 | 1.66× | 0.70× | 24 / 25 |
| startup/bench-no-data | 1.9 ± 0.5 | 2.7 ± 0.9 | 1.41× | 3.5 ± 0.5 | 1.85× | 1.8 ± 0.3 | 0.96× | 1.47× | 4 / 5 |

**Summary.** The summary covers the 44 non-`noop`, non-startup cases. The
geometric mean of Rust/Zig is **1.19×**. The geometric mean of
Rust/Zig baseline (same ISA) is **1.15×**. Rust is faster than Zig in 11
cases, within ±5 % in 7, and slower in 26. Peak memory is the same on both
sides, within about 1 MiB of fixed overhead.

## Not compared

* **`ghostty +version` and other `ghostty` executable startup.** The Zig
  executable needs the GTK app runtime (see Builds). The Rust
  `--no-default-features` `ghostty` builds fine, but it has no Zig
  counterpart here. `startup/bench-no-data` stands in for it.
* **Anything GUI or renderer-side.** This covers font shaping, Metal/OpenGL
  and the GTK app. It is out of scope for `ghostty-bench`.
* **`ghostty-gen` throughput.** Both generators exist, but only the Zig one is
  used, so that both sides read identical bytes. Generator speed is not part
  of the comparison.
* **`page-compression --mode=report`, `scrollback-compression --mode=report`,
  `terminal-snapshot --mode=report` and `terminal-formatter --mode=report`.**
  These modes print statistics. They are not timing modes.

## Reproducing

```sh
# Zig 0.17.0 from https://ziglang.org/download/index.json
tools/bench_compare.sh --zig /path/to/zig --rust-native --zig-baseline \
    --work /tmp/ghostty-bench-compare
# Re-print the table, or re-run a subset:
tools/bench_compare.sh --work /tmp/ghostty-bench-compare --report-only
tools/bench_compare.sh --work /tmp/ghostty-bench-compare --skip-build --skip-gen \
    --runs 20 --filter '^snapshot/'
```

Behind an HTTPS proxy that Zig's package fetcher cannot traverse (as in this
environment), download the URLs listed in `ghostty/build.zig.zon.txt` with
curl into a directory and pass it as `--deps-dir`. A file can also be named
`<zig-hash>.tar.gz`, or be a `<zig-hash>/` directory. `z2d` comes from a
GitHub archive URL, and this session could not reach it. A source copy whose
`zig fetch` hash matched the pin was used instead. `aro` (a git dependency
of `translate_c` on codeberg.org) cannot be fetched either; unpack a source
tarball of the pinned commit into `$WORK/ghostty-src/zig-pkg/<zig-hash>` by
hand, the same way. Zig's compile of the `ghostty` module needs about 11 GB
of memory; use `--jobs 1` on a 15 GB machine, and build nothing else at the
same time, or the compiler is OOM-killed. `gobject` is a `.tar.zst`
file and needs `zstd`. It is only needed for the GTK runtime. Put
`hyperfine` on `PATH`, or point `HYPERFINE=` at it. Without it, the script
falls back to a Python timer with the same warmup and run counts.
