//! Linux owned-child syscall checkpoint; no production test switch or load race.
use super::Child;
use std::fs;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

pub fn trace(command: &mut Command) {
    // SAFETY: only the async-signal-safe ptrace syscall runs after fork. The
    // parent owns this exact child; exec stops it before application execution.
    unsafe {
        command.pre_exec(|| {
            if libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn stopped(child: &mut Child, until: Instant) {
    loop {
        // Child::try_wait cannot be used while tracing: waitpid can return a
        // ptrace stop even without WUNTRACED, which std treats as terminal.
        // Peek only terminal events; let Child reap those, never the stops.
        // SAFETY: zeroed output and exact child PID; WNOWAIT preserves custody.
        let mut terminal: libc::siginfo_t = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe {
                libc::waitid(
                    libc::P_PID,
                    child.0.id(),
                    &mut terminal,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            },
            0
        );
        if matches!(
            terminal.si_code,
            libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED
        ) {
            panic!("child exited before checkpoint: {:?}", child.0.wait());
        }
        // Observe only stops. Terminal reaping remains exclusively in Child.
        // SAFETY: initialized siginfo and exact unreaped owned child PID.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                child.0.id(),
                &mut info,
                libc::WSTOPPED | libc::WNOHANG,
            )
        };
        assert_eq!(rc, 0, "waitid: {}", std::io::Error::last_os_error());
        // SAFETY: waitid initialized the siginfo union with a child event.
        if unsafe { info.si_pid() } != 0 {
            return;
        }
        assert!(Instant::now() < until, "syscall checkpoint timed out");
        std::thread::yield_now();
    }
}

pub fn partial_checkpoint(
    child: &mut Child,
    root: &Path,
    stderr: &mut String,
    launched: Instant,
) -> Instant {
    // Observe diagnostics in memory only; never persist child output.
    let fd = child.0.stderr.as_ref().expect("stderr pipe").as_raw_fd();
    // SAFETY: fcntl operates only on our live owned read descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    let until = launched + Duration::from_secs(3);
    let mut deadline_upper = None;
    let mut deadline_lower = None;
    loop {
        stopped(child, until);
        // The last getcwd is env::current_dir, before canonicalize and the
        // actual search deadline is armed. StageTrace's own deadline is earlier
        // and must not be confused with it. Use ordering, not its remaining_ms.
        // SAFETY: register output belongs to this stopped Linux x86_64 child.
        let mut registers: libc::user_regs_struct = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::ptrace(libc::PTRACE_GETREGS, child.0.id(), 0, &mut registers) },
            0
        );
        if deadline_upper.is_none() && registers.orig_rax == libc::SYS_getcwd as u64 {
            deadline_lower = Some(Instant::now() + Duration::from_secs(1));
        }
        if deadline_upper.is_none() {
            match child
                .0
                .stderr
                .as_mut()
                .expect("stderr")
                .read_to_string(stderr)
            {
                Ok(_) => (),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => (),
                Err(_) => panic!("trace pipe read failed"),
            }
            assert!(stderr.len() < 4096, "bounded trace diagnostics");
            if stderr.lines().any(|line| {
                line.contains("stage=initial_search status=start") && line.contains("count=0")
            }) {
                deadline_upper = Some(Instant::now() + Duration::from_secs(1));
            }
        }
        let sources = fs::read_dir(format!("/proc/{}/fd", child.0.id()))
            .expect("owned descriptors")
            .flatten()
            .filter_map(|entry| fs::read_link(entry.path()).ok())
            .filter(|path| {
                path.parent() == Some(root) && path.extension().is_some_and(|s| s == "rs")
            })
            .collect::<std::collections::HashSet<_>>();
        // Search retains the first source FD before opening the second source.
        // Stopping at every syscall makes this checkpoint lossless, even for two tiny files.
        if sources.len() == 2 {
            let upper = deadline_upper.expect("initial search trace preceded source reads");
            let lower = deadline_lower.expect("getcwd before deadline initialization");
            let reserve_floor = (lower.saturating_duration_since(Instant::now()) / 10)
                .min(Duration::from_millis(100));
            assert!(
                upper.saturating_duration_since(lower) < reserve_floor / 2,
                "deadline envelope too wide; no timing evidence"
            );
            // Past every possible scan deadline, before every possible global
            // deadline. Refuse ambiguous scheduling instead of tuning sleeps.
            let resume = upper - reserve_floor / 2;
            assert!(Instant::now() < resume, "checkpoint missed scan deadline");
            return resume;
        }
        // SAFETY: waitid reported a ptrace stop for this still-owned child.
        assert_eq!(
            unsafe { libc::ptrace(libc::PTRACE_SYSCALL, child.0.id(), 0, 0) },
            0
        );
    }
}

pub fn resume(child: &mut Child, at: Instant) {
    std::thread::sleep(at.saturating_duration_since(Instant::now()));
    // Detach resumes the stopped child. It has never been terminally reaped.
    // SAFETY: exact owned child remains at the checkpoint stop.
    assert_eq!(
        unsafe { libc::ptrace(libc::PTRACE_DETACH, child.0.id(), 0, 0) },
        0
    );
}
