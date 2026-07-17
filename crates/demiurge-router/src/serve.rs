//! Live TCP serving: bounded accept loop, admission, and backend proxying.
//! [DEMI-DP-RCU] [DEMI-XDP-SHED]

use std::io::{self, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use demiurge_cost::DATAPLANE_MAX_CONNS;
use demiurge_dataplane::AdmitBucket;
#[cfg(target_os = "linux")]
use demiurge_dataplane::{IoUringAcceptLoop, IoUringForwarder, IoUringProxySession};

#[cfg(target_os = "linux")]
use crate::http::MAX_HEAD;
use crate::http::{parse_request_identity, read_head};
use crate::routing::{route_with_identity, RouteError, RoutePath};
use crate::{Backend, RequestId, Router};

/// Reverse-direction byte pump job for the shared pump pool.
struct PumpJob {
    from: TcpStream,
    to: TcpStream,
    done: SyncSender<()>,
}

fn pump_pool_size() -> usize {
    std::env::var("DEMIURGE_PUMP_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| worker_thread_count().max(32))
}

fn pump_pool_tx() -> &'static SyncSender<PumpJob> {
    static TX: OnceLock<SyncSender<PumpJob>> = OnceLock::new();
    TX.get_or_init(|| {
        let workers = pump_pool_size();
        let (tx, rx) = mpsc::sync_channel::<PumpJob>(workers.saturating_mul(4).max(32));
        let shared = Arc::new(Mutex::new(rx));
        for _ in 0..workers {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("demiurge-pump".into())
                .spawn(move || loop {
                    let job = {
                        let guard = shared.lock().expect("pump rx");
                        match guard.recv() {
                            Ok(j) => j,
                            Err(_) => break,
                        }
                    };
                    let mut from = job.from;
                    let mut to = job.to;
                    let _ = io::copy(&mut from, &mut to);
                    let _ = to.shutdown(Shutdown::Write);
                    let _ = job.done.send(());
                })
                .expect("pump worker");
        }
        tx
    })
}

struct InflightGuard<'a>(&'a Backend);

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.0.decr_inflight();
    }
}

struct AdmitGuard(Arc<AdmitBucket>);

impl Drop for AdmitGuard {
    fn drop(&mut self) {
        self.0.release(1);
    }
}

/// Userspace admit for one TCP connection — at most one guard per `handle_conn`.
enum AdmitConn {
    Shed,
    Proceed(Option<AdmitGuard>),
}

fn admit_conn(router: &Router) -> AdmitConn {
    let kernel_attached = router.kernel_admit_attached();
    if !router.admit_mode().uses_userspace_admit(kernel_attached) {
        return AdmitConn::Proceed(None);
    }
    if router.admit_bucket().try_admit().is_err() {
        return AdmitConn::Shed;
    }
    AdmitConn::Proceed(Some(AdmitGuard(Arc::clone(router.admit_bucket()))))
}

fn proxy_to_backend(
    client: &mut TcpStream,
    head: &[u8],
    backend: &Backend,
    #[cfg(target_os = "linux")] io_uring_session: Option<&mut IoUringProxySession>,
) -> io::Result<()> {
    backend.incr_inflight();
    let _guard = InflightGuard(backend);

    let mut upstream = TcpStream::connect(backend.addr)?;
    upstream.write_all(head)?;

    #[cfg(target_os = "linux")]
    if let Some(session) = io_uring_session {
        use std::os::fd::AsRawFd;
        let up_read = upstream.try_clone()?;
        let client_write = client.try_clone()?;
        let client_read = client.try_clone()?;
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        // io_uring reverse direction still needs its own session; use a one-shot
        // thread (pool is std::io::copy based). Cap concurrency via max_conns.
        thread::spawn(move || {
            if let Ok(mut pump_session) = IoUringProxySession::new() {
                let _ = pump_session.copy_stream(
                    up_read.as_raw_fd(),
                    client_write.as_raw_fd(),
                    256 * 1024,
                );
            }
            let _ = client_write.shutdown(Shutdown::Write);
            let _ = done_tx.send(());
        });
        session.copy_stream(client_read.as_raw_fd(), upstream.as_raw_fd(), 256 * 1024)?;
        let _ = upstream.shutdown(Shutdown::Write);
        let _ = done_rx.recv();
        return Ok(());
    }

    let up_read = upstream.try_clone()?;
    let client_write = client.try_clone()?;
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    // Reverse direction on the bounded pump pool (join via done ack).
    match pump_pool_tx().try_send(PumpJob {
        from: up_read,
        to: client_write,
        done: done_tx,
    }) {
        Ok(()) => {}
        Err(mpsc::TrySendError::Full(job) | mpsc::TrySendError::Disconnected(job)) => {
            // Pool saturated: one-shot fallback (still joinable).
            thread::spawn(move || {
                let mut from = job.from;
                let mut to = job.to;
                let _ = io::copy(&mut from, &mut to);
                let _ = to.shutdown(Shutdown::Write);
                let _ = job.done.send(());
            });
        }
    }
    let mut client_read = client.try_clone()?;
    let _ = io::copy(&mut client_read, &mut upstream);
    let _ = upstream.shutdown(Shutdown::Write);
    let _ = done_rx.recv();
    Ok(())
}

