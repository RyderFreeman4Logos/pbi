//! One-shot JS/TS grammar capability. No source paths, environment, model, or
//! tool execution crosses this boundary. Linux x86_64 only; no fallback parser.
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

#[cfg(test)]
std::thread_local! {
    pub(crate) static SPAWNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static LAST_CHILD: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

const WORKER_ARG: &str = "--pbi-private-js-spans-v1";
const MAX_SOURCE: usize = super::MAX_SOURCE_BYTES as usize;
const MAX_OUTPUT: usize = 8 + MAX_SOURCE * 4;
const LIMITS: [(libc::__rlimit_resource_t, libc::rlim_t); 4] = [
    (libc::RLIMIT_AS, 512 * 1024 * 1024),
    (libc::RLIMIT_STACK, 8 * 1024 * 1024),
    (libc::RLIMIT_CPU, 2),
    (libc::RLIMIT_CORE, 0),
];

/// Dispatch before CLI/config/auth initialization. The private worker consumes
/// stdin bytes and emits only a binary numeric-span response on inherited fd 3.
/// It cannot spawn another worker or open files/network sockets after dispatch.
pub fn dispatch() {
    let mut args = std::env::args_os().skip(1);
    if args.next().is_some_and(|arg| arg == WORKER_ARG) {
        if args.next().is_some() {
            std::process::exit(2);
        }
        worker_exit();
    }
}

pub(crate) fn project(source: &str, typescript: bool, deadline: Instant) -> Result<String, ()> {
    let output = exchange(source, u8::from(typescript), deadline).map_err(|_| ())?;
    if !output.status.success() {
        return Err(());
    }
    let spans = decode(source, &output.bytes)?;
    let mut code = source.as_bytes().to_vec();
    for (start, end) in spans {
        for byte in &mut code[start as usize..end as usize] {
            if !matches!(*byte, b'\r' | b'\n') {
                *byte = b' ';
            }
        }
    }
    String::from_utf8(code).map_err(|_| ())
}

struct Output {
    status: ExitStatus,
    bytes: Vec<u8>,
    #[cfg(test)]
    pid: u32,
}

// The kit's existing supervisor is private to its external-bwrap executor and
// accepts tool paths/workdirs. Reusing it would expand this capability. Keep
// this single owned child + nonblocking pipes; no writer/reader threads to leak.
struct OwnedWorker {
    child: Child,
    input: Option<ChildStdin>,
    output: File,
    reaped: bool,
}

impl OwnedWorker {
    fn spawn() -> io::Result<Self> {
        let mut fds = [-1; 2];
        // SAFETY: valid two-descriptor storage, checked before ownership transfer.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful pipe2 created these distinct owned descriptors.
        let output = unsafe { File::from_raw_fd(fds[0]) };
        let writer = unsafe { File::from_raw_fd(fds[1]) };
        nonblocking(output.as_raw_fd())?;
        // /proc/self/exe names the running image (including after unlink), not
        // PATH/current_exe's replaceable pathname. The child exec pins that image.
        let mut command = Command::new("/proc/self/exe");
        #[cfg(not(test))]
        command.arg(WORKER_ARG);
        #[cfg(test)]
        command.args([
            "--exact",
            "grammar_worker::tests::worker_entry",
            "--nocapture",
        ]);
        command
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let fd = writer.as_raw_fd();
        // SAFETY: pre_exec uses only async-signal-safe syscalls, no allocation,
        // logging or locks. fd is kept alive across spawn. fd 3 is protocol-only.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 || libc::dup2(fd, 3) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::fcntl(3, libc::F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                limits()
            });
        }
        let child = command.spawn()?;
        #[cfg(test)]
        {
            SPAWNS.with(|count| count.set(count.get() + 1));
            LAST_CHILD.with(|pid| pid.set(child.id()));
        }
        let mut owned = Self {
            child,
            input: None,
            output,
            reaped: false,
        };
        owned.input = owned.child.stdin.take();
        nonblocking(
            owned
                .input
                .as_ref()
                .ok_or_else(io::Error::last_os_error)?
                .as_raw_fd(),
        )?;
        Ok(owned)
    }
}

