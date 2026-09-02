//! Serve-local ownership for subprocess trees.
//!
//! Git helpers and PTY shells may leave descendants behind after their direct
//! child exits. Git commands run in a fresh process group. PTY shells already
//! start a fresh session; for those, this guard also snapshots descendant
//! process groups so terminal job-control groups are terminated together.

#![forbid(unsafe_code)]

use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

#[cfg(unix)]
use std::collections::BTreeSet;
#[cfg(unix)]
use std::process::Stdio;
#[cfg(unix)]
use std::sync::Mutex;

/// Configures a command so its child becomes the leader of a new process group.
pub fn isolate_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }

    #[cfg(not(unix))]
    let _ = command;
}

/// Termination signal used for an owned process tree.
#[derive(Clone, Copy)]
pub enum TerminationSignal {
    /// Ask the process tree to exit gracefully.
    Graceful,
    /// Forcefully terminate the process tree.
    Force,
}

/// Drop guard for one child-owned process tree.
///
/// Numeric process identities are retained only through bounded cleanup.
/// Callers disarm the guard as soon as child and pipe settlement completes,
/// limiting the ordinary PID-reuse exposure of process-group APIs.
pub struct ProcessTree {
    group_id: AtomicI32,
    track_descendants: AtomicBool,
    track_session: AtomicBool,
    #[cfg(unix)]
    tracked: Mutex<TrackedProcesses>,
}

#[cfg(unix)]
#[derive(Default)]
struct TrackedProcesses {
    groups: BTreeSet<i32>,
}

impl ProcessTree {
    /// Owns the process group whose leader has `process_id`.
    pub fn from_process_id(process_id: Option<u32>) -> Self {
        let group_id = process_id
            .and_then(|id| i32::try_from(id).ok())
            .filter(|id| *id > 0)
            .unwrap_or(0);
        #[cfg(unix)]
        let tracked = {
            let mut tracked = TrackedProcesses::default();
            if group_id > 0 {
                tracked.groups.insert(group_id);
            }
            Mutex::new(tracked)
        };
        let owner = Self {
            group_id: AtomicI32::new(group_id),
            track_descendants: AtomicBool::new(false),
            track_session: AtomicBool::new(false),
            #[cfg(unix)]
            tracked,
        };
        #[cfg(unix)]
        {
            let owns_group = {
                #[cfg(target_os = "linux")]
                {
                    process_snapshot().is_some_and(|processes| {
                        processes
                            .iter()
                            .any(|process| process.pid == group_id && process.group == group_id)
                    })
                }
                #[cfg(not(target_os = "linux"))]
                {
                    rustix::process::Pid::from_raw(group_id).is_some_and(|process| {
                        rustix::process::getpgid(Some(process)).is_ok_and(|group| group == process)
                    })
                }
            };
            owner.track_descendants.store(owns_group, Ordering::Release);
        }
        owner
    }

    /// Owns a PTY session whose leader has `process_id`.
    ///
    /// `portable-pty` creates a fresh Unix session before executing the shell.
    /// That identity is verified before descendant tracking is enabled, so a
    /// backend regression cannot accidentally target the host's own session.
    pub(crate) fn from_session_id(process_id: Option<u32>) -> Self {
        let owner = Self::from_process_id(process_id);
        #[cfg(unix)]
        {
            let raw = owner.group_id.load(Ordering::Acquire);
            if raw > 0 {
                let owns_session = {
                    #[cfg(target_os = "linux")]
                    {
                        process_snapshot().is_some_and(|processes| {
                            processes
                                .iter()
                                .any(|process| process.pid == raw && process.session == raw)
                        })
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        rustix::process::Pid::from_raw(raw).is_some_and(|process| {
                            rustix::process::getsid(Some(process))
                                .is_ok_and(|session| session == process)
                        })
                    }
                };
                owner
                    .track_descendants
                    .store(owns_session, Ordering::Release);
                owner.track_session.store(owns_session, Ordering::Release);
                if owns_session {
                    owner.refresh_descendants();
                }
            }
        }
        owner
    }

