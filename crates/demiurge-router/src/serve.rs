//! Live TCP serving: bounded accept loop, admission, and backend proxying.
//! [DEMI-DP-RCU] [DEMI-XDP-SHED]

use std::io::{self, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use demiurge_cost::DATAPLANE_MAX_CONNS;
use demiurge_dataplane::AdmitBucket;
#[cfg(target_os = "linux")]
use demiurge_dataplane::IoUringProxySession;

#[cfg(target_os = "linux")]
use crate::http::MAX_HEAD;
use crate::http::{parse_request_identity, read_head};
use crate::routing::{route_with_identity, RouteError, RoutePath};
use crate::{Backend, RequestId, Router};

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
        let pump = thread::spawn(move || {
            if let Ok(mut pump_session) = IoUringProxySession::new() {
                let _ = pump_session.copy_stream(
                    up_read.as_raw_fd(),
                    client_write.as_raw_fd(),
                    256 * 1024,
                );
            }
            let _ = client_write.shutdown(Shutdown::Write);
        });
        session.copy_stream(client_read.as_raw_fd(), upstream.as_raw_fd(), 256 * 1024)?;
        let _ = upstream.shutdown(Shutdown::Write);
        let _ = pump.join();
        return Ok(());
    }

    let mut up_read = upstream.try_clone()?;
    let mut client_write = client.try_clone()?;
    let pump = thread::spawn(move || {
        let _ = io::copy(&mut up_read, &mut client_write);
        let _ = client_write.shutdown(Shutdown::Write);
    });
    let mut client_read = client.try_clone()?;
    let _ = io::copy(&mut client_read, &mut upstream);
    let _ = upstream.shutdown(Shutdown::Write);
    let _ = pump.join();
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
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
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
    std::hint::black_box(router.dataplane_pi());

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

/// `serve`, with an explicit cap on concurrent proxied connections. Excess
/// connections shed `503` immediately instead of spawning an unbounded
/// thread per accept — the L7 analogue of the admit bucket.
pub fn serve_with_max_conns(
    listener: TcpListener,
    router: Arc<Router>,
    max_conns: usize,
) -> io::Result<()> {
    let live = Arc::new(AtomicUsize::new(0));
    let max_conns = max_conns.max(1);
    let workers = worker_thread_count();
    let backlog = workers.saturating_mul(2).max(1);
    let (tx, rx) = mpsc::sync_channel::<ConnJob>(backlog);
    let shared_rx = Arc::new(Mutex::new(rx));

    for _ in 0..workers {
        let rx = Arc::clone(&shared_rx);
        let router = Arc::clone(&router);
        thread::spawn(move || loop {
            let job = {
                let guard = rx.lock().expect("worker rx");
                match guard.recv() {
                    Ok(job) => job,
                    Err(_) => break,
                }
            };
            let _ = handle_conn(job.client, Arc::clone(&router));
        });
    }

    for conn in listener.incoming() {
        let Ok(mut client) = conn else { continue };
        let Some(slot) = try_conn_slot(&live, max_conns) else {
            let _ =
                client.write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n");
            continue;
        };
        match tx.try_send(ConnJob {
            client,
            _slot: slot,
        }) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(mut job)) => {
                drop(job._slot);
                let _ = job.client.write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n",
                );
            }
            Err(mpsc::TrySendError::Disconnected(_)) => break,
        }
    }
    Ok(())
}
