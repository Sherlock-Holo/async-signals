//! Library for easier and safe Unix signal handling with async Stream.
//!
//! You can use this crate with any async runtime.

use std::collections::HashMap;
use std::convert::TryFrom;
use std::os::raw::c_int;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Context;
use std::task::Poll;

use crossbeam_queue::SegQueue;
use crossbeam_skiplist::{SkipMap, SkipSet};
use futures_util::task::AtomicWaker;
use futures_util::Stream;
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::Result;
use once_cell::sync::Lazy;

static ID_GEN: AtomicU64 = AtomicU64::new(0);
static SIGNAL_SET: Lazy<SkipMap<u64, Arc<InnerSignals>>> = Lazy::new(Default::default);

// why SIGNAL_SET use SkipMap but SIGNAL_RECORD use the Mutex<HashMap>?
//
// SIGNAL_SET is used to register the InnerSignals and notify the InnerSignals when signal is
// accepted, the signal handle function require **make syscall that are explicitly marked safe for
// signal handlers and only share global data using atomics**, so the SIGNAL_SET need a lock-free
// container
//
// however SIGNAL_RECORD just used to count the signal watcher, when a new signal is watched, the
// signal handle function need to be installed, and when no one watch a signal, the signal handle
// function need to be removed, we need a mutex to protect this operation
static SIGNAL_RECORD: Lazy<Mutex<HashMap<c_int, usize>>> = Lazy::new(Default::default);

extern "C" fn handle(receive_signal: c_int) {
    for signal in SIGNAL_SET.iter() {
        let signal = signal.value();
        if signal.wants.contains(&receive_signal) {
            signal.queue.push(receive_signal);
            signal.waker.wake();
        }
    }
}

#[derive(Debug)]
struct InnerSignals {
    queue: SegQueue<c_int>,
    waker: AtomicWaker,
    wants: SkipSet<c_int>,
}

impl InnerSignals {
    fn new(wants: SkipSet<c_int>) -> Arc<Self> {
        Arc::new(Self {
            queue: SegQueue::new(),
            waker: Default::default(),
            wants,
        })
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
    id: u64,
    inner: Arc<InnerSignals>,
}

impl Drop for Signals {
    fn drop(&mut self) {
        SIGNAL_SET.remove(&self.id);

        let mut signal_record = SIGNAL_RECORD.lock().unwrap();
        for drop_signal in self.inner.wants.iter() {
            let count = signal_record.get_mut(&*drop_signal).expect("must exist");
            *count -= 1;

            if *count > 0 {
                continue;
            }

            // no one wants to handle this signal, let default handler handle it.
            let default_handler = SigHandler::SigDfl;
            let default_action =
                SigAction::new(default_handler, SaFlags::SA_RESTART, SigSet::empty());

            unsafe {
                // TODO should I ignore error?
                let _ = sigaction(
                    Signal::try_from(*drop_signal).expect("checked"),
                    &default_action,
                );
            }
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
    pub fn new<I: IntoIterator<Item = c_int>>(signals: I) -> Result<Signals> {
        let handler = SigHandler::Handler(handle);
        let action = SigAction::new(handler, SaFlags::SA_RESTART, SigSet::empty());

        let mut signal_record = SIGNAL_RECORD.lock().unwrap();
        let wants = SkipSet::new();
        for signal in signals {
            wants.insert(signal);

            Self::record_and_install_signal_handle(&mut signal_record, signal, action)?;
        }

        let inner_signals = InnerSignals::new(wants);

        let id = ID_GEN.fetch_add(1, Ordering::Relaxed);
        SIGNAL_SET.insert(id, inner_signals.clone());

        Ok(Self {
            id,
            inner: inner_signals,
        })
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
    #[inline]
    pub fn add_signal(&mut self, signal: c_int) -> Result<()> {
        // signal is registered
        if self.inner.wants.get(&signal).is_some() {
            return Ok(());
        }

        let handler = SigHandler::Handler(handle);
        let action = SigAction::new(handler, SaFlags::SA_RESTART, SigSet::empty());
        let mut signal_record = SIGNAL_RECORD.lock().unwrap();

        Self::record_and_install_signal_handle(&mut signal_record, signal, action)?;

        drop(signal_record);

        self.inner.wants.insert(signal);

        Ok(())
    }

    fn record_and_install_signal_handle(
        signal_record: &mut HashMap<c_int, usize>,
        signal: c_int,
        action: SigAction,
    ) -> Result<()> {
        // increase signal record count
        if *signal_record
            .entry(signal)
            .and_modify(|count| *count += 1)
            .or_insert(1)
            == 1
        {
            // register handle when record is 1, that means no previous Signals watch this signal
            unsafe {
                sigaction(Signal::try_from(signal)?, &action)?;
            }
        }

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
