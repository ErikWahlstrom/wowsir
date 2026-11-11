//! Minimal Chrome DevTools Protocol client over `--remote-debugging-pipe`.
//!
//! No websocket, no port discovery, no generated protocol bindings: just
//! NUL-delimited JSON over two pipes, request/response correlation, and
//! per-session event routing.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::pipe;
use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Debug)]
pub struct Event {
    pub method: String,
    pub params: Value,
}

type Pending = Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>;

pub struct Cdp {
    next_id: AtomicU64,
    pending: Arc<Pending>,
    out: mpsc::UnboundedSender<Vec<u8>>,
    /// sessionId ("" = browser level) -> event sink
    routes: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<Event>>>>,
}

impl Cdp {
    pub fn new(mut rx: pipe::Receiver, mut tx: pipe::Sender) -> Arc<Self> {
        let pending: Arc<Pending> = Arc::new(Mutex::new(HashMap::new()));
        let routes: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<Event>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (out, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        // writer
        tokio::spawn(async move {
            while let Some(mut msg) = out_rx.recv().await {
                msg.push(0);
                if tx.write_all(&msg).await.is_err() {
                    break;
                }
            }
        });

        // reader
        let p2 = pending.clone();
        let r2 = routes.clone();
        tokio::spawn(async move {
            let mut buf: Vec<u8> = Vec::with_capacity(1 << 20);
            let mut chunk = vec![0u8; 1 << 16];
            loop {
                let n = match rx.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&chunk[..n]);
                let mut start = 0;
                while let Some(pos) = buf[start..].iter().position(|&b| b == 0) {
                    let frame = &buf[start..start + pos];
                    start += pos + 1;
                    let Ok(v) = serde_json::from_slice::<Value>(frame) else {
                        continue;
                    };
                    dispatch(v, &p2, &r2);
                }
                buf.drain(..start);
            }
            // browser gone: fail everything pending
            for (_, tx) in p2.lock().unwrap().drain() {
                let _ = tx.send(Err("browser connection closed".into()));
            }
            r2.lock().unwrap().clear();
        });

        Arc::new(Cdp {
            next_id: AtomicU64::new(1),
            pending,
            out,
            routes,
        })
    }

    /// Route events for `session` ("" for browser-level) to a new receiver.
    pub fn subscribe(&self, session: &str) -> mpsc::UnboundedReceiver<Event> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.routes.lock().unwrap().insert(session.to_string(), tx);
        rx
    }

    pub fn unsubscribe(&self, session: &str) {
        self.routes.lock().unwrap().remove(session);
    }

    pub async fn call(&self, session: Option<&str>, method: &str, params: Value) -> Result<Value> {
        self.call_timeout(session, method, params, Duration::from_secs(30))
            .await
    }

    pub async fn call_timeout(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        if self.out.send(serde_json::to_vec(&msg)?).is_err() {
            bail!("browser connection closed");
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(anyhow!("{method}: {e}")),
            Ok(Err(_)) => Err(anyhow!("{method}: browser connection closed")),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(anyhow!(
                    "{method}: timed out after {}ms",
                    timeout.as_millis()
                ))
            }
        }
    }
}

fn dispatch(
    v: Value,
    pending: &Pending,
    routes: &Mutex<HashMap<String, mpsc::UnboundedSender<Event>>>,
) {
    if let Some(id) = v.get("id").and_then(|i| i.as_u64()) {
        if let Some(tx) = pending.lock().unwrap().remove(&id) {
            let r = match v.get("error") {
                Some(e) => Err(e
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error")
                    .to_string()),
                None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = tx.send(r);
        }
        return;
    }
    let Some(method) = v.get("method").and_then(|m| m.as_str()) else {
        return;
    };
    let session = v.get("sessionId").and_then(|s| s.as_str()).unwrap_or("");
    let routes = routes.lock().unwrap();
    if let Some(tx) = routes.get(session) {
        let _ = tx.send(Event {
            method: method.to_string(),
            params: v.get("params").cloned().unwrap_or(Value::Null),
        });
    }
}
