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
./scripts/run-linux-bench.sh \
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

### Lock-Only Load Benchmark

Command:

```bash
./scripts/run-linux-bench.sh \
  --duration-ms 2000 \
  --warmup-ms 500 \
  --threads 1,2,4,8,16 \
  --shards 1,16,64 \
  --mixes 80-20,50-50,20-80 \
  --latency-sample-rate 512
```

16 threads, one hot shard:

| Mix | rblock ops/sec | parking_lot ops/sec | rblock p999 | parking_lot p999 |
| --- | ---: | ---: | ---: | ---: |
| 80/20 | 22.40M | 12.92M | 58.0us | 69.8us |
| 50/50 | 20.78M | 9.36M | 54.4us | 77.3us |
| 20/80 | 24.26M | 7.73M | 58.0us | 88.3us |

16 threads, 16 shards:

| Mix | rblock ops/sec | parking_lot ops/sec | rblock p999 | parking_lot p999 |
| --- | ---: | ---: | ---: | ---: |
| 80/20 | 87.47M | 94.19M | 1.4us | 1.9us |
| 50/50 | 102.64M | 103.54M | 1.6us | 2.0us |
| 20/80 | 120.76M | 119.56M | 1.5us | 1.9us |

16 threads, 64 shards:

| Mix | rblock ops/sec | parking_lot ops/sec | rblock p999 | parking_lot p999 |
| --- | ---: | ---: | ---: | ---: |
| 80/20 | 204.83M | 195.43M | 740ns | 670ns |
| 50/50 | 211.84M | 209.63M | 640ns | 640ns |
| 20/80 | 230.00M | 232.46M | 610ns | 590ns |

The single-shard case shows where the read-biased policy buys substantial
aggregate throughput under contention. Once contention is spread over many
shards, the result is much closer: `rblock` is competitive, but it is not a
universal throughput win over `parking_lot`.

### Writer Progress Warning

Lock-only writer-progress harness, 16 reader threads in a tight loop, one writer
thread repeatedly trying to acquire the write lock, zero synthetic work.

This is not a fixed offered-rate write workload. The writer thread runs as fast
as the lock lets it, so `writer acq/s` is a progress signal under reader
pressure. The latency columns are the main reason this benchmark exists.

Command:

```bash
BENCH=writer_progress \
./scripts/run-linux-bench.sh \
  --duration-ms 2000 \
  --warmup-ms 500 \
  --readers 1,2,4,8,16
```

| Readers | rblock writer acq/s | parking_lot writer acq/s | rblock p999 | parking_lot p999 |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 1.10M | 5.41M | 12.1us | 570ns |
| 2 | 220.0k | 4.20M | 53.9us | 1.4us |
| 4 | 125.9k | 2.36M | 65.0us | 5.1us |
| 8 | 10.3k | 1.41M | 1.4ms | 12.3us |
| 16 | 9 | 658.6k | 230.7ms | 46.6us |

This is the sharp edge. If writer progress or write p999 is the primary SLO,
use a fair lock policy.

### What These Numbers Mean

The load benchmark compares lock policy while keeping the synthetic sharded
design the same. It measures aggregate operation latency, not separate reader
and writer wait time.

That distinction matters most when readers are continuous. A read-biased lock
can raise aggregate throughput while making writer progress much worse. A fair
lock can improve writer tails at a throughput cost. A purpose-built concurrent
map may beat both if its exclusive path does less work than your full
cache/store path.

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
