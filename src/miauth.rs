use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MiAuthSession {
    pub session_id: String,
    pub url: String,
    pub host: String,
}

#[derive(Debug, Deserialize)]
pub struct MiAuthCheckResponse {
    pub token: String,
    pub user: serde_json::Value,
}

impl MiAuthSession {
    /// 新しいMiAuthセッションを作成
    pub fn new(host: &str, app_name: &str, description: Option<&str>, permissions: &[&str]) -> Self {
        use rand::Rng;
        
        // セッションIDを生成（ランダムな16文字の英数字）
        let charset = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        let mut rng = rand::rng();
        let session_id: String = (0..16)
            .map(|_| {
                let idx = rng.random_range(0..charset.len());
                charset[idx] as char
            })
            .collect();
        
        // MiAuth URLを構築
        let mut url = format!(
            "https://{}/miauth/{}?name={}",
            host,
            session_id,
            urlencoding::encode(app_name)
        );
        
        if let Some(desc) = description {
            url.push_str(&format!("&description={}", urlencoding::encode(desc)));
        }
        
        if !permissions.is_empty() {
            url.push_str(&format!("&permission={}", permissions.join(",")));
        }
        
        Self {
            session_id,
            url,
            host: host.to_string(),
        }
    }
    
    /// 認証が完了したかチェックし、トークンとユーザー情報を取得
    pub async fn check(&self) -> Result<(String, Option<String>), Box<dyn std::error::Error>> {
        let check_url = format!(
            "https://{}/api/miauth/{}/check",
            self.host,
            self.session_id
        );
        
        let client = reqwest::Client::new();
        let response = client.post(&check_url)
            .header("Content-Type", "application/json")
            .body("{}")
            .send()
            .await?;
        
        if !response.status().is_success() {
            return Err(format!("認証が完了していません (HTTP {})", response.status()).into());
        }
        
        let auth_response: MiAuthCheckResponse = response.json().await?;
        
        // ユーザー名を取得
        let username = auth_response.user.get("username")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        
        Ok((auth_response.token, username))
    }
}
