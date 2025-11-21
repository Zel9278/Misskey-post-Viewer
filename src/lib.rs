use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use url::Url;

pub struct MisskeyClient {
    write: mpsc::UnboundedSender<Message>,
    read: futures::stream::SplitStream<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>>,
}

impl MisskeyClient {
    pub async fn connect(host: &str, token: Option<String>) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let protocol = "wss";
        let mut url_str = format!("{}://{}/streaming", protocol, host);
        
        if let Some(t) = &token {
            url_str.push_str(&format!("?i={}", t));
        }

        let url = Url::parse(&url_str)?;
        println!("Connecting to {}...", url);

        // タイムアウトを2秒に短縮
        let (ws_stream, _) = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            connect_async(url)
        ).await??;
        println!("Connected!");

        let (write_stream, read_stream) = ws_stream.split();

        // メッセージ送信用のチャネルを作成
        let (tx, mut rx) = mpsc::unbounded_channel();

        // 書き込みタスク
        let mut write_stream = write_stream;
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if let Err(_e) = write_stream.send(msg).await {
                    // 接続が閉じられた場合は静かに終了（エラーログを出さない）
                    break;
                }
            }
        });

        // ハートビートタスク (60秒ごとに 'h' を送信)
        let tx_heartbeat = tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                if tx_heartbeat.send(Message::Text("h".to_string())).is_err() {
                    break;
                }
            }
        });

        Ok(MisskeyClient {
            write: tx,
            read: read_stream,
        })
    }

    pub fn subscribe(&self, channel: &str, id: &str, params: serde_json::Value) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let connect_msg = json!({
            "type": "connect",
            "body": {
                "channel": channel,
                "id": id,
                "params": params
            }
        });

        self.write.send(Message::Text(connect_msg.to_string()))?;
        println!("Subscribed to {}", channel);
        Ok(())
    }

    pub async fn next_message(&mut self) -> Option<Result<Message, tokio_tungstenite::tungstenite::Error>> {
        self.read.next().await
    }
    
    pub fn close(self) {
        // MisskeyClientをドロップすることで、writeチャネルが閉じられ、
        // 書き込みタスクとハートビートタスクが自動的に終了する
        drop(self.write);
        println!("[CLOSE] WebSocket connection closed");
    }
}
