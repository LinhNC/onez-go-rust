# Performance benchmark

The release profile is Linux, 4 vCPU, 2 GiB RAM, local SSD/NVMe, one million seeded links, analytics enabled, 200 concurrent keep-alive clients, and a 60-second warmed run.

Seed a dedicated database:

```sh
cargo run --release -p shorten-cli --bin seed \
  -database .data/benchmark.db -count 1000000
```

Start the service against it, warm the target link, then run:

```sh
cargo run --release -p shorten-cli --bin bench \
  -target http://127.0.0.1:8080/00000000 \
  -duration 60s \
  -concurrency 200
```

The harness fails unless it observes at least 5,000 requests/second, p95 at or below 10 ms, p99 at or below 25 ms, and under 0.1% unexpected responses. Capture its output together with the commit, host CPU/RAM/storage, service configuration, `/metrics`, and query plans.

For management queries, run `EXPLAIN QUERY PLAN` for newest/updated/visits/slug/destination list orders, status/expiration filters and FTS search. Integration tests assert the default updated list index; release evidence must record all production-sized plans and p95 below 50 ms.

Run the repeatable HTTP query harness against the seeded service:

```sh
cargo run --release -p shorten-cli --bin querybench \
  -base-url http://127.0.0.1:8080 \
  -api-key "$SHORTEN_ADMIN_API_KEY" \
  -iterations 200
```

Filtered and full-text queries intentionally omit an exact total count. They return `total: null` in JSON and “Filtered links” in the management UI so the first keyset page does not require a full-dataset count.

## Recorded evidence

### Go baseline

The 2026-08-10 development run used commit `ae8a5d7` plus the working-tree migration, toolchain `go=go1.26.3`, macOS/arm64, a local SSD, one million deterministic rows, 12 SQLite read connections, and the default WAL/durability settings. This is useful tuning evidence, but it does not replace the required Linux 4-vCPU/2-GiB staging sign-off.

Seed creation completed in 54.142 seconds. The warmed 60-second redirect run used 200 keep-alive clients and completed 7,636,795 requests at 127,277 requests/second, p95 3.487 ms, p99 5.295 ms, with zero unexpected responses.

| Management query | p50 | p95 | p99 |
| --- | ---: | ---: | ---: |
| Updated | 0.342 ms | 0.573 ms | 0.730 ms |
| Active | 0.244 ms | 0.387 ms | 0.467 ms |
| Expired | 0.101 ms | 0.132 ms | 0.177 ms |
| Visits | 0.362 ms | 1.260 ms | 2.760 ms |
| Destination | 0.301 ms | 0.493 ms | 0.652 ms |
| FTS | 22.743 ms | 24.099 ms | 39.857 ms |

All 1,200 management benchmark requests returned HTTP 200, and every measured p95 remained below the 50 ms budget.

### Rust run

Record one block per release sign-off. The budgets are unchanged, so a run that misses any budget is a release blocker, not a tuning note.

| Field | Value |
| --- | --- |
| commit | |
| toolchain | `rustc=` / `cargo=` |
| host CPU/RAM/storage | |
| `SHORTEN_READ_CONNECTIONS` | |
| cache and analytics settings | |
| seed duration | |
| requests completed | |
| requests/second | |
| redirect p95 / p99 | |
| unexpected responses | |
| query plans | |
| management p50 / p95 / p99 | |

FTS is the slowest management path on both implementations and the first place a Rust regression shows up: compare its p95 against the 24.099 ms baseline before signing off.
