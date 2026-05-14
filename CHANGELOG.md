# Changelog

All notable changes to this crate will be documented in this file.

This project follows semantic versioning.

## [0.1.0] - 2026-05-14

### Added

- Initial `rblock` crate.
- `RwLock<T>` type alias built on `lock_api`.
- `RwLockReadGuard` and `RwLockWriteGuard` guard aliases.
- `RawReadBiasedRwLock`, a read-biased raw reader-writer lock backed by
  `parking_lot_core`.
- Lock-only throughput and latency benchmark: `rwlock_load`.
- Writer-progress benchmark comparing this lock against `parking_lot::RwLock`.
- Linux benchmark sync harness for isolated crate benchmarking.

### Notes

- This lock intentionally favors readers. Waiting writers do not block new
  readers, so it is not a general-purpose fair `RwLock` replacement.
- Use it for measured, read-heavy, sharded cache workloads with short critical
  sections. Prefer a fair lock when writer progress or write-tail latency is
  the primary SLO.
