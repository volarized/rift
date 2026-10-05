//! The process readings: the current process's memory, CPU time, open files, and disk
//! bytes, read through `sysinfo` each time the meter's reader collects.
//!
//! Each reading is an OpenTelemetry observable instrument whose callback the SDK runs on
//! its reader's task: no task of Rift's ticks, and a process that installed no meter
//! registers nothing and reads nothing. A reading the platform does not report stays absent
//! and never becomes zero. Totals stay cumulative; a collector derives rates from them.

use std::sync::{Arc, Mutex, PoisonError};

mod tokio_runtime;

pub(crate) use self::tokio_runtime::observe_runtime;
use crate::metrics::with_meter;

/// `process.memory.usage`: the resident set, in bytes.
const PROCESS_MEMORY_USAGE: &str = "process.memory.usage";
/// `process.memory.virtual`: the virtual memory size, in bytes.
const PROCESS_MEMORY_VIRTUAL: &str = "process.memory.virtual";
/// `process.cpu.time`: the CPU time the process consumed, in seconds. `sysinfo` reports one
/// total, so the reading carries no `cpu.mode`.
const PROCESS_CPU_TIME: &str = "process.cpu.time";
/// Open file descriptors: `process.unix.file_descriptor.count`.
#[cfg(not(windows))]
const PROCESS_OPEN_FILES: (&str, &str) =
    ("process.unix.file_descriptor.count", "{file_descriptor}");
/// Open handles of every kind: `process.windows.handle.count`.
#[cfg(windows)]
const PROCESS_OPEN_FILES: (&str, &str) = ("process.windows.handle.count", "{handle}");
/// `process.disk.io`: bytes read and written, by `disk.io.direction`.
const PROCESS_DISK_IO: &str = "process.disk.io";

/// One read of the process, as the platform reported it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProcessReading {
    /// Resident set size in bytes.
    pub(crate) resident_bytes: Option<u64>,
    /// Virtual memory size in bytes.
    pub(crate) virtual_bytes: Option<u64>,
    /// CPU time consumed since the process started, in milliseconds.
    pub(crate) cpu_time_ms: Option<u64>,
    /// Open file descriptors, or open handles on Windows.
    pub(crate) open_files: Option<u64>,
    /// Bytes read since the process started.
    pub(crate) read_bytes: Option<u64>,
    /// Bytes written since the process started.
    pub(crate) written_bytes: Option<u64>,
}

/// One read of the process the readings watch.
pub(crate) trait ProcessReader: Send + 'static {
    /// Refreshes and reads the process. The call blocks on the OS.
    fn read(&mut self) -> ProcessReading;
}

/// Reads the current process through `sysinfo`.
pub(crate) struct SystemProcessReader {
    system: sysinfo::System,
    pid: Option<sysinfo::Pid>,
}

impl SystemProcessReader {
    /// A reader of the current process. A platform that names no current process reads
    /// nothing.
    pub(crate) fn current() -> Self {
        Self {
            system: sysinfo::System::new(),
            pid: sysinfo::get_current_pid().ok(),
        }
    }
}

impl ProcessReader for SystemProcessReader {
    fn read(&mut self) -> ProcessReading {
        let Some(pid) = self.pid else {
            return ProcessReading::default();
        };
        let refresh = sysinfo::ProcessRefreshKind::nothing()
            .with_memory()
            .with_cpu()
            .with_disk_usage()
            .without_tasks();
        self.system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[pid]),
            false,
            refresh,
        );
        let Some(process) = self.system.process(pid) else {
            return ProcessReading::default();
        };
        let disk = process.disk_usage();
        ProcessReading {
            resident_bytes: Some(process.memory()),
            virtual_bytes: Some(process.virtual_memory()),
            cpu_time_ms: Some(process.accumulated_cpu_time()),
            open_files: process
                .open_files()
                .and_then(|count| u64::try_from(count).ok()),
            read_bytes: Some(disk.total_read_bytes),
            written_bytes: Some(disk.total_written_bytes),
        }
    }
}

/// `value` as an up-down counter observes it; a value past `i64::MAX` observes that.
fn signed(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Registers the process readings `reader` reads with the installed meter; answers whether
/// a meter was installed.
///
/// Each callback runs one `reader.read()` on the reader's task: on an Apple M3 a median of
/// 23 us, 34 us at p99, and 173 us for the first. A collection runs the five callbacks in
/// series, so the readings cost five reads per collection. The mutex around `reader` is
/// taken by these callbacks alone, which the SDK already runs one at a time.
pub(crate) fn observe_process(reader: impl ProcessReader) -> bool {
    let reader = Arc::new(Mutex::new(reader));
    let read = move || reader.lock().unwrap_or_else(PoisonError::into_inner).read();
    with_meter(|meter| {
        let reading = read.clone();
        let _usage = meter
            .i64_observable_up_down_counter(PROCESS_MEMORY_USAGE)
            .with_unit("By")
            .with_callback(move |instrument| {
                if let Some(bytes) = reading().resident_bytes {
                    instrument.observe(signed(bytes), &[]);
                }
            })
            .build();
        let reading = read.clone();
        let _virtual = meter
            .i64_observable_up_down_counter(PROCESS_MEMORY_VIRTUAL)
            .with_unit("By")
            .with_callback(move |instrument| {
                if let Some(bytes) = reading().virtual_bytes {
                    instrument.observe(signed(bytes), &[]);
                }
            })
            .build();
        let reading = read.clone();
        let _cpu = meter
            .f64_observable_counter(PROCESS_CPU_TIME)
            .with_unit("s")
            .with_callback(move |instrument| {
                if let Some(millis) = reading().cpu_time_ms {
                    instrument.observe(std::time::Duration::from_millis(millis).as_secs_f64(), &[]);
                }
            })
            .build();
        let reading = read.clone();
        let (open_files, unit) = PROCESS_OPEN_FILES;
        let _files = meter
            .i64_observable_up_down_counter(open_files)
            .with_unit(unit)
            .with_callback(move |instrument| {
                if let Some(count) = reading().open_files {
                    instrument.observe(signed(count), &[]);
                }
            })
            .build();
        let reading = read;
        let _disk = meter
            .u64_observable_counter(PROCESS_DISK_IO)
            .with_unit("By")
            .with_callback(move |instrument| {
                let now = reading();
                for (direction, bytes) in [("read", now.read_bytes), ("write", now.written_bytes)] {
                    if let Some(bytes) = bytes {
                        instrument.observe(
                            bytes,
                            &[opentelemetry::KeyValue::new("disk.io.direction", direction)],
                        );
                    }
                }
            })
            .build();
    })
}

#[cfg(test)]
mod tests;
