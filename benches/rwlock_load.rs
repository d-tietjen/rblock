use std::env;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use rblock::RwLock;

const BATCH: usize = 64;

#[derive(Debug, Clone)]
struct Args {
    duration: Duration,
    warmup: Duration,
    latency_sample_rate: usize,
    threads: Vec<usize>,
    shards: Vec<usize>,
    mixes: Vec<Mix>,
}

#[derive(Debug, Clone, Copy)]
struct Mix {
    label: &'static str,
    read_pct: u8,
}

#[repr(align(128))]
struct Slot {
    lock: RwLock<usize>,
}

fn main() {
    let args = Args::parse();

    println!(
        "rwlock_load: duration={}ms warmup={}ms latency_sample_rate={}",
        args.duration.as_millis(),
        args.warmup.as_millis(),
        args.latency_sample_rate,
    );
    println!();
    println!(
        "| {:>7} | {:>6} | {:>8} | {:>14} | {:>14} | {:>10} | {:>10} | {:>10} | {:>8} |",
        "threads", "shards", "mix", "ops/sec", "ops/thread/sec", "p50", "p99", "p99.9", "samples"
    );
    println!(
        "| {:>7} | {:>6} | {:>8} | {:>14} | {:>14} | {:>10} | {:>10} | {:>10} | {:>8} |",
        "-".repeat(7),
        "-".repeat(6),
        "-".repeat(8),
        "-".repeat(14),
        "-".repeat(14),
        "-".repeat(10),
        "-".repeat(10),
        "-".repeat(10),
        "-".repeat(8),
    );

    for &threads in &args.threads {
        for &shards in &args.shards {
            for &mix in &args.mixes {
                let result = run_case(
                    threads,
                    shards,
                    mix,
                    args.warmup,
                    args.duration,
                    args.latency_sample_rate,
                );
                println!(
                    "| {:>7} | {:>6} | {:>8} | {:>14.0} | {:>14.0} | {:>10} | {:>10} | {:>10} | {:>8} |",
                    threads,
                    shards,
                    mix.label,
                    result.ops_per_sec(),
                    result.ops_per_thread_per_sec(),
                    format_ns(result.percentile_ns(500)),
                    format_ns(result.percentile_ns(990)),
                    format_ns(result.percentile_ns(999)),
                    result.samples.len(),
                );
            }
        }
    }
}

struct RunResult {
    ops: usize,
    elapsed: Duration,
    threads: usize,
    samples: Vec<u64>,
}

struct WorkerResult {
    ops: usize,
    samples: Vec<u64>,
}

impl RunResult {
    fn ops_per_sec(&self) -> f64 {
        self.ops as f64 / self.elapsed.as_secs_f64()
    }

    fn ops_per_thread_per_sec(&self) -> f64 {
        self.ops_per_sec() / self.threads as f64
    }

    fn percentile_ns(&self, per_mille: usize) -> Option<u64> {
        if self.samples.is_empty() {
            return None;
        }
        let index = (self.samples.len() - 1) * per_mille / 1000;
        Some(self.samples[index])
    }
}

fn run_case(
    threads: usize,
    shards: usize,
    mix: Mix,
    warmup: Duration,
    duration: Duration,
    latency_sample_rate: usize,
) -> RunResult {
    assert!(threads > 0);
    assert!(shards > 0);

    let locks = Arc::new(
        (0..shards)
            .map(|_| Slot {
                lock: RwLock::new(0),
            })
            .collect::<Vec<_>>(),
    );
    let phase = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(threads + 1));
    let mut handles = Vec::with_capacity(threads);

    for thread_idx in 0..threads {
        let locks = Arc::clone(&locks);
        let phase = Arc::clone(&phase);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            let mut rng = splitmix64(0x9e37_79b9_7f4a_7c15 ^ thread_idx as u64);
            let mut measured_ops = 0usize;
            let mut samples = Vec::new();

            barrier.wait();
            loop {
                let current_phase = phase.load(Ordering::Relaxed);
                if current_phase == 2 {
                    break;
                }

                for _ in 0..BATCH {
                    let measure = current_phase == 1
                        && latency_sample_rate != 0
                        && measured_ops.is_multiple_of(latency_sample_rate);
                    if measure {
                        let start = Instant::now();
                        run_op(&locks, shards, mix, &mut rng);
                        samples.push(elapsed_ns(start));
                    } else {
                        run_op(&locks, shards, mix, &mut rng);
                    }

                    if current_phase == 1 {
                        measured_ops += 1;
                    }
                }
            }

            WorkerResult {
                ops: measured_ops,
                samples,
            }
        }));
    }

    barrier.wait();
    thread::sleep(warmup);
    let start = Instant::now();
    phase.store(1, Ordering::Relaxed);
    thread::sleep(duration);
    phase.store(2, Ordering::Relaxed);

    let mut ops = 0usize;
    let mut samples = Vec::new();
    for handle in handles {
        let mut worker = handle.join().expect("bench worker panicked");
        ops += worker.ops;
        samples.append(&mut worker.samples);
    }
    samples.sort_unstable();

    RunResult {
        ops,
        elapsed: start.elapsed(),
        threads,
        samples,
    }
}

fn elapsed_ns(start: Instant) -> u64 {
    start.elapsed().as_nanos().min(u64::MAX as u128) as u64
}

#[inline(always)]
fn run_op(locks: &[Slot], shards: usize, mix: Mix, rng: &mut u64) {
    let shard = if shards == 1 {
        0
    } else {
        next_usize(rng) % shards
    };
    let is_read =
        mix.read_pct == 100 || (mix.read_pct != 0 && next_usize(rng) % 100 < mix.read_pct as usize);

    if is_read {
        let guard = locks[shard].lock.read();
        black_box(*guard);
    } else {
        let mut guard = locks[shard].lock.write();
        *guard = guard.wrapping_add(1);
        black_box(*guard);
    }
}

