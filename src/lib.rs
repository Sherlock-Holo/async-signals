//! Library for easier and safe Unix signal handling with async Stream.
//!
//! You can use this crate with any async runtime.

use std::collections::HashMap;
use std::convert::TryFrom;
use std::io::{Read, Write};
use std::os::raw::c_int;
use std::os::unix::net::UnixStream;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::task::Context;
use std::task::Poll;
use std::{io, thread};

use crossbeam_queue::SegQueue;
use crossbeam_skiplist::SkipSet;
use futures_util::task::AtomicWaker;
use futures_util::Stream;
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use once_cell::sync::Lazy;

static PIPE: Lazy<io::Result<(UnixStream, UnixStream)>> = Lazy::new(|| {
    let (reader, write) = UnixStream::pair()?;
    // don't block the signal handler
    write.set_nonblocking(true)?;

    Ok((reader, write))
});

static SIGNALS_SET: Lazy<SignalsSet> = Lazy::new(SignalsSet::default);

#[derive(Debug)]
struct SignalsSet {
    id_gen: AtomicU32,
    signal_notifiers: Mutex<SignalNotifiers>,
    start_signal_handle_thread: Once,
}

impl Default for SignalsSet {
    fn default() -> Self {
        Self {
            id_gen: Default::default(),
            signal_notifiers: Mutex::new(Default::default()),
            start_signal_handle_thread: Once::new(),
        }
    }
}

impl SignalsSet {
    fn generate_id(&self) -> u32 {
        self.id_gen.fetch_add(1, Ordering::Relaxed)
    }

    fn start_signal_handle_thread(&'static self) {
        self.start_signal_handle_thread.call_once(|| {
            thread::spawn(|| {
                let mut reader = get_pipe_reader();
                loop {
                    let mut buf = [0];
                    (&mut reader).read_exact(&mut buf).unwrap_or_else(|err| {
                        panic!("pipe reader read return error {err}, that should not happened")
                    });

                    let signal_notifiers = self.signal_notifiers.lock().unwrap();
                    for signal_inner in signal_notifiers.notifiers.values() {
                        if signal_inner.interest(buf[0] as _) {
                            signal_inner.notify(buf[0] as _);
                        }
                    }
                }
            });
        })
    }
}

#[derive(Debug, Default)]
struct SignalNotifiers {
    installed_signals: HashMap<c_int, usize>,
    notifiers: HashMap<u32, Arc<SignalsInner>>,
}

impl SignalNotifiers {
    fn add_signal(&mut self, signal: c_int) -> io::Result<()> {
        let count = self
            .installed_signals
            .entry(signal)
            .and_modify(|count| *count += 1)
            .or_insert(1);
        if *count == 1 {
            let handler = SigHandler::Handler(handle);
            let action = SigAction::new(handler, SaFlags::SA_RESTART, SigSet::empty());

            unsafe {
                sigaction(Signal::try_from(signal)?, &action)?;
            }
        }

        Ok(())
    }

    fn remove_signal(&mut self, signal: c_int) -> io::Result<()> {
        if let Some(count) = self.installed_signals.get_mut(&signal) {
            *count -= 1;
            if *count == 0 {
                let action =
                    SigAction::new(SigHandler::SigDfl, SaFlags::SA_RESTART, SigSet::empty());

                unsafe {
                    let signal = Signal::try_from(signal)
                        .unwrap_or_else(|_| panic!("signal {signal} should be valid"));
                    let _ = sigaction(signal, &action);
                }

                self.installed_signals.remove(&signal);
            }
        }

        Ok(())
    }

    fn add_notifier(&mut self, id: u32, signals_inner: Arc<SignalsInner>) {
        self.notifiers.insert(id, signals_inner);
    }

    fn remove_notifier(&mut self, id: u32) {
        self.notifiers.remove(&id);
    }
}

fn get_pipe_writer() -> &'static UnixStream {
    match PIPE.as_ref() {
        Err(_) => unreachable!("if init pipe failed, should not get pipe writer"),
        Ok((_, writer)) => writer,
    }
}

fn get_pipe_reader() -> &'static UnixStream {
    match PIPE.as_ref() {
        Err(_) => unreachable!("if init pipe failed, should not get pipe writer"),
        Ok((reader, _)) => reader,
    }
}

extern "C" fn handle(receive_signal: c_int) {
    let _ = get_pipe_writer().write(&[receive_signal as _]);
}

#[derive(Debug)]
struct SignalsInner {
    queue: SegQueue<c_int>,
    waker: AtomicWaker,
    interests: SkipSet<c_int>,
}

impl SignalsInner {
    fn new(interests: SkipSet<c_int>) -> Self {
        Self {
            queue: Default::default(),
            waker: Default::default(),
            interests,
        }
    }

    fn interest(&self, signal: c_int) -> bool {
        self.interests.contains(&signal)
    }

    fn notify(&self, signal: c_int) {
        self.queue.push(signal);
        self.waker.wake();
    }
}