fn handle_disaggregated(
    mut client: TcpStream,
    head: Vec<u8>,
    router: Arc<Router>,
    prefill: Arc<Backend>,
    request_id: RequestId,
    prompt_tokens: u64,
    #[cfg(target_os = "linux")] io_uring_session: Option<&mut IoUringProxySession>,
) -> io::Result<()> {
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let router2 = Arc::clone(&router);
    let prefill_label = prefill.label.clone();
    let _prefill_worker = crate::routing::dispatch_prefill(
        prefill,
        head.clone(),
        request_id,
        prompt_tokens,
        move |signals, io_result| {
            let response = match io_result {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("demiurge: prefill I/O error for {prefill_label}: {e}");
                    let _ = done_tx.send(Err(RouteError::PrefillIo(e.to_string())));
                    return;
                }
            };
            let placement = match crate::routing::on_prefill_complete(
                &router2,
                &signals,
                &response,
                &prefill_label,
            ) {
                Ok(p) => p,
                Err(e) => {
                    let _ = done_tx.send(Err(e));
                    return;
                }
            };
            let _ = done_tx.send(Ok(placement));
        },
    );

    let placement = match done_rx
        .recv()
        .map_err(|_| io::Error::other("prefill channel"))?
    {
        Ok(p) => p,
        Err(
            RouteError::NoBackend
            | RouteError::HandoffMissing
            | RouteError::KvAdmitRejected
            | RouteError::PrefillIo(_),
        ) => {
            let _ =
                client.write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n");
            return Ok(());
        }
    };

    let result = proxy_to_backend(
        &mut client,
        &head,
        placement.backend.as_ref(),
        #[cfg(target_os = "linux")]
        io_uring_session,
    );
    drop(placement.reservation);
    result
}

fn handle_conn(client: TcpStream, router: Arc<Router>) -> io::Result<()> {
    let _admit_guard = match admit_conn(&router) {
        AdmitConn::Shed => {
            let mut client = client;
            let _ =
                client.write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n");
            return Ok(());
        }
        AdmitConn::Proceed(guard) => guard,
    };

    let mut client = client;
    #[cfg(target_os = "linux")]
    let mut io_uring_session = router.io_uring_proxy_session();

    let head = {
        #[cfg(target_os = "linux")]
        {
            if let Some(ref mut session) = io_uring_session {
                use std::os::fd::AsRawFd;
                session.read_http_head(client.as_raw_fd(), MAX_HEAD)?
            } else {
                read_head(&mut client)?
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            read_head(&mut client)?
        }
    };
    // Cache-domain isolation on the live path: resolve the edge-authenticated
    // identity (if any) so warmth discounts are gated per tenant/group.
    // Missing headers fail closed to identity-less routing. [DEMI-S1-DOMAIN]
    let identity = parse_request_identity(&head);
    match route_with_identity(&router, &head, identity.as_ref()) {
        Ok(RoutePath::Colocated(b) | RoutePath::DecodeOnly(b)) => proxy_to_backend(
            &mut client,
            &head,
            b.as_ref(),
            #[cfg(target_os = "linux")]
            io_uring_session.as_mut(),
        ),
        Ok(RoutePath::Disaggregated {
            prefill,
            request_id,
            prompt_tokens,
        }) => handle_disaggregated(
            client,
            head,
            router,
            prefill,
            request_id,
            prompt_tokens,
            #[cfg(target_os = "linux")]
            io_uring_session.as_mut(),
        ),
        Err(
            RouteError::NoBackend
            | RouteError::HandoffMissing
            | RouteError::KvAdmitRejected
            | RouteError::PrefillIo(_),
        ) => {
            let _ =
                client.write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n");
            Ok(())
        }
    }
}

/// RAII slot in the live-connection budget; releases on drop.
struct ConnSlot(Arc<AtomicUsize>);

impl Drop for ConnSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

struct ConnJob {
    client: TcpStream,
    _slot: ConnSlot,
}

/// Reserve a connection slot, or `None` when the cap is reached.
fn try_conn_slot(live: &Arc<AtomicUsize>, max_conns: usize) -> Option<ConnSlot> {
    // Increment-first (mirrors AdmitBucket): racing acceptors can never
    // land more than `max_conns` slots because each checks its own result.
    let prev = live.fetch_add(1, Ordering::Relaxed);
    if prev >= max_conns {
        live.fetch_sub(1, Ordering::Relaxed);
        return None;
    }
    Some(ConnSlot(Arc::clone(live)))
}

fn worker_thread_count() -> usize {
    std::env::var("DEMIURGE_WORKER_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(256)
}

pub fn serve(listener: TcpListener, router: Arc<Router>) -> io::Result<()> {
    serve_with_max_conns(listener, router, DATAPLANE_MAX_CONNS as usize)
}

/// Spawn the bounded handle_conn worker pool; returns per-worker senders.
fn spawn_conn_workers(router: &Arc<Router>, workers: usize) -> Vec<mpsc::SyncSender<ConnJob>> {
    let per_worker_backlog = 2usize;
    let mut senders = Vec::with_capacity(workers);
    for _ in 0..workers {
        let (tx, rx) = mpsc::sync_channel::<ConnJob>(per_worker_backlog);
        senders.push(tx);
        let router = Arc::clone(router);
        thread::Builder::new()
            .name("demiurge-conn".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    let _ = handle_conn(job.client, Arc::clone(&router));
                }
            })
            .expect("conn worker");
    }
    senders
}

