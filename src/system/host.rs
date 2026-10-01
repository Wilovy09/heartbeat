//! Heartbeat's own host, read with `sysinfo`: what `top` shows (CPU, memory, swap, load,
//! the busiest processes) plus the disks behind `/` and the database.

use std::path::{Path, PathBuf};

use sysinfo::{Disks, ProcessRefreshKind, ProcessesToUpdate, System};

use super::{Cpu, Disk, MAX_PROCESSES, Memory, Process, Snapshot, Swap};

/// Kept between samples: CPU usage (the system's and each process's) is measured as the
/// change since the previous refresh, so the first sample after startup reads ~0.
pub struct HostSampler {
    sys: System,
    disks: Disks,
    /// The database file, to report the disk it lives on.
    database: PathBuf,
}

impl HostSampler {
    #[must_use]
    pub fn new(database: &Path) -> Self {
        let mut sys = System::new();
        sys.refresh_cpu_usage();
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, process_kind());
        Self {
            sys,
            disks: Disks::new_with_refreshed_list(),
            database: database
                .canonicalize()
                .unwrap_or_else(|_| database.to_path_buf()),
        }
    }

    /// Blocking (it reads `/proc`, or asks the kernel, for every process): run it off the
    /// async runtime.
    pub fn sample(&mut self) -> Snapshot {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        self.sys
            .refresh_processes_specifics(ProcessesToUpdate::All, true, process_kind());
        self.disks.refresh(true);

        let load = System::load_average();
        let mut processes: Vec<Process> = self
            .sys
            .processes()
            .values()
            .map(|p| Process {
                pid: p.pid().as_u32(),
                name: p.name().to_string_lossy().into_owned(),
                cpu_pct: f64::from(p.cpu_usage()),
                memory_bytes: p.memory(),
            })
            .collect();
        Snapshot {
            cpu: Cpu {
                usage_pct: f64::from(self.sys.global_cpu_usage()),
                cores: u32::try_from(self.sys.cpus().len()).ok(),
                load: Some([load.one, load.five, load.fifteen]),
            },
            memory: Memory {
                total_bytes: self.sys.total_memory(),
                used_bytes: self.sys.used_memory(),
                available_bytes: Some(self.sys.available_memory()),
            },
            swap: (self.sys.total_swap() > 0).then(|| Swap {
                total_bytes: self.sys.total_swap(),
                used_bytes: self.sys.used_swap(),
            }),
            disks: self.disks(),
            processes: busiest(&mut processes),
            uptime_secs: Some(System::uptime()),
        }
    }

    /// The disks behind `/` and behind the database (one entry if they're the same):
    /// listing every mount would bring in pseudo and read-only system volumes, which are
    /// often "full" by design and would trip the disk alert.
    fn disks(&self) -> Vec<Disk> {
        let holding = |path: &Path| {
            self.disks
                .list()
                .iter()
                .filter(|d| path.starts_with(d.mount_point()) && d.total_space() > 0)
                .max_by_key(|d| d.mount_point().as_os_str().len())
        };
        let mut out: Vec<Disk> = Vec::new();
        for disk in [holding(Path::new("/")), holding(&self.database)]
            .into_iter()
            .flatten()
        {
            let mount = disk.mount_point().to_string_lossy().into_owned();
            if out.iter().all(|d| d.mount != mount) {
                out.push(Disk {
                    mount,
                    total_bytes: disk.total_space(),
                    used_bytes: disk.total_space().saturating_sub(disk.available_space()),
                });
            }
        }
        out
    }
}

fn process_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing().with_cpu().with_memory()
}

/// The `MAX_PROCESSES` busiest processes: the top half by CPU and the top half by memory,
/// so either column can be sorted on. Each `cpu_pct` is of one core, like `top`'s.
fn busiest(all: &mut [Process]) -> Vec<Process> {
    let half = MAX_PROCESSES / 2;
    all.sort_by(|a, b| b.cpu_pct.total_cmp(&a.cpu_pct));
    let mut out: Vec<Process> = all.iter().take(half).cloned().collect();
    all.sort_by_key(|p| std::cmp::Reverse(p.memory_bytes));
    for p in all.iter() {
        if out.len() >= MAX_PROCESSES {
            break;
        }
        if out.iter().all(|o| o.pid != p.pid) {
            out.push(p.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sample_reads_this_machine() {
        let mut sampler = HostSampler::new(Path::new("."));
        let snapshot = sampler.sample();
        assert!(snapshot.memory.total_bytes > 0);
        assert!(snapshot.memory.used_bytes <= snapshot.memory.total_bytes);
        assert!(snapshot.cpu.cores.unwrap_or(0) > 0);
        assert!(!snapshot.disks.is_empty(), "at least the disk behind /");
        assert!(!snapshot.processes.is_empty() && snapshot.processes.len() <= MAX_PROCESSES);
    }

    #[test]
    fn the_busiest_mix_cpu_and_memory_without_repeats() {
        let mut all: Vec<Process> = (0..50_u32)
            .map(|i| Process {
                pid: i,
                name: format!("p{i}"),
                cpu_pct: f64::from(i),
                memory_bytes: u64::from(100 - i),
            })
            .collect();
        let top = busiest(&mut all);
        assert_eq!(top.len(), MAX_PROCESSES);
        let pids: Vec<u32> = top.iter().map(|p| p.pid).collect();
        assert!(pids.contains(&49) && pids.contains(&0), "{pids:?}");
        let mut unique = pids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), pids.len());
    }
}
