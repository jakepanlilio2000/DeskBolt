use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};
use std::time::Duration;

use deskbolt_proto::TunnelMsg;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

const TUNNEL_CHUNK_SIZE: usize = 32 * 1024;
const TUNNEL_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

struct ActiveTunnel {
    write_tx: mpsc::Sender<Vec<u8>>,
    tasks: Vec<JoinHandle<()>>,
}

struct PendingReverse {
    stream: TcpStream,
}

/// Manages per-session Channel `0x07` encrypted TCP port forwarding (local & reverse).
pub struct TunnelSessionHandler {
    tx: mpsc::Sender<TunnelMsg>,
    tunnels: Arc<Mutex<HashMap<u32, ActiveTunnel>>>,
    reverse_listeners: HashMap<u32, JoinHandle<()>>,
    pending_reverse: Arc<Mutex<HashMap<u32, PendingReverse>>>,
    next_reverse_tunnel_id: Arc<AtomicU32>,
}

impl TunnelSessionHandler {
    pub fn new(tx: mpsc::Sender<TunnelMsg>) -> Self {
        Self {
            tx,
            tunnels: Arc::new(Mutex::new(HashMap::new())),
            reverse_listeners: HashMap::new(),
            pending_reverse: Arc::new(Mutex::new(HashMap::new())),
            next_reverse_tunnel_id: Arc::new(AtomicU32::new(0x8000_0001)),
        }
    }

    pub async fn handle_message(&mut self, msg: TunnelMsg) {
        match msg {
            TunnelMsg::TunnelOpen {
                tunnel_id,
                target_host,
                target_port,
            } => {
                self.open_forward_tunnel(tunnel_id, target_host, target_port)
                    .await;
            }
            TunnelMsg::TunnelData { tunnel_id, bytes } => {
                let sender = {
                    let guard = self.tunnels.lock().await;
                    guard.get(&tunnel_id).map(|t| t.write_tx.clone())
                };
                if let Some(write_tx) = sender
                    && write_tx.send(bytes).await.is_err()
                {
                    self.close_tunnel(tunnel_id, "tunnel write channel closed")
                        .await;
                }
            }
            TunnelMsg::TunnelClose { tunnel_id, .. } => {
                self.remove_tunnel(tunnel_id).await;
            }
            TunnelMsg::ReverseListenReq {
                listener_id,
                bind_port,
                ..
            } => {
                self.start_reverse_listener(listener_id, bind_port).await;
            }
            TunnelMsg::ReverseUnlisten { listener_id } => {
                if let Some(handle) = self.reverse_listeners.remove(&listener_id) {
                    handle.abort();
                }
            }
            TunnelMsg::TunnelOpenAck {
                tunnel_id,
                ok,
                error: _,
            } => {
                let pending = {
                    let mut guard = self.pending_reverse.lock().await;
                    guard.remove(&tunnel_id)
                };
                if let Some(p) = pending
                    && ok
                {
                    self.attach_stream(tunnel_id, p.stream).await;
                }
            }
            _ => {}
        }
    }

    async fn open_forward_tunnel(&self, tunnel_id: u32, target_host: String, target_port: u16) {
        let host = target_host.trim().to_string();
        if host.is_empty() || target_port == 0 {
            let _ = self
                .tx
                .send(TunnelMsg::TunnelOpenAck {
                    tunnel_id,
                    ok: false,
                    error: Some("invalid tunnel target host or port".into()),
                })
                .await;
            return;
        }

        let tx = self.tx.clone();
        let tunnels = self.tunnels.clone();
        tokio::spawn(async move {
            match tokio::time::timeout(
                TUNNEL_CONNECT_TIMEOUT,
                TcpStream::connect((host.as_str(), target_port)),
            )
            .await
            {
                Ok(Ok(stream)) => {
                    let _ = stream.set_nodelay(true);
                    attach_stream_shared(&tunnels, &tx, tunnel_id, stream).await;
                    let _ = tx
                        .send(TunnelMsg::TunnelOpenAck {
                            tunnel_id,
                            ok: true,
                            error: None,
                        })
                        .await;
                }
                Ok(Err(e)) => {
                    let _ = tx
                        .send(TunnelMsg::TunnelOpenAck {
                            tunnel_id,
                            ok: false,
                            error: Some(format!("connect {host}:{target_port} failed: {e}")),
                        })
                        .await;
                }
                Err(_) => {
                    let _ = tx
                        .send(TunnelMsg::TunnelOpenAck {
                            tunnel_id,
                            ok: false,
                            error: Some(format!("connect {host}:{target_port} timed out")),
                        })
                        .await;
                }
            }
        });
    }

