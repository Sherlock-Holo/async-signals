//! Library for easier and safe Unix signal handling, and async!
//!
//! You can use this crate with `tokio`, `async-std` or `futures::executor` runtime.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::io::Result;
use std::os::raw::c_int;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::task::{Poll, Waker};
use std::thread;

use futures_util::stream::Stream;
use futures_util::task::Context;
use mio::{Events, Poll as MioPoll, PollOpt, Ready, Token};
use signal_hook::iterator::Signals as SysSignals;

use lazy_static::lazy_static;

lazy_static! {
    static ref WAKERS: Arc<Mutex<HashMap<Token, Waker>>> = Arc::new(Mutex::new(HashMap::new()));
    static ref TOKEN_GEN: AtomicU64 = AtomicU64::new(1);
    static ref POLL: MioPoll = MioPoll::new().unwrap();
}

const REACTOR_INIT_ONCE: Once = Once::new();

fn reactor_loop() {
    let mut events = Events::with_capacity(1024);

    loop {
        POLL.poll(&mut events, None).expect("poll signal failed");

        let mut wakers = WAKERS.lock().unwrap();

        for event in &events {
            let token = event.token();

            if let Some(waker) = wakers.remove(&token) {
                waker.wake();
            }
        }
    }
}

/// Handle unix signal like `signal_hook::iterator::Signals`, receive signals
/// with `futures::stream::Stream`.
///
/// If you want to unregister all signal which register to a `Signals`, just drop it, it will
/// unregister all signals.
pub struct Signals {
    token: Token,
    sys_signals: SysSignals,
    signals_buf: Vec<c_int>,
    is_registered: bool,
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
    ///     let signal = signal.unwrap();
    ///
    ///     assert_eq!(signal, libc::SIGINT);
    /// }
    /// ```
    pub fn new<I, S>(signals: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Borrow<c_int>,
    {
        let token = TOKEN_GEN.fetch_add(1, Ordering::Relaxed);

        let signal = Self {
            token: Token(token as usize),
            sys_signals: SysSignals::new(signals)?,
            signals_buf: vec![],
            is_registered: false,
        };

        Ok(signal)
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
    ///     let signal = signal.unwrap();
    ///
    ///     assert_eq!(signal, libc::SIGINT);
    /// }
    /// ```
    #[inline]
    pub fn add_signal(&self, signal: c_int) -> Result<()> {
        self.sys_signals.add_signal(signal)
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        WAKERS.lock().unwrap().remove(&self.token);

        self.sys_signals.close();

        // TODO should I ignore the error?
        let _ = POLL.deregister(&self.sys_signals);
    }
}

impl Stream for Signals {
    type Item = Result<c_int>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let myself = self.get_mut();

        if let Some(signal) = myself.signals_buf.pop() {
            return Poll::Ready(Some(Ok(signal)));
        }

        let pending = myself.sys_signals.pending().into_iter();

        myself.signals_buf.extend(pending);

        if let Some(signal) = myself.signals_buf.pop() {
            return Poll::Ready(Some(Ok(signal)));
        }

        WAKERS
            .lock()
            .unwrap()
            .insert(myself.token, cx.waker().clone());

        let result = if myself.is_registered {
            POLL.reregister(
                &myself.sys_signals,
                myself.token,
                Ready::readable(),
                PollOpt::edge() | PollOpt::oneshot(),
            )
        } else {
            // lazy init reactor thread
            REACTOR_INIT_ONCE.call_once(|| {
                thread::spawn(reactor_loop);
            });

            myself.is_registered = true;

            POLL.register(
                &myself.sys_signals,
                myself.token,
                Ready::readable(),
                PollOpt::edge() | PollOpt::oneshot(),
            )
        };

        if let Err(err) = result {
            return Poll::Ready(Some(Err(err)));
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

    #[async_std::test]
    async fn interrupt() {
        let mut signal = Signals::new(vec![libc::SIGINT]).unwrap();

        let pid = unistd::getpid();

        sys::signal::kill(pid, Some(sys::signal::SIGINT)).unwrap();

        let interrupt = signal.next().await.unwrap().unwrap();

        assert_eq!(interrupt, libc::SIGINT);
    }

    #[async_std::test]
    async fn add_signal() {
        let mut signal = Signals::new(vec![libc::SIGHUP]).unwrap();

        signal.add_signal(libc::SIGINT).unwrap();

        let pid = unistd::getpid();

        sys::signal::kill(pid, Some(sys::signal::SIGINT)).unwrap();

        let interrupt = signal.next().await.unwrap().unwrap();

        assert_eq!(interrupt, libc::SIGINT);
    }
}
