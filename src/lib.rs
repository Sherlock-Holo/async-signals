//! Library for easier and safe Unix signal handling with async Stream.
//!
//! You can use this crate with any async runtime.

use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::TryFrom;
use std::os::raw::c_int;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::task::Context;
use std::task::{Poll, Waker};

use futures_util::stream::Stream;
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::Result;

use lazy_static::lazy_static;

lazy_static! {
    static ref ID_GEN: AtomicU64 = AtomicU64::new(0);
    static ref SIGNAL_SET: RwLock<HashMap<u64, Arc<Mutex<InnerSignals>>>> =
        RwLock::new(HashMap::new());
    static ref SIGNAL_RECORD: Mutex<HashMap<c_int, usize>> = Mutex::new(HashMap::new());
}

extern "C" fn handle(receive_signal: c_int) {
    let signal_map = SIGNAL_SET.read().unwrap();

    for (_, signal) in signal_map.iter() {
        let mut signal = signal.lock().unwrap();

        if signal.wants.contains(&receive_signal) {
            signal.queue.push_back(receive_signal);

            if let Some(waker) = signal.waker.take() {
                waker.wake();
            }
        }
    }
}

#[derive(Debug)]
struct InnerSignals {
    queue: VecDeque<c_int>,
    waker: Option<Waker>,
    wants: HashSet<c_int>,
}

impl InnerSignals {
    fn new(wants: HashSet<c_int>) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            queue: VecDeque::new(),
            waker: None,
            wants,
        }))
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
    inner: Arc<Mutex<InnerSignals>>,
}

impl Drop for Signals {
    fn drop(&mut self) {
        let mut signal_set = SIGNAL_SET.write().unwrap();

        let mut signal_record = SIGNAL_RECORD.lock().unwrap();

        signal_set.remove(&self.id);

        let inner = self.inner.lock().unwrap();

        for drop_signal in inner.wants.iter() {
            let count = signal_record.get_mut(drop_signal).expect("must exist");

            if *count > 1 {
                *count -= 1;
                continue;
            }

            signal_record.remove(drop_signal);

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
    /// #[async_std::main]
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
        let id = ID_GEN.fetch_add(1, Ordering::Relaxed);

        let mut signal_set = SIGNAL_SET.write().unwrap();

        let mut signal_record = SIGNAL_RECORD.lock().unwrap();

        let handler = SigHandler::Handler(handle);

        let action = SigAction::new(handler, SaFlags::SA_RESTART, SigSet::empty());

        let mut wants = HashSet::new();

        for signal in signals {
            // register handle
            unsafe {
                sigaction(Signal::try_from(signal)?, &action)?;
            }

            wants.insert(signal);

            // increase signal record count
            signal_record
                .entry(signal)
                .and_modify(|count| *count += 1)
                .or_insert(1);
        }

        let inner_signals = InnerSignals::new(wants);

        signal_set.insert(id, inner_signals.clone());

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
    /// #[async_std::main]
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
        let mut signal_record = SIGNAL_RECORD.lock().unwrap();

        let handler = SigHandler::Handler(handle);

        let action = SigAction::new(handler, SaFlags::SA_RESTART, SigSet::empty());

        // register handle
        unsafe {
            sigaction(Signal::try_from(signal)?, &action)?;
        }

        let mut inner = self.inner.lock().unwrap();

        inner.wants.insert(signal);

        // increase signal record count
        signal_record
            .entry(signal)
            .and_modify(|count| *count += 1)
            .or_insert(1);

        Ok(())
    }
}

impl Stream for Signals {
    type Item = c_int;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut inner = self.inner.lock().unwrap();

        if let Some(signal) = inner.queue.pop_front() {
            return Poll::Ready(Some(signal));
        }

        inner.waker.replace(cx.waker().clone());

        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;
    use nix::sys;
    use nix::unistd;

    use super::*;

    #[async_std::test]
    async fn interrupt() {
        let mut signal = Signals::new(vec![libc::SIGINT]).unwrap();

        let pid = unistd::getpid();

        sys::signal::kill(pid, Some(sys::signal::SIGINT)).unwrap();

        let interrupt = signal.next().await.unwrap();

        assert_eq!(interrupt, libc::SIGINT);
    }

    #[async_std::test]
    async fn add_signal() {
        let mut signal = Signals::new(vec![libc::SIGHUP]).unwrap();

        signal.add_signal(libc::SIGINT).unwrap();

        let pid = unistd::getpid();

        sys::signal::kill(pid, Some(sys::signal::SIGINT)).unwrap();

        let interrupt = signal.next().await.unwrap();

        assert_eq!(interrupt, libc::SIGINT);
    }

    #[async_std::test]
    async fn multi_signals() {
        let mut signal1 = Signals::new(vec![libc::SIGINT]).unwrap();

        let mut signal2 = Signals::new(vec![libc::SIGINT]).unwrap();

        let pid = unistd::getpid();

        sys::signal::kill(pid, Some(sys::signal::SIGINT)).unwrap();

        assert_eq!(signal1.next().await.unwrap(), libc::SIGINT);

        assert_eq!(signal2.next().await.unwrap(), libc::SIGINT);
    }
}
