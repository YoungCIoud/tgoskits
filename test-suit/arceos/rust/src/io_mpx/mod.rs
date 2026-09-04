//! I/O multiplexing primitives: `eventfd` and `pipe` unit tests plus an
//! `epoll` smoke test. These cover the syscall surface that async runtimes
//! (e.g. tokio/mio) need to drive timers and wake-ups on ArceOS.

mod epoll;
mod eventfd;
mod pipe;
mod syscalls;

pub fn run() -> crate::TestResult {
    pipe::run()?;
    eventfd::run()?;
    epoll::run()?;
    Ok(())
}