#[inline(always)]
fn next_usize(state: &mut u64) -> usize {
    let mut x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    x.wrapping_mul(0x2545_f491_4f6c_dd1d) as usize
}

fn splitmix64(mut state: u64) -> u64 {
    state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl Args {
    fn parse() -> Self {
        let mut args = Self {
            duration: Duration::from_millis(300),
            warmup: Duration::from_millis(100),
            latency_sample_rate: 1024,
            threads: default_threads(),
            shards: vec![1, 16],
            mixes: vec![
                Mix {
                    label: "100-0",
                    read_pct: 100,
                },
                Mix {
                    label: "95-5",
                    read_pct: 95,
                },
                Mix {
                    label: "80-20",
                    read_pct: 80,
                },
                Mix {
                    label: "50-50",
                    read_pct: 50,
                },
                Mix {
                    label: "20-80",
                    read_pct: 20,
                },
                Mix {
                    label: "0-100",
                    read_pct: 0,
                },
            ],
        };

        let mut raw = env::args().skip(1);
        while let Some(arg) = raw.next() {
            match arg.as_str() {
                "--duration-ms" => {
                    args.duration = Duration::from_millis(parse_next(&mut raw, "--duration-ms"));
                }
                "--warmup-ms" => {
                    args.warmup = Duration::from_millis(parse_next(&mut raw, "--warmup-ms"));
                }
                "--latency-sample-rate" => {
                    args.latency_sample_rate = parse_next_usize(&mut raw, "--latency-sample-rate");
                }
                "--threads" => {
                    args.threads = parse_list(&parse_next_string(&mut raw, "--threads"));
                }
                "--shards" => {
                    args.shards = parse_list(&parse_next_string(&mut raw, "--shards"));
                }
                "--mixes" => {
                    args.mixes = parse_mixes(&parse_next_string(&mut raw, "--mixes"));
                }
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                "--bench" => {}
                other => {
                    eprintln!("unknown argument: {other}");
                    print_help();
                    std::process::exit(2);
                }
            }
        }

        args.threads.sort_unstable();
        args.threads.dedup();
        args.shards.sort_unstable();
        args.shards.dedup();
        assert!(!args.threads.is_empty(), "--threads cannot be empty");
        assert!(!args.shards.is_empty(), "--shards cannot be empty");
        assert!(!args.mixes.is_empty(), "--mixes cannot be empty");
        args
    }
}

fn default_threads() -> Vec<usize> {
    let max = thread::available_parallelism()
        .map(|threads| threads.get())
        .unwrap_or(4)
        .min(16);
    let mut threads = [1, 2, 4, 8, 16]
        .into_iter()
        .filter(|&threads| threads <= max)
        .collect::<Vec<_>>();
    if !threads.contains(&max) {
        threads.push(max);
    }
    threads
}

fn parse_next(raw: &mut impl Iterator<Item = String>, name: &str) -> u64 {
    parse_next_string(raw, name)
        .parse()
        .unwrap_or_else(|error| panic!("{name} expects an integer: {error}"))
}

fn parse_next_usize(raw: &mut impl Iterator<Item = String>, name: &str) -> usize {
    parse_next_string(raw, name)
        .parse()
        .unwrap_or_else(|error| panic!("{name} expects an integer: {error}"))
}

fn parse_next_string(raw: &mut impl Iterator<Item = String>, name: &str) -> String {
    raw.next()
        .unwrap_or_else(|| panic!("{name} expects a value"))
}

fn parse_list(input: &str) -> Vec<usize> {
    input
        .split(',')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let value = part
                .parse()
                .unwrap_or_else(|error| panic!("invalid integer in list `{input}`: {error}"));
            assert!(value > 0, "list values must be greater than zero");
            value
        })
        .collect()
}

fn parse_mixes(input: &str) -> Vec<Mix> {
    input
        .split(',')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (read, write) = part
                .split_once('-')
                .unwrap_or_else(|| panic!("mix `{part}` must look like READ-WRITE"));
            let read_pct = read
                .parse::<u8>()
                .unwrap_or_else(|error| panic!("invalid read percentage in `{part}`: {error}"));
            let write_pct = write
                .parse::<u8>()
                .unwrap_or_else(|error| panic!("invalid write percentage in `{part}`: {error}"));
            assert_eq!(
                read_pct as u16 + write_pct as u16,
                100,
                "mix `{part}` must add up to 100"
            );
            Mix {
                label: Box::leak(part.to_owned().into_boxed_str()),
                read_pct,
            }
        })
        .collect()
}

fn format_ns(ns: Option<u64>) -> String {
    let Some(ns) = ns else {
        return "-".to_owned();
    };

    if ns < 1_000 {
        format!("{ns}ns")
    } else if ns < 1_000_000 {
        format!("{:.1}us", ns as f64 / 1_000.0)
    } else if ns < 1_000_000_000 {
        format!("{:.1}ms", ns as f64 / 1_000_000.0)
    } else {
        format!("{:.1}s", ns as f64 / 1_000_000_000.0)
    }
}

fn print_help() {
    eprintln!(
        "usage: cargo bench --bench rwlock_load -- \\
         [--duration-ms N] [--warmup-ms N] [--threads 1,2,4] \\
         [--shards 1,16] [--mixes 100-0,95-5,80-20,50-50,20-80,0-100] \\
         [--latency-sample-rate N]"
    );
}
