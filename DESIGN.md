# netmon design

## Scope

`netmon` is a Linux-only terminal monitor. It reads kernel counters, converts
counter deltas into rates, and renders the current state with ratatui. The
program deliberately has no daemon, configuration file, database, or network
service.

## Runtime architecture

```text
main()
  └─ run_app()
       ├─ ui()                         render current App state
       ├─ keyboard/mouse events        mutate view state
       └─ App::tick() every 500 ms
            ├─ /proc/net/dev           interface byte counters
            ├─ /proc/stat              system CPU counters
            ├─ /proc/meminfo           memory and swap percentages
            ├─ /proc/diskstats         system disk sectors
            ├─ /proc/mounts + statvfs  disk-space usage
            └─ ConnMonitor::sample()   ss sockets and process metrics
                                      └─ async reverse DNS cache
```

The crate is split into four source responsibilities:

- `src/main.rs` owns application state, interface counters, sampling cadence,
  event handling, and process startup.
- `src/metrics.rs` owns Linux system-counter readers (`/proc`, `/sys`, mounts,
  and `statvfs`) without owning history or rendering state.
- `src/conns.rs` owns socket parsing, per-connection rates, process aggregation,
  process resource reads, service lookup, and the connection sampler worker.
- `src/ui.rs` owns waveform rendering, metric layout, table rendering, sorting,
  and terminal interaction presentation.

`App` is the single owner of persistent runtime state. Histories use bounded
`VecDeque`s, with the newest sample at the back. A reset or interface switch
clears histories and all delta baselines that cannot be safely carried over.

## Sampling and rate rules

The event loop targets a 500 ms interval (2 Hz). Each rate is calculated as:

```text
rate = (current_counter - previous_counter) / elapsed_seconds
```

The first observation only establishes a baseline. Saturating subtraction keeps
counter resets from producing a huge spike. If the active interface disappears,
its displayed rate is set to zero and its baseline is removed.

Automatic interface selection accumulates RX+TX deltas for a one-second window.
It switches only when another interface is ahead by more than 4096 bytes and
the debounce interval has elapsed. Manual interface selection disables this
mode until the user re-enables it.

System disk throughput sums only whole-device rows present in `/sys/block` so
partition counters are not double-counted. If the device list is unavailable,
the metric is treated as unavailable. Disk-space usage prefers the largest
mounted `/dev` filesystem and uses `/` as a useful overlay/container fallback.

## Connection monitor

`ConnMonitor::sample()` submits work to a dedicated sampler thread, which runs
`ss -tunpi` and reads process data. The UI consumes the latest completed
snapshot without waiting for the subprocess or `/proc/<pid>` reads. Established
TCP sockets are measured from cumulative byte counters; UDP rows are retained
for context but have no per-socket rate. Process information is read only for
PIDs visible in the current sample, then grouped into the optional aggregate
view. Generation IDs prevent a snapshot from before a reset being applied after
that reset.

The same worker reads active `USER_PROCESS` records from utmp, scans `/proc`
for processes whose controlling terminal matches an active `pts/N`, and reads
`/var/log/lastlog` for historical login times. Sessions with the same username
are collapsed into one row; the displayed login time is the newest value across
utmp and lastlog. When utmp is unavailable, sessions are inferred from running
processes with a `pts/N` controlling terminal. Accounts with no lastlog record
are omitted when offline. A completed command or shell builtin is not
recoverable from `/proc`, so the process column describes the newest process
that is still running.

Reverse DNS is the only asynchronous lookup. Results are sent back through a
channel and cached by IP. Service names are loaded from `/etc/services`.

Reverse DNS remains independently asynchronous inside the sampler, while the
UI-side `ConnMonitor` owns only the latest snapshot, filtering, sorting, and
scroll state.

## Rendering

The top panel contains eight equal-height metric rows. Each waveform is drawn as
a thin braille line. Only screen-visible horizontal columns are sampled from the
history, so a full 24-hour buffer does not require a full-size normalized and
smoothed temporary series on every frame. The X-axis stretches partial history
across the chart and scrolls once the bounded history is full.

Terminals shorter than 46 rows automatically show the connection panel in full
screen mode. This prevents the fixed metric layout from compressing the table
into an unusable area. The explicit `c` focus toggle uses the same panel layout.

## Failure handling

- A failed `/proc` read produces an unavailable metric or a zero sample where a
  history value is required; it does not terminate the monitor.
- If `ss` is missing or exits unsuccessfully, the connection panel displays an
  availability message while system metrics and the online-user panel continue
  updating. A reset marks the monitor available again so a later successful
  invocation can recover.
- Hostname lookup failures are cached as misses and never block rendering.

## Verification

The test suite covers parser behavior, aggregation and filtering, sort order,
headless rendering modes, interface auto-selection, and unavailable states.
Recommended checks before merging changes are:

```sh
cargo fmt -- --check
cargo test --target x86_64-unknown-linux-musl --all-targets
cargo clippy --target x86_64-unknown-linux-musl --all-targets -- -D warnings
cargo build --release --target x86_64-unknown-linux-musl
```
