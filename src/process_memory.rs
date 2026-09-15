// SPDX-License-Identifier: MIT
//! OS accounting for one process, independent of provider/allocator telemetry.
use super::*;

#[derive(Clone)]
pub(super) struct Reading {
    pub pid: u32,
    pub started: u64,
    pub resident: u64,
    pub footprint: u64,
    pub peak: u64,
    pub at: Instant,
}

#[cfg(target_os = "macos")]
pub(super) fn read(pid: u32) -> Option<Reading> {
    use libproc::pid_rusage::{pidrusage, RUsageInfoV4};
    let pid_i32 = i32::try_from(pid).ok().filter(|pid| *pid > 0)?;
    let usage = pidrusage::<RUsageInfoV4>(pid_i32).ok()?;
    Some(Reading {
        pid,
        started: usage.ri_proc_start_abstime,
        resident: usage.ri_resident_size,
        footprint: usage.ri_phys_footprint,
        peak: usage.ri_lifetime_max_phys_footprint,
        at: Instant::now(),
    })
}

#[cfg(not(target_os = "macos"))]
pub(super) fn read(_pid: u32) -> Option<Reading> {
    None
}

pub(super) fn growth(current: &Reading, previous: Option<&Reading>) -> Option<i64> {
    let previous = previous?;
    if current.pid != previous.pid || current.started != previous.started {
        return None;
    }
    let elapsed = current.at.checked_duration_since(previous.at)?;
    if elapsed.is_zero() {
        return None;
    }
    Some(signed_rate_bytes(
        current.footprint,
        previous.footprint,
        elapsed,
    ))
}

pub(super) fn summary(sample: &Sample) -> String {
    sample
        .process_memory
        .as_ref()
        .map(|reading| {
            format!(
                "PROCESS {} · footprint {}",
                reading.pid,
                bytes(reading.footprint)
            )
        })
        .unwrap_or_else(|| mlx_runtime_summary(&sample.mlx))
}

pub(super) fn detail(sample: &Sample) -> String {
    sample
        .process_memory
        .as_ref()
        .map(|reading| {
            format!(
                "peak {} · growth {} · OS",
                bytes(reading.peak),
                sample
                    .process_memory_growth
                    .map(signed_rate)
                    .unwrap_or_else(|| "—".into())
            )
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn growth_tracks_same_process_and_preserves_decreases() {
        let first = Reading {
            pid: 42,
            started: 1,
            resident: 100,
            footprint: 100,
            peak: 200,
            at: Instant::now(),
        };
        let mut next = first.clone();
        next.at += Duration::from_secs(2);
        next.footprint = 140;
        assert_eq!(growth(&next, Some(&first)), Some(20));
        next.footprint = 60;
        assert_eq!(growth(&next, Some(&first)), Some(-20));
        assert_eq!(growth(&next, None), None);
        assert_eq!(growth(&first, Some(&first)), None);
        next.pid = 43;
        assert_eq!(growth(&next, Some(&first)), None);
        next.pid = 42;
        next.started = 2;
        assert_eq!(growth(&next, Some(&first)), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn os_read_works_for_this_process_and_rejects_invalid_pids() {
        let reading = read(std::process::id()).expect("read own OS memory counters");
        assert!(reading.footprint > 0);
        assert!(reading.resident > 0);
        assert!(reading.peak >= reading.footprint);
        assert!(read(0).is_none());
        assert!(read(u32::MAX).is_none());
    }
}
