use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

// トークンを難読化するためのシンプルなXOR暗号化 + Base64
const OBFUSCATION_KEY: &[u8] = b"MisskeyPostViewer2024";

fn obfuscate_token(token: &str) -> String {
    let key_bytes = OBFUSCATION_KEY;
    let obfuscated: Vec<u8> = token
        .as_bytes()
        .iter()
        .enumerate()
        .map(|(i, &b)| b ^ key_bytes[i % key_bytes.len()])
        .collect();
    BASE64.encode(&obfuscated)
}

fn deobfuscate_token(encoded: &str) -> Option<String> {
    let decoded = BASE64.decode(encoded).ok()?;
    let key_bytes = OBFUSCATION_KEY;
    let original: Vec<u8> = decoded
        .iter()
        .enumerate()
        .map(|(i, &b)| b ^ key_bytes[i % key_bytes.len()])
        .collect();
    String::from_utf8(original).ok()
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub enum TimelineType {
    #[serde(rename = "hybrid")]
    Hybrid,
    #[serde(rename = "local")]
    Local,
    #[serde(rename = "home")]
    Home,
    #[serde(rename = "global")]
    Global,
}

impl Default for TimelineType {
    fn default() -> Self {
        TimelineType::Hybrid
    }
}

impl TimelineType {
    pub fn to_channel_name(&self) -> &str {
        match self {
            TimelineType::Hybrid => "hybridTimeline",
            TimelineType::Local => "localTimeline",
            TimelineType::Home => "homeTimeline",
            TimelineType::Global => "globalTimeline",
        }
    }
    
    pub fn display_name(&self) -> &str {
        match self {
            TimelineType::Hybrid => "ハイブリッド",
            TimelineType::Local => "ローカル",
            TimelineType::Home => "ホーム",
            TimelineType::Global => "グローバル",
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Account {
    pub name: String,
    pub host: String,
    #[serde(skip)]
    pub token: Option<String>,  // 実際に使用されるトークン（メモリ上のみ）
    #[serde(default, rename = "token")]
    token_raw: Option<String>,  // 後方互換性のため：旧形式の生トークン
    #[serde(default)]
    token_obfuscated: Option<String>,  // 難読化されたトークン（ファイル保存用）
    #[serde(default)]
    pub timeline: TimelineType,
    #[serde(default)]
    pub enabled: bool, // アカウントの有効/無効
    #[serde(default = "default_text_color")]
    pub text_color: [u8; 3], // RGB色 (デフォルト: 白 [255, 255, 255])
}

impl Account {
    /// 新しいアカウントを作成
    pub fn new(name: String, host: String, token: Option<String>, timeline: TimelineType, enabled: bool, text_color: [u8; 3]) -> Self {
        Self {
            name,
            host,
            token,
            token_raw: None,
            token_obfuscated: None,
            timeline,
            enabled,
            text_color,
        }
    }
    
    /// デシリアライズ後にトークンを復元する
    pub fn restore_token(&mut self) {
        // 難読化トークンがあれば優先的に使用
        if let Some(ref obfuscated) = self.token_obfuscated {
            self.token = deobfuscate_token(obfuscated);
        } else if self.token_raw.is_some() {
            // 旧形式の生トークンがあればそれを使用
            self.token = self.token_raw.take();
        }
    }
    
    /// トークンを難読化して保存用に準備
    pub fn prepare_for_save(&mut self) {
        if let Some(ref token) = self.token {
            self.token_obfuscated = Some(obfuscate_token(token));
        } else {
            self.token_obfuscated = None;
        }
        self.token_raw = None; // 生トークンはクリア
    }
}

fn default_text_color() -> [u8; 3] {
    [255, 255, 255]
}

impl Default for Account {
    fn default() -> Self {
        Self {
            name: String::new(),
            host: String::new(),
            token: None,
            token_raw: None,
            token_obfuscated: None,
            timeline: TimelineType::default(),
            enabled: true,
            text_color: default_text_color(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AppConfig {
    #[serde(default)]
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub active_account_index: usize,
    #[serde(default)]
    pub debug: bool,
    #[serde(default)]
    pub fallback_font: Option<String>,
}

impl AppConfig {
    pub fn new() -> Result<Self, config::ConfigError> {
        let mut builder = config::Config::builder();

        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                let exe_config_path = exe_dir.join("config.toml");
                if exe_config_path.exists() {
                    builder = builder.add_source(config::File::from(exe_config_path));
                }
            }
        }

        let current_dir_config = PathBuf::from("config.toml");
        if current_dir_config.exists() {
            builder = builder.add_source(config::File::from(current_dir_config));
        }

        builder = builder.add_source(config::Environment::with_prefix("MISSKEY"));

        let settings = builder.build()?;
        let mut config: AppConfig = settings.try_deserialize()?;
        
        // トークンを復元
        for account in &mut config.accounts {
            account.restore_token();
        }
        
        // アカウントがなければデフォルトを追加
        if config.accounts.is_empty() {
            config.accounts.push(Account {
                name: "Default Account".to_string(),
                host: "misskey.io".to_string(),
                token: None,
                token_raw: None,
                token_obfuscated: None,
                timeline: TimelineType::default(),
                enabled: true,
                text_color: default_text_color(),
            });
        }
        
        Ok(config)
    }
    
    pub fn get_active_account(&self) -> Option<&Account> {
        self.accounts.get(self.active_account_index)
    }
    
    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        use std::io::Write;
        
        let mut content = String::new();
        content.push_str("# Misskey Post Viewer Configuration\n\n");
        content.push_str(&format!("active_account_index = {}\n", self.active_account_index));
        content.push_str(&format!("debug = {}\n", self.debug));
        if let Some(font) = &self.fallback_font {
            content.push_str(&format!("fallback_font = \"{}\"\n", font));
        }
        content.push_str("\n");
        
        for account in &self.accounts {
            content.push_str("[[accounts]]\n");
            content.push_str(&format!("name = \"{}\"\n", account.name));
            content.push_str(&format!("host = \"{}\"\n", account.host));
            // トークンは難読化して保存
            if let Some(token) = &account.token {
                let obfuscated = obfuscate_token(token);
                content.push_str(&format!("token_obfuscated = \"{}\"\n", obfuscated));
            }
            // タイムライン設定を保存
            let timeline_str = match account.timeline {
                TimelineType::Hybrid => "hybrid",
                TimelineType::Local => "local",
                TimelineType::Home => "home",
                TimelineType::Global => "global",
            };
            content.push_str(&format!("timeline = \"{}\"\n", timeline_str));
            content.push_str(&format!("enabled = {}\n", account.enabled));
            content.push_str(&format!("text_color = [{}, {}, {}]\n", 
                account.text_color[0], account.text_color[1], account.text_color[2]));
            content.push_str("\n");
        }
        
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                let config_path = exe_dir.join("config.toml");
                println!("設定ファイルを保存: {:?}", config_path);
                println!("保存するアカウント数: {}", self.accounts.len());
                let mut file = std::fs::File::create(&config_path)?;
                file.write_all(content.as_bytes())?;
                println!("保存完了!");
                return Ok(());
            }
        }
        
        let current_dir_config = PathBuf::from("config.toml");
        println!("設定ファイルを保存: {:?}", current_dir_config);
        let mut file = std::fs::File::create(current_dir_config)?;
        file.write_all(content.as_bytes())?;
        Ok(())
    }
}
