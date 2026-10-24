//! The private tokio runtime, endpoint setup and close-reason wording.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Mutex;
use std::time::Duration;

use iroh::endpoint::{ConnectionError, QuicTransportConfig, presets};
use iroh::{Endpoint, EndpointAddr, EndpointId, TransportAddr};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::wire::{ALPN, close};

/// Which network the endpoint lives on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Net {
    /// number0 relays + n0 DNS/pkarr address lookup: dial by bare endpoint id.
    Public,
    /// Loopback only, no relays, no lookup (tests).
    Local,
}

/// Where a guest dials.
#[derive(Clone, Debug)]
pub(crate) struct Target {
    pub id: EndpointId,
    pub direct: Vec<SocketAddr>,
}

impl Target {
    pub(crate) fn addr(&self) -> EndpointAddr {
        EndpointAddr::from_parts(self.id, self.direct.iter().copied().map(TransportAddr::Ip))
    }
}

pub(crate) async fn bind(net: Net, accept: bool) -> Result<Endpoint, String> {
    let transport = QuicTransportConfig::builder()
        .max_idle_timeout(Duration::from_secs(15).try_into().ok())
        .build();
    let mut b = match net {
        Net::Public => Endpoint::builder(presets::N0),
        Net::Local => Endpoint::builder(presets::Minimal)
            .clear_ip_transports()
            .bind_addr(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .map_err(|e| e.to_string())?,
    }
    .transport_config(transport);
    if accept {
        b = b.alpns(vec![ALPN.to_vec()]);
    }
    b.bind().await.map_err(|e| e.to_string())
}

/// The loopback address a local-mode endpoint can be dialed at.
pub(crate) fn local_addr(ep: &Endpoint) -> Option<SocketAddr> {
    ep.bound_sockets()
        .into_iter()
        .find(|a| a.is_ipv4())
        .map(|a| SocketAddr::from((Ipv4Addr::LOCALHOST, a.port())))
}

/// Human wording for why the host connection ended (guest side).
pub(crate) fn guest_close_reason(e: &ConnectionError) -> String {
    match e {
        ConnectionError::ApplicationClosed(c) => {
            match u32::try_from(c.error_code.into_inner()).unwrap_or(u32::MAX) {
                close::HOST_ENDED => "The host ended the session",
                close::KICKED => "The host removed you from the session",
                close::FULL => "The session is full",
                close::TOO_SLOW => "Your connection couldn't keep up with the host",
                close::PROTOCOL => "The host is using a different version of Ember",
                close::LINK_LOST => "Lost connection",
                _ => "The host ended the session",
            }
        }
        ConnectionError::LocallyClosed => "Disconnected",
        ConnectionError::VersionMismatch => "The host is using a different version of Ember",
        _ => "Lost connection",
    }
    .to_string()
}

/// Whether a connection that ended like this is worth dialing again: the link dropped (time
/// out, reset, network error), not the host ending the session, removing you or refusing you.
pub(crate) fn rejoinable(e: &ConnectionError) -> bool {
    match e {
        ConnectionError::ApplicationClosed(c) => {
            u32::try_from(c.error_code.into_inner()) == Ok(close::LINK_LOST)
        }
        ConnectionError::TimedOut
        | ConnectionError::Reset
        | ConnectionError::TransportError(_)
        | ConnectionError::CidsExhausted => true,
        _ => false,
    }
}

/// Owns the session's runtime and its main task; stopping it signals the task,
/// gives it a moment to close connections gracefully, then drops the runtime.
pub(crate) struct Session {
    rt: Mutex<Option<tokio::runtime::Runtime>>,
    stop_tx: watch::Sender<bool>,
    main: Mutex<Option<JoinHandle<()>>>,
}

impl Session {
    pub(crate) fn new() -> std::io::Result<(Self, watch::Receiver<bool>)> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("together")
            .enable_all()
            .build()?;
        let (stop_tx, stop_rx) = watch::channel(false);
        Ok((
            Self {
                rt: Mutex::new(Some(rt)),
                stop_tx,
                main: Mutex::new(None),
            },
            stop_rx,
        ))
    }

    pub(crate) fn handle(&self) -> Option<tokio::runtime::Handle> {
        self.rt.lock().ok()?.as_ref().map(|r| r.handle().clone())
    }

    pub(crate) fn block_on<F: std::future::Future>(&self, f: F) -> Option<F::Output> {
        let guard = self.rt.lock().ok()?;
        let rt = guard.as_ref()?;
        Some(rt.block_on(f))
    }

    pub(crate) fn set_main(&self, h: JoinHandle<()>) {
        if let Ok(mut m) = self.main.lock() {
            *m = Some(h);
        }
    }

    pub(crate) fn stop(&self) {
        let _ = self.stop_tx.send(true);
        let rt = self.rt.lock().ok().and_then(|mut g| g.take());
        let main = self.main.lock().ok().and_then(|mut g| g.take());
        let Some(rt) = rt else { return };
        if tokio::runtime::Handle::try_current().is_ok() {
            // Can't block inside another runtime; let the tasks die with it.
            rt.shutdown_background();
            return;
        }
        if let Some(main) = main {
            rt.block_on(async {
                let _ = tokio::time::timeout(Duration::from_millis(1500), main).await;
            });
        }
        rt.shutdown_timeout(Duration::from_millis(250));
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}
