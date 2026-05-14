# rblock

A small `lock_api`-compatible read-biased `RwLock` for sharded, read-heavy
cache workloads.

This crate is intentionally narrow. It is not a general replacement for
`parking_lot::RwLock`; it is a policy choice for very short cache-shard critical
sections where read throughput matters more than bounded writer progress.

## What It Optimizes

The reader fast path only checks for an active writer. Waiting writers do not
close a reader gate, so new readers can keep entering while a writer is queued.
That avoids reader convoys in read-heavy mixed workloads.

The tradeoff is fairness. A constant stream of readers can delay a writer much
longer than a fair lock would. Use this when that tradeoff is explicit and
measured.

## When To Use It

| Workload or system shape | Recommendation |
| --- | --- |
| Read-heavy sharded cache, very short read/write critical sections | Use `rblock::RwLock` |
| Mostly reads with occasional small writes, latency target is read-side p99/p999 | Use `rblock::RwLock` |
| Write-heavy, skewed, or writer-tail-sensitive workload | Prefer `parking_lot::RwLock` or a fair policy |
| Long critical sections, IO while locked, blocking work while locked | Do not use this lock |
| General shared hash map where you want a ready-made concurrent map | Consider DashMap |
| Need bounded writer progress as a correctness or SLO property | Use `parking_lot::RwLock` |

## API

```rust
use rblock::RwLock;

let lock = RwLock::new(0usize);

{
    let value = lock.read();
    assert_eq!(*value, 0);
}

{
    let mut value = lock.write();
    *value += 1;
}
```

The exported guard types are:

- `RwLock<T>`
- `RwLockReadGuard<'_, T>`
- `RwLockWriteGuard<'_, T>`

## Benchmarks

This crate includes lock-only benchmarks. Run them directly from the crate root:

```bash
cargo bench --bench rwlock_load -- \
  --duration-ms 500 \
  --warmup-ms 150 \
  --threads 1,2,4,8,16 \
  --shards 1,16,64 \
  --mixes 100-0,95-5,80-20,50-50,20-80,0-100 \
  --latency-sample-rate 1024

cargo bench --bench writer_progress -- \
  --duration-ms 500 \
  --warmup-ms 150 \
  --readers 1,2,4,8,16
```

For Linux runs, use the crate-local harness. It syncs only this crate into a
temporary workspace on the server:

```bash
./scripts/run-linux-bench.sh

BENCH=writer_progress \
./scripts/run-linux-bench.sh -- \
  --duration-ms 500 \
  --warmup-ms 150 \
  --readers 1,2,4,8,16
```

For application-level A/B testing, compare your sharded data structure with this
lock against the same data structure using a fair lock. Record both throughput
and latency, including p50, p99, p999, read p999, and write p999. That matters:
this lock can improve ops/sec and read tails while making writer tails worse
under continuous readers.

## Representative Results

These are representative Linux results from May 14, 2026. They are
workload-specific; rerun the harness for your hardware and key distribution
before making a production decision.

### Shared cache lock policy

512B values, uniform keys, 16 clients, 16 vCPU budget:

| Mix | Read-biased ops/sec | Parking-lot ops/sec | Read-biased p999 | Parking-lot p999 |
| --- | ---: | ---: | ---: | ---: |
| 80/20 | 28.0M | 26.2M | 6.2us | 12.9us |
| 50/50 | 19.6M | 17.4M | 13.9us | 28.6us |
| 20/80 | 16.3M | 14.0M | 15.9us | 34.9us |

Across the broader shared-cache A/B suite used during development,
read-biased won 22 of 24 sharded-cache rows versus parking_lot. The largest
wins showed up in read-heavy or mixed shared-handle workloads.

### Writer Progress Warning

Lock-only writer-progress harness, 16 reader threads in a tight loop, one writer
thread repeatedly trying to acquire the write lock, zero synthetic work.

This is not a fixed offered-rate write workload. The writer thread runs as fast
as the lock lets it, so `writer acq/s` is a progress signal under reader
pressure. The latency columns are the main reason this benchmark exists.

| Lock | Writer acq/s | Writer p999 |
| --- | ---: | ---: |
| rblock | 2 | 246.7ms |
| parking_lot | 690k | 39.6us |

This is the sharp edge. If writer progress or write p999 is the primary SLO,
use a fair lock policy.

### What These Numbers Mean

The tables above compare lock policy while keeping the surrounding sharded
design the same. They do not claim that this crate is faster than every
concurrent map or every cache layout.

That distinction matters most under write-heavy Zipf skew. A read-biased lock
can raise aggregate throughput while making write p999 worse. A fair lock can
improve writer tails at a throughput cost. A purpose-built concurrent map may
beat both if its exclusive path does less work than your full cache/store path.

Use these results to decide whether `rblock` is worth testing in your design.
Use your own application benchmark to decide whether it is worth shipping.

## Production Guidance

Use this lock as a measured optimization, not as a default synchronization
primitive. The best fit is a striped cache where:

- shard count is high enough to spread contention,
- reads dominate,
- critical sections are CPU-only and very short,
- write latency is allowed to be less fair than read latency,
- benchmarks cover the real read/write mix and key distribution.

For write-heavy workloads, especially Zipf-skewed workloads, use the fair
`parking_lot` policy or a data structure with a smaller write path.