impl Drop for OwnedWorker {
    fn drop(&mut self) {
        self.input.take();
        if !self.reaped {
            // SAFETY: unreaped child still reserves its PID, setsid established
            // its private group at spawn. No descendants allowed by seccomp.
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        // Child is dead: drain its finite pipe without blocking or saving bytes.
        let mut discard = [0; 8192];
        while matches!(self.output.read(&mut discard), Ok(n) if n > 0) {}
    }
}

fn nonblocking(fd: i32) -> io::Result<()> {
    // SAFETY: fd is an owned live pipe, F_GETFL/F_SETFL take no pointers.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
enum WorkerFailure {
    InputLimit,
    OutputLimit,
    Deadline,
    Io,
}

fn exchange(source: &str, mode: u8, deadline: Instant) -> Result<Output, WorkerFailure> {
    if source.len() > MAX_SOURCE {
        return Err(WorkerFailure::InputLimit);
    }
    if Instant::now() >= deadline {
        return Err(WorkerFailure::Deadline);
    }
    let deadline = deadline.min(Instant::now() + Duration::from_secs(2));
    let mut worker = OwnedWorker::spawn().map_err(|_| WorkerFailure::Io)?;
    let mut sent = 0;
    let mut bytes = Vec::new();
    let mut eof = false;
    loop {
        if Instant::now() >= deadline {
            return Err(WorkerFailure::Deadline);
        }
        let mut progress = false;
        if let Some(input) = worker.input.as_mut() {
            let prefix = [mode];
            let chunk = if sent == 0 {
                &prefix[..]
            } else {
                &source.as_bytes()[sent - 1..(sent - 1 + 8192).min(source.len())]
            };
            match input.write(chunk) {
                Ok(n) => {
                    sent += n;
                    progress = n > 0;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => {
                    worker.input.take();
                }
            }
            if sent == source.len() + 1 {
                worker.input.take();
            }
        }
        let mut chunk = [0; 8192];
        match worker.output.read(&mut chunk) {
            Ok(0) => eof = true,
            Ok(n) => {
                if bytes.len() + n > MAX_OUTPUT {
                    return Err(WorkerFailure::OutputLimit);
                }
                bytes.extend_from_slice(&chunk[..n]);
                progress = true;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return Err(WorkerFailure::Io),
        }
        if eof {
            if let Some(status) = worker.child.try_wait().map_err(|_| WorkerFailure::Io)? {
                worker.reaped = true;
                return Ok(Output {
                    status,
                    bytes,
                    #[cfg(test)]
                    pid: worker.child.id(),
                });
            }
        }
        if !progress {
            // Nonblocking stdin keeps backpressure inside the same wall budget.
            std::thread::sleep(
                Duration::from_millis(1).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
}

fn decode(source: &str, bytes: &[u8]) -> Result<Vec<(u32, u32)>, ()> {
    if bytes.get(..4) != Some(b"JSP1") {
        return Err(());
    }
    let number = |at| -> Result<u32, ()> {
        Ok(u32::from_le_bytes(
            bytes
                .get(at..at + 4)
                .ok_or(())?
                .try_into()
                .map_err(|_| ())?,
        ))
    };
    let count = number(4)? as usize;
    if count > source.len() / 2 + 1 || bytes.len() != 8 + count * 8 {
        return Err(());
    }
    let mut spans = Vec::with_capacity(count);
    let mut previous = 0;
    for index in 0..count {
        let start = number(8 + index * 8)?;
        let end = number(12 + index * 8)?;
        if start < previous
            || start >= end
            || end as usize > source.len()
            || !source.is_char_boundary(start as usize)
            || !source.is_char_boundary(end as usize)
        {
            return Err(());
        }
        spans.push((start, end));
        previous = end;
    }
    Ok(spans)
}

fn encode(spans: &[(u32, u32)]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8 + spans.len() * 8);
    bytes.extend_from_slice(b"JSP1");
    bytes.extend_from_slice(&(spans.len() as u32).to_le_bytes());
    for (start, end) in spans {
        bytes.extend_from_slice(&start.to_le_bytes());
        bytes.extend_from_slice(&end.to_le_bytes());
    }
    bytes
}

fn limits() -> io::Result<()> {
    for (resource, value) in LIMITS {
        let limit = libc::rlimit {
            rlim_cur: value,
            rlim_max: value,
        };
        // SAFETY: pointer references initialized stack storage during syscall.
        if unsafe { libc::setrlimit(resource, &limit) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn worker_exit() -> ! {
    std::panic::set_hook(Box::new(|_| {}));
    let result = worker_run();
    std::process::exit(if result.is_ok() { 0 } else { 1 });
}

fn worker_run() -> Result<(), ()> {
    limits().map_err(|_| ())?;
    // SAFETY: worker-only, before parsing any input; discard every inherited
    // descriptor except stdin/null stdout/null stderr and the protocol pipe.
    if unsafe { libc::syscall(libc::SYS_close_range, 4u32, u32::MAX, 0u32) } != 0 {
        return Err(());
    }
    sandbox()?;
    // SAFETY: fd 3 was explicitly installed by the parent, uniquely owned here.
    let mut output = unsafe { File::from_raw_fd(3) };
    let mut input = io::stdin().lock();
    let mut mode = [0];
    input.read_exact(&mut mode).map_err(|_| ())?;
    #[cfg(test)]
    if tests::fixture(mode[0], &mut output) {
        return Ok(());
    }
    if mode[0] > 1 {
        return Err(());
    }
    let mut source = String::new();
    input
        .take(MAX_SOURCE as u64 + 1)
        .read_to_string(&mut source)
        .map_err(|_| ())?;
    if source.len() > MAX_SOURCE {
        return Err(());
    }
    let spans = crate::javascript_grammar::spans(&source, mode[0] == 1)?;
    let bytes = encode(&spans);
    if bytes.len() > MAX_OUTPUT {
        return Err(());
    }
    output.write_all(&bytes).map_err(|_| ())
}

// A parser capability, not a mount/filesystem namespace. After dynamic loading
// it can allocate, use its pipes, and exit, but cannot open paths, connect,
// execute, fork, or clone. Unknown architectures/setup failures refuse parsing.
fn sandbox() -> Result<(), ()> {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return Err(());
    }
    let allow = [
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_close,
        libc::SYS_fstat,
        libc::SYS_brk,
        libc::SYS_mmap,
        libc::SYS_munmap,
        libc::SYS_mremap,
        libc::SYS_madvise,
        libc::SYS_mprotect,
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_sigaltstack,
        libc::SYS_getpid,
        libc::SYS_gettid,
        libc::SYS_tgkill,
        libc::SYS_futex,
        libc::SYS_sched_yield,
        libc::SYS_clock_gettime,
        libc::SYS_getrandom,
        libc::SYS_prlimit64,
        libc::SYS_exit,
        libc::SYS_exit_group,
    ];
    let instruction = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
    let mut filter = vec![
        instruction(0x20, 0, 0, 4),
        instruction(0x15, 1, 0, 0xc000003e),
        instruction(0x06, 0, 0, libc::SECCOMP_RET_KILL_PROCESS),
        instruction(0x20, 0, 0, 0),
    ];
    for syscall in allow {
        filter.push(instruction(0x15, 0, 1, syscall as u32));
        filter.push(instruction(0x06, 0, 0, libc::SECCOMP_RET_ALLOW));
    }
    filter.push(instruction(
        0x06,
        0,
        0,
        libc::SECCOMP_RET_ERRNO | libc::EPERM as u32,
    ));
    let program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };
    // SAFETY: post-exec single-purpose worker, valid immutable BPF storage
    // remains live until the kernel copies it. No untrusted data read yet.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0
        || unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0
        || unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program) } != 0
    {
        return Err(());
    }
    Ok(())
}

#[cfg(test)]
#[path = "grammar_worker_tests.rs"]
mod tests;