    async fn start_reverse_listener(&mut self, listener_id: u32, bind_port: u16) {
        if let Some(old) = self.reverse_listeners.remove(&listener_id) {
            old.abort();
        }
        match TcpListener::bind(("127.0.0.1", bind_port)).await {
            Ok(listener) => {
                let bound_port = listener.local_addr().map(|a| a.port()).unwrap_or(bind_port);
                let _ = self
                    .tx
                    .send(TunnelMsg::ReverseListenAck {
                        listener_id,
                        bound_port,
                        ok: true,
                        error: None,
                    })
                    .await;

                let tx = self.tx.clone();
                let pending = self.pending_reverse.clone();
                let id_gen = self.next_reverse_tunnel_id.clone();
                let handle = tokio::spawn(async move {
                    while let Ok((stream, _)) = listener.accept().await {
                        let _ = stream.set_nodelay(true);
                        let tunnel_id = id_gen.fetch_add(1, Ordering::Relaxed);
                        {
                            let mut guard = pending.lock().await;
                            guard.insert(tunnel_id, PendingReverse { stream });
                        }
                        if tx
                            .send(TunnelMsg::ReverseIncoming {
                                tunnel_id,
                                listener_id,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                });
                self.reverse_listeners.insert(listener_id, handle);
            }
            Err(e) => {
                let _ = self
                    .tx
                    .send(TunnelMsg::ReverseListenAck {
                        listener_id,
                        bound_port: 0,
                        ok: false,
                        error: Some(format!("bind 127.0.0.1:{bind_port} failed: {e}")),
                    })
                    .await;
            }
        }
    }

    async fn attach_stream(&self, tunnel_id: u32, stream: TcpStream) {
        attach_stream_shared(&self.tunnels, &self.tx, tunnel_id, stream).await;
    }

    async fn remove_tunnel(&self, tunnel_id: u32) {
        if let Some(t) = self.tunnels.lock().await.remove(&tunnel_id) {
            for h in t.tasks {
                h.abort();
            }
        }
    }

    async fn close_tunnel(&self, tunnel_id: u32, reason: &str) {
        self.remove_tunnel(tunnel_id).await;
        let _ = self
            .tx
            .send(TunnelMsg::TunnelClose {
                tunnel_id,
                reason: reason.to_string(),
            })
            .await;
    }

    pub async fn cleanup_all(&mut self) {
        for (_, h) in self.reverse_listeners.drain() {
            h.abort();
        }
        self.pending_reverse.lock().await.clear();
        let mut guard = self.tunnels.lock().await;
        for (_, t) in guard.drain() {
            for h in t.tasks {
                h.abort();
            }
        }
    }
}

async fn attach_stream_shared(
    tunnels: &Arc<Mutex<HashMap<u32, ActiveTunnel>>>,
    tx: &mpsc::Sender<TunnelMsg>,
    tunnel_id: u32,
    stream: TcpStream,
) {
    let (mut rd, mut wr) = stream.into_split();
    let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(64);

    let tx_rd = tx.clone();
    let tunnels_rd = tunnels.clone();
    let rd_task = tokio::spawn(async move {
        let mut buf = vec![0u8; TUNNEL_CHUNK_SIZE];
        let reason = loop {
            match rd.read(&mut buf).await {
                Ok(0) => break "remote EOF".to_string(),
                Ok(n) => {
                    if tx_rd
                        .send(TunnelMsg::TunnelData {
                            tunnel_id,
                            bytes: buf[..n].to_vec(),
                        })
                        .await
                        .is_err()
                    {
                        break "session closed".to_string();
                    }
                }
                Err(e) => break format!("read error: {e}"),
            }
        };
        if let Some(t) = tunnels_rd.lock().await.remove(&tunnel_id) {
            for h in t.tasks {
                h.abort();
            }
        }
        let _ = tx_rd
            .send(TunnelMsg::TunnelClose { tunnel_id, reason })
            .await;
    });

    let wr_task = tokio::spawn(async move {
        while let Some(bytes) = write_rx.recv().await {
            if wr.write_all(&bytes).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });

    if let Some(old) = tunnels.lock().await.insert(
        tunnel_id,
        ActiveTunnel {
            write_tx,
            tasks: vec![rd_task, wr_task],
        },
    ) {
        for h in old.tasks {
            h.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tcp_port_forward_tunnel_echo_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let echo_task = tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 64];
                if let Ok(n) = sock.read(&mut buf).await {
                    let _ = sock.write_all(&buf[..n]).await;
                }
            }
        });

        let (tx, mut rx) = mpsc::channel(32);
        let mut handler = TunnelSessionHandler::new(tx);
        handler
            .handle_message(TunnelMsg::TunnelOpen {
                tunnel_id: 7,
                target_host: addr.ip().to_string(),
                target_port: addr.port(),
            })
            .await;

        let ack = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            ack,
            TunnelMsg::TunnelOpenAck {
                tunnel_id: 7,
                ok: true,
                ..
            }
        ));

        handler
            .handle_message(TunnelMsg::TunnelData {
                tunnel_id: 7,
                bytes: b"tunnel-hello".to_vec(),
            })
            .await;

        let msg = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            msg,
            TunnelMsg::TunnelData { tunnel_id: 7, bytes } if bytes == b"tunnel-hello"
        ));

        handler
            .handle_message(TunnelMsg::TunnelClose {
                tunnel_id: 7,
                reason: "done".into(),
            })
            .await;
        handler.cleanup_all().await;
        let _ = echo_task.await;
    }
}
