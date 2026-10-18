//! The tokio runtime the network and keyring work runs on, beside GTK's main loop. A future
//! started with [`run`] runs there and is awaited from the main loop.

use std::future::Future;
use std::sync::OnceLock;

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("signet-io")
            .build()
            .expect("a tokio runtime")
    })
}

/// Runs `future` on the runtime; the returned future, awaited on the main loop, gives its result.
pub fn run<F>(future: F) -> impl Future<Output = F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let handle = runtime().spawn(future);
    async move { handle.await.expect("a background task panicked") }
}
