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
| Rust side | ghostty-rs `ef30aaa` (after the optimizations below; the [before](#before-optimization) numbers are from `30a4945`), rustc 1.97.0 (2d8144b78 2026-07-07) |
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
| SIMD | `-Dsimd=true` (default): C++ Highway and simdutf, which pick AVX2 at runtime | pure Rust `std::arch`/`memchr`: SSE2 (baseline) code, plus AVX2 base64 and UTF-8 decoding and SSE4.2 CRC32C selected at runtime |
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
  with 3 warmups and 20 runs and merged in, and the startup case uses 80 runs.
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
| stream/ascii | 68.7 ± 3.9 | 95.7 ± 11.3 | 1.39× | 82.9 ± 5.5 | 1.21× | 97.6 ± 10.4 | 1.42× | 0.98× | 6 / 7 |
| stream/ascii-lines | 136.7 ± 13.1 | 148.7 ± 16.7 | 1.09× | 150.7 ± 9.4 | 1.10× | 151.4 ± 12.7 | 1.11× | 0.98× | 6 / 7 |
| stream/seq | 1,119.7 ± 86.5 | 1,465.0 ± 62.9 | 1.31× | 1,360.1 ± 43.6 | 1.21× | 1,189.6 ± 51.4 | 1.06× | 1.23× | 5 / 7 |
| stream/utf8 | 1,434.5 ± 83.0 | 1,435.4 ± 38.8 | 1.00× | 1,391.6 ± 52.4 | 0.97× | 1,336.5 ± 46.2 | 0.93× | 1.07× | 6 / 7 |
| stream/styled | 132.3 ± 9.2 | 167.6 ± 7.2 | 1.27× | 166.9 ± 12.6 | 1.26× | 160.3 ± 10.2 | 1.21× | 1.05× | 6 / 7 |
| stream/osc | 1,023.4 ± 12.3 | 790.7 ± 70.9 | 0.77× | 850.5 ± 28.4 | 0.83× | 1,226.0 ± 76.8 | 1.20× | 0.64× | 7 / 8 |
| stream/kitty | 79.0 ± 8.1 | 40.7 ± 4.4 | 0.51× | 38.5 ± 6.2 | 0.49× | 77.5 ± 15.7 | 0.98× | 0.52× | 5 / 6 |
| parser/ascii | 1,286.5 ± 48.3 | 799.5 ± 36.3 | 0.62× | 849.0 ± 479.0 | 0.66× | 1,056.9 ± 55.6 | 0.82× | 0.76× | 4 / 6 |
| parser/utf8 | 971.8 ± 38.9 | 897.5 ± 58.4 | 0.92× | 813.1 ± 89.3 | 0.84× | 1,011.7 ± 65.7 | 1.04× | 0.89× | 4 / 6 |
| parser/styled | 1,338.6 ± 102.4 | 781.8 ± 21.2 | 0.58× | 752.8 ± 30.8 | 0.56× | 1,056.1 ± 163.4 | 0.79× | 0.74× | 4 / 6 |
| osc-parser | 60.8 ± 3.1 | 55.3 ± 10.1 | 0.91× | 63.1 ± 7.3 | 1.04× | 60.1 ± 8.0 | 0.99× | 0.92× | 4 / 6 |
| apc-parser/kitty | 23.1 ± 2.5 | 27.1 ± 1.9 | 1.17× | 25.8 ± 1.5 | 1.11× | 24.8 ± 3.4 | 1.07× | 1.09× | 5 / 6 |
| cp-width/noop | 1.6 ± 0.7 | 2.4 ± 1.4 | 1.46× | 2.3 ± 0.3 | 1.38× | 1.5 ± 0.3 | 0.94× | 1.56× | 4 / 6 |
| cp-width/wcwidth | 460.3 ± 21.1 | 465.5 ± 24.1 | 1.01× | 468.3 ± 27.5 | 1.02× | 474.0 ± 18.0 | 1.03× | 0.98× | 4 / 6 |
| cp-width/simd | 749.9 ± 30.2 | 406.4 ± 20.4 | 0.54× | 386.2 ± 18.4 | 0.52× | 778.0 ± 25.9 | 1.04× | 0.52× | 4 / 6 |
| cp-width/table | 417.0 ± 12.4 | 378.8 ± 21.2 | 0.91× | 389.5 ± 21.2 | 0.93× | 414.0 ± 19.7 | 0.99× | 0.92× | 4 / 6 |
| grapheme/noop | 356.3 ± 21.3 | 377.8 ± 21.9 | 1.06× | 380.8 ± 15.7 | 1.07× | 371.3 ± 19.7 | 1.04× | 1.02× | 4 / 6 |
| grapheme/table | 411.6 ± 26.6 | 461.1 ± 20.0 | 1.12× | 467.6 ± 22.5 | 1.14× | 386.1 ± 18.1 | 0.94× | 1.19× | 4 / 6 |
| is-symbol/uucode | 514.3 ± 90.0 | 492.3 ± 65.4 | 0.96× | 512.7 ± 52.1 | 1.00× | 457.1 ± 30.2 | 0.89× | 1.08× | 4 / 6 |
| is-symbol/table | 388.2 ± 18.3 | 377.7 ± 11.7 | 0.97× | 407.0 ± 21.2 | 1.05× | 398.8 ± 20.5 | 1.03× | 0.95× | 4 / 6 |
| screen-clone/noop | 41.1 ± 2.0 | 28.8 ± 2.1 | 0.70× | 34.1 ± 3.6 | 0.83× | 46.8 ± 7.7 | 1.14× | 0.61× | 6 / 7 |
| screen-clone/clone | 170.0 ± 7.9 | 165.8 ± 10.4 | 0.98× | 166.2 ± 19.8 | 0.98× | 169.9 ± 9.7 | 1.00× | 0.98× | 97 / 98 |
| screen-clone/render | 379.7 ± 26.0 | 348.8 ± 16.9 | 0.92× | 367.7 ± 32.4 | 0.97× | 391.3 ± 22.7 | 1.03× | 0.89× | 6 / 7 |
| screen-clone/render-clean | 280.5 ± 25.4 | 161.3 ± 20.0 | 0.58× | 180.4 ± 12.0 | 0.64× | 260.6 ± 16.5 | 0.93× | 0.62× | 6 / 7 |
| screen-clone/render-partial | 346.0 ± 19.8 | 259.0 ± 11.6 | 0.75× | 261.1 ± 12.6 | 0.75× | 309.1 ± 18.4 | 0.89× | 0.84× | 6 / 7 |
| screen-clone/render-scroll | 900.0 ± 30.0 | 1,042.6 ± 44.0 | 1.16× | 1,054.1 ± 43.1 | 1.17× | 886.5 ± 39.6 | 0.98× | 1.18× | 73 / 73 |
| screen-clone/render-output | 381.1 ± 11.8 | 404.5 ± 23.4 | 1.06× | 408.7 ± 27.0 | 1.07× | 385.9 ± 21.4 | 1.01× | 1.05× | 6 / 7 |
| hyperlink-map/lookup | 597.5 ± 14.3 | 373.7 ± 22.5 | 0.63× | 358.9 ± 13.8 | 0.60× | 331.2 ± 18.4 | 0.55× | 1.13× | 4 / 6 |
| hyperlink-map/churn | 366.4 ± 21.0 | 318.6 ± 29.5 | 0.87× | 391.5 ± 29.9 | 1.07× | 310.9 ± 25.2 | 0.85× | 1.02× | 4 / 6 |
| page-compression/compress | 905.1 ± 62.1 | 873.3 ± 35.7 | 0.96× | 880.4 ± 24.9 | 0.97× | 937.0 ± 39.0 | 1.04× | 0.93× | 69 / 70 |
| page-compression/store | 934.0 ± 41.4 | 882.0 ± 56.6 | 0.94× | 874.2 ± 67.7 | 0.94× | 979.2 ± 68.0 | 1.05× | 0.90× | 69 / 70 |
| page-compression/decompress | 454.9 ± 24.3 | 439.6 ± 34.4 | 0.97× | 428.6 ± 9.3 | 0.94× | 455.2 ± 72.3 | 1.00× | 0.97× | 134 / 135 |
| scrollback-compression/noop | 497.8 ± 43.8 | 483.3 ± 114.8 | 0.97× | 443.4 ± 21.1 | 0.89× | 524.6 ± 79.8 | 1.05× | 0.92× | 196 / 197 |
| scrollback-compression/compress | 647.9 ± 46.0 | 645.8 ± 15.1 | 1.00× | 670.9 ± 67.5 | 1.04× | 716.8 ± 52.1 | 1.11× | 0.90× | 196 / 197 |
| scrollback-compression/incremental | 651.9 ± 50.2 | 653.4 ± 15.6 | 1.00× | 650.3 ± 41.0 | 1.00× | 654.7 ± 47.1 | 1.00× | 1.00× | 196 / 197 |
| scrollback-compression/restore | 802.5 ± 25.1 | 770.4 ± 18.1 | 0.96× | 778.1 ± 41.1 | 0.97× | 864.3 ± 132.2 | 1.08× | 0.89× | 230 / 231 |
| formatter/noop | 8.9 ± 1.4 | 10.6 ± 0.9 | 1.19× | 10.8 ± 11.0 | 1.21× | 9.8 ± 1.0 | 1.09× | 1.09× | 11 / 12 |
| formatter/plain | 933.6 ± 104.7 | 822.8 ± 26.9 | 0.88× | 752.3 ± 28.2 | 0.81× | 783.9 ± 32.1 | 0.84× | 1.05× | 12 / 13 |
| formatter/vt | 470.4 ± 27.3 | 553.8 ± 27.4 | 1.18× | 701.9 ± 25.5 | 1.49× | 484.2 ± 23.6 | 1.03× | 1.14× | 12 / 13 |
| formatter/html | 1,091.4 ± 46.6 | 1,070.3 ± 63.1 | 0.98× | 1,156.9 ± 103.3 | 1.06× | 1,064.5 ± 37.6 | 0.98× | 1.01× | 12 / 13 |
| formatter/terminal-roundtrip | 24.9 ± 4.8 | 28.0 ± 5.2 | 1.13× | 27.9 ± 3.4 | 1.12× | 25.4 ± 6.6 | 1.02× | 1.10× | 20 / 20 |
| resize/noop | 336.9 ± 30.6 | 368.2 ± 16.9 | 1.09× | 359.2 ± 26.9 | 1.07× | 334.8 ± 16.1 | 0.99× | 1.10× | 22 / 23 |
| resize/cols | 178.2 ± 19.4 | 198.0 ± 9.7 | 1.11× | 183.5 ± 16.3 | 1.03× | 193.2 ± 11.3 | 1.08× | 1.02× | 24 / 25 |
| resize/cols-no-reflow | 1,432.5 ± 58.5 | 1,357.8 ± 45.3 | 0.95× | 1,342.3 ± 33.5 | 0.94× | 1,370.8 ± 45.8 | 0.96× | 0.99× | 22 / 23 |
| resize/rows | 1,330.3 ± 42.8 | 1,516.8 ± 56.0 | 1.14× | 1,317.5 ± 24.4 | 0.99× | 1,374.0 ± 58.2 | 1.03× | 1.10× | 22 / 23 |
| resize/both | 194.3 ± 15.2 | 194.2 ± 15.0 | 1.00× | 170.7 ± 14.3 | 0.88× | 191.3 ± 12.3 | 0.98× | 1.02× | 24 / 24 |
| snapshot/noop | 12.2 ± 1.0 | 14.5 ± 3.4 | 1.19× | 14.1 ± 5.7 | 1.15× | 13.9 ± 2.7 | 1.14× | 1.04× | 14 / 15 |
| snapshot/encode | 373.9 ± 14.1 | 388.5 ± 41.5 | 1.04× | 297.9 ± 16.7 | 0.80× | 579.9 ± 32.7 | 1.55× | 0.67× | 14 / 15 |
| snapshot/decode | 396.8 ± 17.2 | 420.6 ± 20.3 | 1.06× | 367.8 ± 20.8 | 0.93× | 425.0 ± 16.8 | 1.07× | 0.99× | 21 / 23 |
| snapshot/decode-ready | 149.2 ± 14.3 | 171.4 ± 13.5 | 1.15× | 132.6 ± 15.8 | 0.89× | 141.5 ± 8.1 | 0.95× | 1.21× | 14 / 15 |
| startup/bench-no-data | 1.7 ± 0.5 | 2.4 ± 0.6 | 1.38× | 2.3 ± 0.1 | 1.33× | 1.6 ± 0.3 | 0.93× | 1.49× | 4 / 6 |
| snapshot/encode-kitty | 288.1 ± 50.9 | 253.6 ± 11.8 | 0.88× | 251.3 ± 12.4 | 0.87× | 438.9 ± 30.8 | 1.52× | 0.58× | 23 / 25 |

The exact flags for each case are in the `CASES` array of
`tools/bench_compare.sh`.

**Summary.** The summary covers the 44 non-`noop`, non-startup cases. The
geometric mean of Rust/Zig is **0.94×** (1.19× before optimization). The
geometric mean of Rust/Zig baseline (same ISA) is **0.93×** (1.15× before).
Rust native/Zig is 0.93×. Rust is faster than Zig in 17 cases, within ±5 %
in 13, and slower in 14. Peak memory is the same on both sides, within about
1–2 MiB of fixed overhead.

### Binary sizes

| binary | Zig bytes | Rust bytes | Zig stripped | Rust stripped |
|---|---:|---:|---:|---:|
| ghostty-bench | 67,925,960 | 2,950,680 | 9,620,344 | 2,378,224 |
| ghostty-gen | 6,475,896 | 921,776 | 570,424 | 636,744 |

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
1.7 ms and Rust 2.4 ms. Both are close to the timer's resolution.

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

## Analysis

### Where the Rust port is still slower

* **`stream/ascii` and `stream/styled` against native Zig (1.27–1.39×).**
  Like-for-like they are at parity (0.98× and 1.05× against Zig baseline).
  The remaining gap is AVX-512 code generation in native Zig: Zig baseline
  itself is 1.2–1.4× slower than native Zig here, while
  `-C target-cpu=native` gains Rust only 0–13 %.
* **`stream/seq` (1.31× / 1.23×).** `seq` output is one short line (about 8
  bytes) per print call, so it measures the per-call cost of the print path
  (`print_slice`, `print_slice_fill` setup, the LF handler and
  `Stream::next_slice`). Leaving out the `memset` artefact, Rust executes
  about 1.2× the instructions of Zig baseline here (984 M vs 830 M Ir on
  8 MiB), spread over many small per-call differences rather than one hot
  spot. This benchmark did not change with the optimizations above (the
  pre-optimization binary measures 1.24× / 1.21× in this session); the
  earlier table's 1.14× was a quieter run.
* **`screen-clone/render-scroll` (1.16× / 1.18×), `resize/rows`
  (1.14× / 1.10×) and `grapheme/table` (1.12× / 1.19×).** Unchanged by these
  optimizations: side by side in this session the pre-optimization binary
  measures 1.16×, 1.10× and 1.14× against Zig, the current one 1.14×, 1.07×
  and 1.17×. The first two were within noise in the earlier run. Not
  investigated further.
* **`formatter/vt` (1.18× / 1.14×).** After specialization the per-cell loop
  is structurally the same as Zig's `writeCellRun`, but it executes about
  63 Ir per emitted cell against Zig's 53 (777 M vs 662 M on 20 loops),
  mostly in packed-field extraction. The native Rust build is slower still
  (1.49×): LLVM's AVX-512 code for this loop is worse than its SSE2 code.
* **`apc-parser/kitty` (1.17× / 1.09×).** The base64 decoding now matches
  simdutf; the remaining difference (about 4 ms of 27 ms) is per-command
  allocation and zeroing of the decode buffer (Zig decodes in place, which
  Rust slices cannot alias) and process overhead. Removing the zeroing in
  an experiment did not change the wall time measurably.
* **`snapshot/decode-ready` (1.15× / 1.21×).** A 150 ms case at the edge
  of the noise: a separate 10-run measurement in the same session gave
  1.04× / 1.06×. Its decode loop now matches Zig's fixed-width stores.
* **Startup and `noop` modes (1.1–1.5×, 0.7–1 ms).** Process start and
  reading the data file; at the timer's resolution.

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
* **Render-state updates: `render-clean` 0.6×, `render-partial` 0.75×.** Also
  `screen-clone/noop` (0.7×) and the full-stream OSC path (`stream/osc`,
  0.64–0.77×).
* **Snapshot encode at baseline ISA (0.58–0.67× against Zig baseline)**,
  because the Rust build now uses the CRC32C instruction at runtime while
  Zig baseline uses its software CRC.
* **`hyperlink-map/lookup`: 0.63× against native Zig.** It is 1.13× against
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