/// Round-robin try_send into per-worker queues; sheds 503 when all are full.
fn dispatch_conn(
    mut client: TcpStream,
    live: &Arc<AtomicUsize>,
    max_conns: usize,
    senders: &[mpsc::SyncSender<ConnJob>],
    next: &mut usize,
) -> bool {
    let Some(slot) = try_conn_slot(live, max_conns) else {
        let _ = client.write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n");
        return true;
    };
    let mut job = ConnJob {
        client,
        _slot: slot,
    };
    let workers = senders.len();
    for attempt in 0..workers {
        let idx = (*next + attempt) % workers;
        match senders[idx].try_send(job) {
            Ok(()) => {
                *next = idx.wrapping_add(1);
                return true;
            }
            Err(mpsc::TrySendError::Full(j)) => {
                job = j;
            }
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
        }
    }
    drop(job._slot);
    let _ = job
        .client
        .write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n");
    true
}

/// `serve`, with an explicit cap on concurrent proxied connections. Excess
/// connections shed `503` immediately instead of spawning an unbounded
/// thread per accept — the L7 analogue of the admit bucket.
///
/// On Linux with `DEMIURGE_IOURING=1` (or a router built with io_uring), accept
/// is owned by an io_uring `Accept` loop (G6). Otherwise std `incoming()` is used.
/// Accepted sockets are dispatched round-robin to per-worker queues so dequeue
/// never contends on a shared mutex.
pub fn serve_with_max_conns(
    listener: TcpListener,
    router: Arc<Router>,
    max_conns: usize,
) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    if router.io_uring_enabled() || IoUringForwarder::io_uring_enabled_from_env() {
        return serve_iouring_accept(listener, router, max_conns);
    }
    serve_std_accept(listener, router, max_conns)
}

fn serve_std_accept(
    listener: TcpListener,
    router: Arc<Router>,
    max_conns: usize,
) -> io::Result<()> {
    let live = Arc::new(AtomicUsize::new(0));
    let max_conns = max_conns.max(1);
    let workers = worker_thread_count().max(1);
    let senders = spawn_conn_workers(&router, workers);
    let _ = pump_pool_tx();

    let mut next = 0usize;
    for conn in listener.incoming() {
        let Ok(client) = conn else { continue };
        if !dispatch_conn(client, &live, max_conns, &senders, &mut next) {
            break;
        }
    }
    Ok(())
}

/// Linux: io_uring `Accept` owns the listen fd; workers still run `handle_conn`.
#[cfg(target_os = "linux")]
fn serve_iouring_accept(
    listener: TcpListener,
    router: Arc<Router>,
    max_conns: usize,
) -> io::Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let live = Arc::new(AtomicUsize::new(0));
    let max_conns = max_conns.max(1);
    let workers = worker_thread_count().max(1);
    let senders = spawn_conn_workers(&router, workers);
    let _ = pump_pool_tx();

    // Keep `listener` alive so the fd is not closed under the accept loop.
    let mut acceptor = IoUringAcceptLoop::new(listener.as_raw_fd())?;
    let mut next = 0usize;
    loop {
        let client_fd = match acceptor.accept_one() {
            Ok(fd) => fd,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        // SAFETY: fd freshly returned by io_uring Accept; sole owner.
        let client = unsafe { TcpStream::from_raw_fd(client_fd) };
        if !dispatch_conn(client, &live, max_conns, &senders, &mut next) {
            break;
        }
    }
    Ok(())
}
