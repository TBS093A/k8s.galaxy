//! Long-running streams the UI follows: pod logs (`kubectl logs -f`) and port-forwards
//! (`kubectl port-forward`, through the API server's WebSocket). Each runs in its own task;
//! dropping the task's handle stops it.

use std::sync::mpsc::Sender;
use std::time::Duration;

use futures::{AsyncBufReadExt, StreamExt};
use k8s_openapi::api::core::v1::Pod;
use kube::api::LogParams;
use kube::{Api, Client};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

use crate::source::Msg;

/// Lines kept in a followed log; older ones are dropped.
pub const LOG_CAP: usize = 20_000;

#[derive(Clone, Debug)]
pub struct LogReq {
    /// Lines from an older request (another container, the previous instance) are ignored.
    pub id: u64,
    pub namespace: String,
    pub pod: String,
    pub container: Option<String>,
    /// The log of the previous (crashed) container instance instead of following the current one.
    pub previous: bool,
}

#[derive(Clone, Debug)]
pub struct Forward {
    pub id: u64,
    pub namespace: String,
    pub pod: String,
    /// 0 = any free port.
    pub local: u16,
    pub remote: u16,
}

/// What a port-forward is doing, for the forwards list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForwardState {
    /// Bound local port once listening.
    pub local: Option<u16>,
    pub active: usize,
    pub total: usize,
    /// Fatal (bind failed) or the last connection's error.
    pub error: Option<String>,
}

/// Log text as the terminal can show it: tabs expanded, escape sequences and other control characters dropped.
pub fn clean(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\t' => out.push_str("    "),
            // CSI sequences (colors): ESC [ … final byte.
            '\x1b' => {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
            }
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

pub async fn follow_logs(client: Client, req: LogReq, tx: Sender<Msg>) {
    let id = req.id;
    let lp = LogParams {
        follow: !req.previous,
        tail_lines: Some(1000),
        container: req.container.clone(),
        previous: req.previous,
        ..Default::default()
    };
    let api = Api::<Pod>::namespaced(client, &req.namespace);
    let reader = match api.log_stream(&req.pod, &lp).await {
        Ok(r) => Box::pin(r),
        Err(e) => {
            let _ = tx.send(Msg::LogEnd { id, reason: format!("error: {e}") });
            return;
        }
    };
    let mut chunks = reader.lines().ready_chunks(512);
    while let Some(batch) = chunks.next().await {
        let mut lines = Vec::with_capacity(batch.len());
        let mut err = None;
        for l in batch {
            match l {
                Ok(l) => lines.push(clean(&l)),
                Err(e) => {
                    err = Some(e.to_string());
                    break;
                }
            }
        }
        if tx.send(Msg::LogLines { id, lines }).is_err() {
            return;
        }
        if let Some(e) = err {
            let _ = tx.send(Msg::LogEnd { id, reason: format!("error: {e}") });
            return;
        }
    }
    let reason = if req.previous { "end of the previous instance's log" } else { "stream closed — the container stopped" };
    let _ = tx.send(Msg::LogEnd { id, reason: reason.into() });
}

/// Demo: the recent log, then a new line every second or so.
pub async fn demo_logs(req: LogReq, tx: Sender<Msg>) {
    let id = req.id;
    let start: Vec<String> = (0..120).map(|i| crate::demo::log_line(&req.pod, i)).collect();
    if tx.send(Msg::LogLines { id, lines: start }).is_err() {
        return;
    }
    if req.previous {
        let _ = tx.send(Msg::LogEnd { id, reason: "end of the previous instance's log".into() });
        return;
    }
    for i in 120.. {
        let wait = 300 + crate::fx::hash(&format!("{}{i}", req.pod)) % 1200;
        tokio::time::sleep(Duration::from_millis(wait)).await;
        if tx.send(Msg::LogLines { id, lines: vec![crate::demo::log_line(&req.pod, i)] }).is_err() {
            return;
        }
    }
}

/// Listen on 127.0.0.1:`local` and tunnel every connection to the pod's port. `client` None is the
/// demo: connections get a canned HTTP answer.
pub async fn forward(client: Option<Client>, f: Forward, tx: Sender<Msg>) {
    let id = f.id;
    let mut state = ForwardState::default();
    let report = |s: &ForwardState| tx.send(Msg::Forward { id, state: s.clone() }).is_ok();
    let listener = match TcpListener::bind(("127.0.0.1", f.local)).await {
        Ok(l) => l,
        Err(e) => {
            state.error = Some(format!("127.0.0.1:{}: {e}", f.local));
            report(&state);
            return;
        }
    };
    state.local = listener.local_addr().ok().map(|a| a.port());
    if !report(&state) {
        return;
    }
    // Dropping the set (when this task is aborted) closes every open connection.
    let mut conns: JoinSet<Result<(), String>> = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((sock, _)) = accepted else { continue };
                state.active += 1;
                state.total += 1;
                let (client, f) = (client.clone(), f.clone());
                conns.spawn(async move {
                    match client {
                        Some(c) => tunnel(c, &f, sock).await,
                        None => demo_answer(&f, sock).await,
                    }
                });
            }
            Some(done) = conns.join_next() => {
                state.active = state.active.saturating_sub(1);
                if let Ok(Err(e)) = done {
                    state.error = Some(e);
                }
            }
        }
        if !report(&state) {
            return;
        }
    }
}

