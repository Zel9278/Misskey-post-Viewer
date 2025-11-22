use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
    pub token: Option<String>,
    #[serde(default)]
    pub timeline: TimelineType,
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
        
        // アカウントがなければデフォルトを追加
        if config.accounts.is_empty() {
            config.accounts.push(Account {
                name: "Default Account".to_string(),
                host: "misskey.io".to_string(),
                token: None,
                timeline: TimelineType::default(),
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
        content.push_str("[[accounts]]\n");
        
        for account in &self.accounts {
            content.push_str(&format!("name = \"{}\"\n", account.name));
            content.push_str(&format!("host = \"{}\"\n", account.host));
            if let Some(token) = &account.token {
                content.push_str(&format!("token = \"{}\"\n", token));
            }
            // タイムライン設定を保存
            let timeline_str = match account.timeline {
                TimelineType::Hybrid => "hybrid",
                TimelineType::Local => "local",
                TimelineType::Home => "home",
                TimelineType::Global => "global",
            };
            content.push_str(&format!("timeline = \"{}\"\n", timeline_str));
            content.push_str("\n[[accounts]]\n");
        }
        
        // 最後の[[accounts]]を削除
        if content.ends_with("\n[[accounts]]\n") {
            content.truncate(content.len() - 14);
        }
        
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                let config_path = exe_dir.join("config.toml");
                let mut file = std::fs::File::create(config_path)?;
                file.write_all(content.as_bytes())?;
                return Ok(());
            }
        }
        
        let current_dir_config = PathBuf::from("config.toml");
        let mut file = std::fs::File::create(current_dir_config)?;
        file.write_all(content.as_bytes())?;
        Ok(())
    }
}
