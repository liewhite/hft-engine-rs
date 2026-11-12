//! run_main 的告警链路端到端：启动试发 → 主流程 panic → 退出原因外送 → 退出前发完。
//! 独立测试进程（run_main 会装全局 subscriber 与 panic hook），告警发到本地 HTTP 桩。

use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// 极简 webhook 桩：每个连接读一个请求，记下 body，回 200
async fn spawn_stub() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let b = bodies.clone();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let b = b.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = sock.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    let Some((head, body)) = text.split_once("\r\n\r\n") else { continue };
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                        .unwrap_or(0);
                    if body.len() < len {
                        continue;
                    }
                    b.lock().unwrap().push(body.to_string());
                    let _ = sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").await;
                    buf.clear();
                }
            });
        }
    });
    (url, bodies)
}

#[tokio::test]
async fn panic_in_main_is_alerted_and_flushed() {
    let (url, bodies) = spawn_stub().await;
    std::env::set_var("ALERT_WEBHOOK_URL", &url);
    std::env::set_var("NO_PROXY", "127.0.0.1");
    std::env::set_var("no_proxy", "127.0.0.1");
    std::env::remove_var("SLACK_BOT_TOKEN");
    std::env::remove_var("SLACK_ALERT_CHANNEL");

    let result = hft_engine_rs::engine::run_main(|| async {
        if true {
            panic!("模拟主流程崩溃");
        }
        Ok(())
    })
    .await;

    assert!(result.is_err(), "panic 要转成错误返回");
    let got = bodies.lock().unwrap().clone();
    assert!(got[0].contains("启动自检"), "{got:?}");
    assert!(got.iter().any(|b| b.contains("模拟主流程崩溃")), "panic 内容要外送: {got:?}");
    assert!(got.iter().any(|b| b.contains("进程因错误退出")), "退出原因要在返回前发完: {got:?}");
}