async fn tunnel(client: Client, f: &Forward, mut sock: TcpStream) -> Result<(), String> {
    let api = Api::<Pod>::namespaced(client, &f.namespace);
    let mut pf = api.portforward(&f.pod, &[f.remote]).await.map_err(|e| format!("{}/{}: {e}", f.namespace, f.pod))?;
    let mut upstream = pf.take_stream(f.remote).ok_or("no stream for the port")?;
    let res = tokio::io::copy_bidirectional(&mut sock, &mut upstream).await;
    drop(upstream);
    let joined = pf.join().await;
    res.map_err(|e| format!("port {}: {e}", f.remote))?;
    joined.map_err(|e| format!("port {}: {e}", f.remote))
}

async fn demo_answer(f: &Forward, mut sock: TcpStream) -> Result<(), String> {
    let mut buf = [0u8; 2048];
    let _ = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut buf)).await;
    let body = format!("k8s-galaxy demo: forwarded to {}/{} port {}\n", f.namespace, f.pod, f.remote);
    let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    sock.write_all(reply.as_bytes()).await.map_err(|e| e.to_string())
}

/// A shell in the pod through `kubectl exec -it` on the real terminal (the caller suspends the UI).
/// bash when the image has it, else sh.
pub fn exec(kubectl_args: &[String], req: &crate::app::ExecReq) -> Result<String, String> {
    use std::io::Write;
    let target = format!("{}/{}{}", req.namespace, req.pod, req.container.as_ref().map(|c| format!(" ({c})")).unwrap_or_default());
    println!("\x1b[1;36m◆ k8s-galaxy\x1b[0m shell in {target} — exit (or Ctrl-D) returns to the dashboard\n");
    let mut cmd = std::process::Command::new("kubectl");
    cmd.args(kubectl_args).args(["exec", "-it", "-n", &req.namespace, &req.pod]);
    if let Some(c) = &req.container {
        cmd.args(["-c", c]);
    }
    cmd.args(["--", "sh", "-c", "command -v bash >/dev/null 2>&1 && exec bash || exec sh"]);
    let status = match cmd.status() {
        Ok(s) => s,
        Err(e) => return Err(format!("kubectl exec: {e} (is kubectl installed?)")),
    };
    // Exit codes of the shell's last command come back too: only kubectl's own failures (no
    // shell in the image, no access) stop to show their message.
    if !status.success() && status.code() != Some(130) {
        print!("\n\x1b[33mkubectl exited with {status}\x1b[0m — Enter returns to the dashboard ");
        let _ = std::io::stdout().flush();
        let _ = std::io::stdin().read_line(&mut String::new());
    }
    Ok(format!("shell in {target} closed"))
}

/// `8080`, `9000:8080` (local:remote) or `:8080` (any free local port).
pub fn parse_ports(s: &str) -> Result<(u16, u16), String> {
    let port = |p: &str| p.trim().parse::<u16>().ok().filter(|p| *p > 0).ok_or_else(|| format!("not a port: {p:?}"));
    match s.trim().split_once(':') {
        None => port(s).map(|p| (p, p)),
        Some((l, r)) if l.trim().is_empty() => Ok((0, port(r)?)),
        Some((l, r)) => Ok((port(l)?, port(r)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports() {
        assert_eq!(parse_ports("8080"), Ok((8080, 8080)));
        assert_eq!(parse_ports(" 9000:80 "), Ok((9000, 80)));
        assert_eq!(parse_ports(":5432"), Ok((0, 5432)));
        assert!(parse_ports("0").is_err());
        assert!(parse_ports("http").is_err());
        assert!(parse_ports("1:99999").is_err());
    }

    #[test]
    fn cleans_terminal_codes() {
        assert_eq!(clean("\x1b[31mERROR\x1b[0m\tboom\r"), "ERROR    boom");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn demo_forward_answers() {
        let (tx, rx) = std::sync::mpsc::channel();
        let f = Forward { id: 1, namespace: "shop".into(), pod: "api-0".into(), local: 0, remote: 8080 };
        let task = tokio::spawn(forward(None, f, tx));
        let port = match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Msg::Forward { state, .. }) => state.local.unwrap(),
            _ => panic!("no forward state"),
        };
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(b"GET / HTTP/1.0\r\n\r\n").await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        assert!(out.contains("shop/api-0 port 8080"), "{out}");
        task.abort();
    }
}