    /// Sends a signal to every currently known group in the owned tree.
    pub fn signal(&self, signal: TerminationSignal) {
        #[cfg(unix)]
        {
            if self.track_descendants.load(Ordering::Acquire) {
                self.refresh_descendants();
            }
            let signal = match signal {
                TerminationSignal::Graceful => rustix::process::Signal::TERM,
                TerminationSignal::Force => rustix::process::Signal::KILL,
            };
            let groups = self
                .tracked
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .groups
                .clone();
            for raw in groups {
                if let Some(group) = rustix::process::Pid::from_raw(raw) {
                    let _ = rustix::process::kill_process_group(group, signal);
                }
            }
        }

        #[cfg(not(unix))]
        let _ = signal;
    }

    /// Returns whether any tracked process group is still observable.
    pub fn is_alive(&self) -> bool {
        #[cfg(unix)]
        {
            self.tracked
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .groups
                .iter()
                .copied()
                .filter_map(rustix::process::Pid::from_raw)
                .any(|group| rustix::process::test_kill_process_group(group).is_ok())
        }

        #[cfg(not(unix))]
        {
            false
        }
    }

    /// Relinquishes numeric process identities after cleanup settles.
    pub fn disarm(&self) {
        self.track_descendants.store(false, Ordering::Release);
        self.track_session.store(false, Ordering::Release);
        self.group_id.store(0, Ordering::Release);
        #[cfg(unix)]
        {
            let mut tracked = self
                .tracked
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            tracked.groups.clear();
        }
    }

    #[cfg(unix)]
    fn refresh_descendants(&self) {
        let root = self.group_id.load(Ordering::Acquire);
        if root <= 0 {
            return;
        }
        let track_session = self.track_session.load(Ordering::Acquire);
        let Some(processes) = process_snapshot() else {
            return;
        };
        let mut tracked = self
            .tracked
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut pids = BTreeSet::new();
        for process in &processes {
            if process.pid == root
                || (track_session && process.session == root)
                || (!track_session && process.group == root)
            {
                pids.insert(process.pid);
            }
        }

        // Rebuild from the current snapshot. Keeping a historical PID/group
        // set lets a later PID reuse turn cleanup into a signal for somebody
        // else's process. Session membership still finds descendants after the
        // session leader exits; deliberately daemonized work is outside this
        // ownership boundary.
        loop {
            let mut changed = false;
            for process in &processes {
                if pids.contains(&process.parent) && pids.insert(process.pid) {
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let groups = processes
            .iter()
            .filter(|process| pids.contains(&process.pid) && process.group > 0)
            .map(|process| process.group)
            .collect();
        tracked.groups = groups;
    }
}

#[cfg(unix)]
#[derive(Clone, Copy)]
struct ProcessRecord {
    pid: i32,
    parent: i32,
    group: i32,
    session: i32,
}

#[cfg(unix)]
fn process_snapshot() -> Option<Vec<ProcessRecord>> {
    const MAX_PROCESS_LIST_BYTES: usize = 4 * 1024 * 1024;

    let executable = ["/bin/ps", "/usr/bin/ps"]
        .into_iter()
        .find(|path| std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file()))?;
    let output = Command::new(executable)
        .args({
            #[cfg(target_os = "linux")]
            {
                ["-axo", "pid=,ppid=,pgid=,sess="]
            }
            #[cfg(not(target_os = "linux"))]
            {
                ["-axo", "pid=,ppid=,pgid="]
            }
        })
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .env_clear()
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.len() > MAX_PROCESS_LIST_BYTES {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let pid = fields.next()?.parse().ok()?;
                let parent = fields.next()?.parse().ok()?;
                let group = fields.next()?.parse().ok()?;
                #[cfg(target_os = "linux")]
                let session = fields.next()?.parse().ok()?;
                #[cfg(not(target_os = "linux"))]
                let session = {
                    let process = rustix::process::Pid::from_raw(pid)?;
                    rustix::process::getsid(Some(process)).ok()?.as_raw_pid()
                };
                Some(ProcessRecord {
                    pid,
                    parent,
                    group,
                    session,
                })
            })
            .collect(),
    )
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            self.group_id.store(0, Ordering::Release);
            self.track_descendants.store(false, Ordering::Release);
            self.track_session.store(false, Ordering::Release);
            let groups = self
                .tracked
                .get_mut()
                .unwrap_or_else(|error| error.into_inner())
                .groups
                .clone();
            for raw in groups {
                if let Some(group) = rustix::process::Pid::from_raw(raw) {
                    let _ =
                        rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
                }
            }
        }
    }
}