/// Handle unix signal like `signal_hook::iterator::Signals`, receive signals
/// with `futures::stream::Stream`.
///
/// If multi `Signals` register a same signal, all of them will receive the signal.
///
/// If you drop all `Signals` which handle a signal like `SIGINT`, when process receive
/// this signal, will use system default handler.
///
/// # Notes:
/// You can't handle `SIGKILL` or `SIGSTOP`.
#[derive(Debug)]
pub struct Signals {
    id: u32,
    inner: Arc<SignalsInner>,
}

impl Drop for Signals {
    fn drop(&mut self) {
        let mut signal_notifiers = SIGNALS_SET.signal_notifiers.lock().unwrap();
        signal_notifiers.remove_notifier(self.id);

        for signal in &self.inner.interests {
            let _ = signal_notifiers.remove_signal(*signal.value());
        }
    }
}

impl Signals {
    /// Creates the `Signals` structure, all signals will be registered.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_signals::Signals;
    /// use futures_util::StreamExt;
    /// use nix::sys;
    /// use nix::unistd;
    ///
    /// #[tokio::main(flavor = "current_thread")]
    /// async fn main() {
    ///     let mut signals = Signals::new(vec![libc::SIGINT]).unwrap();
    ///
    ///     let pid = unistd::getpid();
    ///     sys::signal::kill(pid, Some(sys::signal::SIGINT)).unwrap();
    ///
    ///     let signal = signals.next().await.unwrap();
    ///
    ///     assert_eq!(signal, libc::SIGINT);
    /// }
    /// ```
    pub fn new<I: IntoIterator<Item = c_int>>(signals: I) -> io::Result<Signals> {
        Self::check_pipe()?;

        SIGNALS_SET.start_signal_handle_thread();

        let signals = signals.into_iter().collect::<SkipSet<_>>();
        let id = SIGNALS_SET.generate_id();
        let mut signal_notifiers = SIGNALS_SET.signal_notifiers.lock().unwrap();
        for signal in &signals {
            signal_notifiers.add_signal(*signal.value())?;
        }

        let inner = Arc::new(SignalsInner::new(signals));
        signal_notifiers.add_notifier(id, inner.clone());

        Ok(Self { id, inner })
    }

    fn check_pipe() -> io::Result<()> {
        PIPE.as_ref()
            .map(|_| ())
            .map_err(|err| io::Error::new(err.kind(), err.to_string()))
    }

    /// Registers another signal to a created `Signals`.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_signals::Signals;
    /// use futures_util::StreamExt;
    /// use nix::sys;
    /// use nix::unistd;
    ///
    /// #[tokio::main(flavor = "current_thread")]
    /// async fn main() {
    ///     let mut signals = Signals::new(vec![libc::SIGHUP]).unwrap();
    ///
    ///     signals.add_signal(libc::SIGINT).unwrap();
    ///
    ///     let pid = unistd::getpid();
    ///     sys::signal::kill(pid, Some(sys::signal::SIGINT)).unwrap();
    ///
    ///     let signal = signals.next().await.unwrap();
    ///
    ///     assert_eq!(signal, libc::SIGINT);
    /// }
    /// ```
    pub fn add_signal(&mut self, signal: c_int) -> io::Result<()> {
        SIGNALS_SET
            .signal_notifiers
            .lock()
            .unwrap()
            .add_signal(signal)?;
        self.inner.interests.insert(signal);

        Ok(())
    }
}

impl Stream for Signals {
    type Item = c_int;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // register at first, make sure when ready, we can be notified
        self.inner.waker.register(cx.waker());

        if let Some(signal) = self.inner.queue.pop() {
            return Poll::Ready(Some(signal));
        }

        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;
    use nix::sys;
    use nix::unistd;

    use super::*;

    #[tokio::test]
    async fn interrupt() {
        let mut signal = Signals::new(vec![libc::SIGINT]).unwrap();

        let pid = unistd::getpid();

        sys::signal::kill(pid, Some(sys::signal::SIGINT)).unwrap();

        let interrupt = signal.next().await.unwrap();

        assert_eq!(interrupt, libc::SIGINT);
    }

    #[tokio::test]
    async fn add_signal() {
        let mut signal = Signals::new(vec![libc::SIGHUP]).unwrap();

        signal.add_signal(libc::SIGINT).unwrap();

        let pid = unistd::getpid();

        sys::signal::kill(pid, Some(sys::signal::SIGINT)).unwrap();

        let interrupt = signal.next().await.unwrap();

        assert_eq!(interrupt, libc::SIGINT);
    }

    #[tokio::test]
    async fn multi_signals() {
        let mut signal1 = Signals::new(vec![libc::SIGINT]).unwrap();
        let mut signal2 = Signals::new(vec![libc::SIGINT]).unwrap();

        let pid = unistd::getpid();

        sys::signal::kill(pid, Some(sys::signal::SIGINT)).unwrap();

        assert_eq!(signal1.next().await.unwrap(), libc::SIGINT);

        assert_eq!(signal2.next().await.unwrap(), libc::SIGINT);
    }
}
