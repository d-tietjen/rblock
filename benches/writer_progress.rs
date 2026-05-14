use std::env;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use rblock::RwLock as RblockRwLock;

#[derive(Debug, Clone)]
struct Args {
    duration: Duration,
    warmup: Duration,
    readers: Vec<usize>,
    reader_work: usize,
    writer_work: usize,
}

trait BenchLock: Send + Sync + 'static {
    const NAME: &'static str;

    fn new() -> Self;
    fn read(&self, work: usize);
    fn write(&self, work: usize);
}

struct RblockLock(RblockRwLock<usize>);
struct ParkingLotLock(parking_lot::RwLock<usize>);

impl BenchLock for RblockLock {
    const NAME: &'static str = "rblock";

    fn new() -> Self {
        Self(RblockRwLock::new(0))
    }

    fn read(&self, work: usize) {
        let guard = self.0.read();
        burn(work, *guard);
    }

    fn write(&self, work: usize) {
        let mut guard = self.0.write();
        *guard = guard.wrapping_add(1);
        burn(work, *guard);
    }
}

impl BenchLock for ParkingLotLock {
    const NAME: &'static str = "parking_lot";

    fn new() -> Self {
        Self(parking_lot::RwLock::new(0))
    }

    fn read(&self, work: usize) {
        let guard = self.0.read();
        burn(work, *guard);
    }

    fn write(&self, work: usize) {
        let mut guard = self.0.write();
        *guard = guard.wrapping_add(1);
        burn(work, *guard);
    }
}

fn main() {
    let args = Args::parse();
    println!(
        "writer_progress: duration={}ms warmup={}ms reader_work={} writer_work={}",
        args.duration.as_millis(),
        args.warmup.as_millis(),
        args.reader_work,
        args.writer_work,
    );
    println!();
    println!(
        "| {:>11} | {:>7} | {:>12} | {:>10} | {:>10} | {:>10} | {:>10} |",
        "lock", "readers", "writer acq/s", "p50", "p99", "p99.9", "max"
    );
    println!(
        "| {:>11} | {:>7} | {:>12} | {:>10} | {:>10} | {:>10} | {:>10} |",
        "-".repeat(11),
        "-".repeat(7),
        "-".repeat(12),
        "-".repeat(10),
        "-".repeat(10),
        "-".repeat(10),
        "-".repeat(10),
    );

    for &readers in &args.readers {
        print_result(run_case::<RblockLock>(readers, &args));
        print_result(run_case::<ParkingLotLock>(readers, &args));
    }
}

struct RunResult {
    lock: &'static str,
    readers: usize,
    writes: usize,
    elapsed: Duration,
    samples: Vec<u64>,
}

fn run_case<L: BenchLock>(readers: usize, args: &Args) -> RunResult {
    let lock = Arc::new(L::new());
    let phase = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(readers + 2));
    let mut handles = Vec::with_capacity(readers);

    for _ in 0..readers {
        let lock = Arc::clone(&lock);
        let phase = Arc::clone(&phase);
        let barrier = Arc::clone(&barrier);
        let reader_work = args.reader_work;
        handles.push(thread::spawn(move || {
            barrier.wait();
            while phase.load(Ordering::Relaxed) != 2 {
                lock.read(reader_work);
            }
        }));
    }

    let writer_lock = Arc::clone(&lock);
    let writer_phase = Arc::clone(&phase);
    let writer_barrier = Arc::clone(&barrier);
    let writer_work = args.writer_work;
    let writer = thread::spawn(move || {
        let mut samples = Vec::new();
        let mut writes = 0usize;

        writer_barrier.wait();
        while writer_phase.load(Ordering::Relaxed) != 2 {
            let measured = writer_phase.load(Ordering::Relaxed) == 1;
            let start = measured.then(Instant::now);
            writer_lock.write(writer_work);
            if let Some(start) = start {
                samples.push(elapsed_ns(start));
                writes += 1;
            }
        }

        (writes, samples)
    });

    barrier.wait();
    thread::sleep(args.warmup);
    let start = Instant::now();
    phase.store(1, Ordering::Relaxed);
    thread::sleep(args.duration);
    phase.store(2, Ordering::Relaxed);

    for handle in handles {
        handle.join().expect("reader panicked");
    }
    let (writes, mut samples) = writer.join().expect("writer panicked");
    samples.sort_unstable();

    RunResult {
        lock: L::NAME,
        readers,
        writes,
        elapsed: start.elapsed(),
        samples,
    }
}

fn print_result(result: RunResult) {
    println!(
        "| {:>11} | {:>7} | {:>12.0} | {:>10} | {:>10} | {:>10} | {:>10} |",
        result.lock,
        result.readers,
        result.writes as f64 / result.elapsed.as_secs_f64(),
        format_ns(percentile(&result.samples, 500)),
        format_ns(percentile(&result.samples, 990)),
        format_ns(percentile(&result.samples, 999)),
        format_ns(result.samples.last().copied()),
    );
}

#[inline(always)]
fn burn(work: usize, seed: usize) {
    let mut value = seed;
    for _ in 0..work {
        value = value.wrapping_mul(1664525).wrapping_add(1013904223);
    }
    black_box(value);
}

fn elapsed_ns(start: Instant) -> u64 {
    start.elapsed().as_nanos().min(u64::MAX as u128) as u64
}

fn percentile(samples: &[u64], per_mille: usize) -> Option<u64> {
    if samples.is_empty() {
        return None;
    }
    let index = (samples.len() - 1) * per_mille / 1000;
    Some(samples[index])
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

impl Args {
    fn parse() -> Self {
        let mut args = Self {
            duration: Duration::from_millis(500),
            warmup: Duration::from_millis(150),
            readers: default_readers(),
            reader_work: 0,
            writer_work: 0,
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
                "--readers" => {
                    args.readers = parse_list(&parse_next_string(&mut raw, "--readers"));
                }
                "--reader-work" => {
                    args.reader_work = parse_next_usize(&mut raw, "--reader-work");
                }
                "--writer-work" => {
                    args.writer_work = parse_next_usize(&mut raw, "--writer-work");
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

        args.readers.sort_unstable();
        args.readers.dedup();
        assert!(!args.readers.is_empty(), "--readers cannot be empty");
        args
    }
}

fn default_readers() -> Vec<usize> {
    let max = thread::available_parallelism()
        .map(|threads| threads.get())
        .unwrap_or(4)
        .min(16);
    let mut readers = [1, 2, 4, 8, 16]
        .into_iter()
        .filter(|&readers| readers <= max)
        .collect::<Vec<_>>();
    if !readers.contains(&max) {
        readers.push(max);
    }
    readers
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

fn print_help() {
    eprintln!(
        "usage: cargo bench --bench writer_progress -- \\
         [--duration-ms N] [--warmup-ms N] [--readers 1,2,4] \\
         [--reader-work N] [--writer-work N]"
    );
}
