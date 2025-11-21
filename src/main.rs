#![windows_subsystem = "windows"]

use eframe::egui;
use misskey_post_viewer::MisskeyClient;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};


use serde::{Deserialize, Serialize};
use std::collections::{VecDeque, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::runtime::Runtime;
use crossbeam_channel::{unbounded, Receiver as CrossbeamReceiver};
use tokio_tungstenite::tungstenite::protocol::Message;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, SetWindowLongPtrW, SetForegroundWindow, PostMessageW, FindWindowW,
    GWL_EXSTYLE, WS_EX_LAYERED, WS_EX_TRANSPARENT, WM_USER,
};
use tray_icon::{TrayIconBuilder, menu::{Menu, MenuItem}};
use egui::{ColorImage, TextureHandle};

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
enum TimelineType {
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
    fn to_channel_name(&self) -> &str {
        match self {
            TimelineType::Hybrid => "hybridTimeline",
            TimelineType::Local => "localTimeline",
            TimelineType::Home => "homeTimeline",
            TimelineType::Global => "globalTimeline",
        }
    }
    
    fn display_name(&self) -> &str {
        match self {
            TimelineType::Hybrid => "ハイブリッド",
            TimelineType::Local => "ローカル",
            TimelineType::Home => "ホーム",
            TimelineType::Global => "グローバル",
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct Account {
    name: String,
    host: String,
    token: Option<String>,
    #[serde(default)]
    timeline: TimelineType,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct AppConfig {
    #[serde(default)]
    accounts: Vec<Account>,
    #[serde(default)]
    active_account_index: usize,
    #[serde(default)]
    debug: bool,
    #[serde(default)]
    fallback_font: Option<String>,
    // 互換性のために古い形式もサポート
    #[serde(skip_serializing)]
    host: Option<String>,
    #[serde(skip_serializing)]
    token: Option<String>,
}

impl AppConfig {
    fn new() -> Result<Self, config::ConfigError> {
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
        
        // 古い形式からの移行：hostとtokenがあればアカウントリストに追加
        if let Some(host) = config.host.take() {
            if config.accounts.is_empty() {
                config.accounts.push(Account {
                    name: format!("Account - {}", host),
                    host,
                    token: config.token.take(),
                    timeline: TimelineType::default(),
                });
            }
        }
        
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
    
    fn get_active_account(&self) -> Option<&Account> {
        self.accounts.get(self.active_account_index)
    }
    
    fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
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

#[derive(Clone)]
struct EmojiInfo {
    name: String,
    url: String,
}

struct AnimatedEmoji {
    frames: Vec<ColorImage>,
    frame_durations: Vec<u32>, // ミリ秒
    textures: Vec<TextureHandle>,
    current_frame: usize,
    elapsed_ms: u32,
}

struct Comment {
    text: String,
    x: f32,
    y: f32,
    speed: f32,
    name: String,
    username: String,
    user_host: Option<String>,
    renote_info: Option<(String, String, String, String)>, // (元投稿者のname, 元投稿者のusername, 元投稿者のhost, 元投稿テキスト)
    emojis: Vec<EmojiInfo>, // カスタム絵文字情報
}

enum TrayEvent {
    Settings,
    Quit,
}

struct MisskeyViewerApp {
    comments: VecDeque<Comment>,
    rx: std::sync::mpsc::Receiver<Comment>,
    tray_rx: CrossbeamReceiver<TrayEvent>,
    tray_event_flag: Arc<Mutex<bool>>,
    reconnect_tx: tokio::sync::mpsc::UnboundedSender<AppConfig>,
    _runtime: Runtime,
    window_configured: bool,
    show_settings: bool,
    config: AppConfig,
    is_connected: Arc<Mutex<bool>>,
    // アカウント編集用
    edit_account_name: String,
    edit_account_host: String,
    edit_account_token: String,
    selected_account_index: Option<usize>,
    // 絵文字キャッシュ（静止画）
    emoji_cache: HashMap<String, Option<TextureHandle>>,
    // アニメーション絵文字キャッシュ
    animated_emoji_cache: HashMap<String, AnimatedEmoji>,
    // 絵文字ダウンロード中フラグ
    emoji_downloading: HashMap<String, bool>,
    // 絵文字ダウンロード結果チャネル
    emoji_rx: std::sync::mpsc::Receiver<(String, Vec<u8>)>,
    emoji_tx: std::sync::mpsc::Sender<(String, Vec<u8>)>,
}

impl MisskeyViewerApp {
    fn new(
        cc: &eframe::CreationContext<'_>, 
        config: AppConfig,
        tray_rx: CrossbeamReceiver<TrayEvent>,
        tray_event_flag: Arc<Mutex<bool>>
    ) -> Self {
        // フォント設定 (日本語表示のため)
        let mut fonts = egui::FontDefinitions::default();
        
        // システムフォントを読み込む試み (Windows)
        let font_path = "C:\\Windows\\Fonts\\meiryo.ttc";
        if std::path::Path::new(font_path).exists() {
            if let Ok(font_data) = std::fs::read(font_path) {
                fonts.font_data.insert(
                    "my_font".to_owned(),
                    egui::FontData::from_owned(font_data).tweak(
                        egui::FontTweak {
                            scale: 1.0,
                            ..Default::default()
                        }
                    ).into(),
                );
                
                // Proportionalフォントの先頭に挿入
                fonts.families
                    .entry(egui::FontFamily::Proportional)
                    .or_default()
                    .insert(0, "my_font".to_owned());
                
                // Monospaceフォントの先頭に挿入
                fonts.families
                    .entry(egui::FontFamily::Monospace)
                    .or_default()
                    .insert(0, "my_font".to_owned());
            }
        }

        cc.egui_ctx.set_fonts(fonts);
        
        let (tx, rx) = std::sync::mpsc::channel();
        let (reconnect_tx, mut reconnect_rx) = tokio::sync::mpsc::unbounded_channel::<AppConfig>();
        let (emoji_tx, emoji_rx) = std::sync::mpsc::channel::<(String, Vec<u8>)>();
        let is_connected = Arc::new(Mutex::new(false));
        let is_connected_clone = is_connected.clone();
        let runtime = Runtime::new().expect("Failed to create Tokio runtime");

        // Misskeyクライアントを別スレッドで実行
        let mut current_config = config.clone();
        let debug_mode = current_config.debug;
        let mut is_manual_reconnect = false;
        let mut consecutive_failures = 0u32; // 連続失敗カウンタ
        runtime.spawn(async move {
            loop {
                // 再接続リクエストをチェック
                if let Ok(new_config) = reconnect_rx.try_recv() {
                    let reconnect_start = std::time::Instant::now();
                    println!("[MANUAL] Config update received at {:?}, reconnecting immediately...", reconnect_start.elapsed());
                    *is_connected_clone.lock().unwrap() = false;
                    current_config = new_config;
                    is_manual_reconnect = true;
                    consecutive_failures = 0;
                }
                
                let account = current_config.get_active_account().cloned();
                if let Some(account) = account {
                    let start_time = std::time::Instant::now();
                    println!("[{:?}] Connecting to Misskey ({}) ...", start_time.elapsed(), account.host);
                    match MisskeyClient::connect(&account.host, account.token.clone()).await {
                        Ok(mut client) => {
                            println!("[SUCCESS] WebSocket connected in {:?}!", start_time.elapsed());
                            *is_connected_clone.lock().unwrap() = true;
                            consecutive_failures = 0;
                            is_manual_reconnect = false;
                        
                        // アカウントのタイムライン設定を使用
                        let channel = account.timeline.to_channel_name();
                        let id = format!("{}-1", channel);

                        if let Err(e) = client.subscribe(channel, &id, serde_json::json!({})) {
                            eprintln!("[ERROR] Subscribe failed: {}", e);
                            consecutive_failures += 1;
                            continue;
                        }
                        println!("Subscribed to {} ({}).", channel, account.timeline.display_name());

                        loop {
                            tokio::select! {
                                // 再接続リクエストを即座に受信
                                Some(new_config) = reconnect_rx.recv() => {
                                    let close_start = std::time::Instant::now();
                                    println!("[MANUAL] Reconnection requested, closing current connection...");
                                    client.close();
                                    println!("[MANUAL] Connection closed in {:?}", close_start.elapsed());
                                    *is_connected_clone.lock().unwrap() = false;
                                    current_config = new_config;
                                    is_manual_reconnect = true;
                                    consecutive_failures = 0;
                                    break;
                                }
                                // WebSocketメッセージを受信
                                msg_result = client.next_message() => {
                            if let Some(msg_result) = msg_result {
                            match msg_result {
                                Ok(msg) => {
                                    // println!("Received: {:?}", msg); // デバッグ用: 全メッセージ表示
                                    if let Message::Text(text) = msg {
                                        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) {
                                            if let Some(body) = parsed.get("body") {
                                                if let Some(type_) = body.get("type") {
                                                    if type_ == "note" {
                                                        if let Some(note_body) = body.get("body") {
                                                            if debug_mode { println!("DEBUG: note_body keys: {:?}", note_body.as_object().map(|o| o.keys().collect::<Vec<_>>())); }
                                                            let user = note_body.get("user");
                                                            let name = user.and_then(|u| u.get("name")).and_then(|v| v.as_str()).unwrap_or("Unknown").to_string();
                                                            let username = user.and_then(|u| u.get("username")).and_then(|v| v.as_str()).unwrap_or("Unknown").to_string();
                                                            let user_host = user.and_then(|u| u.get("host")).and_then(|v| v.as_str()).map(|s| s.to_string());
                                                            
                                                            // 絵文字情報を抽出
                                                            let mut emojis = Vec::new();
                                                            let host = &account.host;
                                                            
                                                            if let Some(emojis_obj) = note_body.get("emojis") {
                                                                if debug_mode { println!("DEBUG: Found emojis field: {:?}", emojis_obj); }
                                                                if let Some(emoji_map) = emojis_obj.as_object() {
                                                                    for (emoji_name, emoji_url) in emoji_map {
                                                                        if let Some(url) = emoji_url.as_str() {
                                                                            if debug_mode { println!("DEBUG: Emoji {} -> {}", emoji_name, url); }
                                                                            emojis.push(EmojiInfo {
                                                                                name: emoji_name.clone(),
                                                                                url: url.to_string(),
                                                                            });
                                                                        }
                                                                    }
                                                                }
                                                            } else {
                                                                if debug_mode { println!("DEBUG: No emojis field in note_body"); }
                                                            }
                                                            
                                                            // テキストと名前から絵文字タグを探して、まだURLが取得できていないものをAPIで取得
                                                            let mut all_text = String::new();
                                                            if let Some(text) = note_body.get("text").and_then(|v| v.as_str()) {
                                                                all_text.push_str(text);
                                                            }
                                                            // 名前も追加
                                                            all_text.push(' ');
                                                            all_text.push_str(&name);
                                                            
                                                            let emoji_names: Vec<String> = all_text
                                                                .split(':')
                                                                .enumerate()
                                                                .filter_map(|(i, part)| {
                                                                    if i % 2 == 1 && !part.is_empty() {
                                                                        Some(part.to_string())
                                                                    } else {
                                                                        None
                                                                    }
                                                                })
                                                                .collect();
                                                            
                                                            for emoji_name in emoji_names {
                                                                // 既に取得済みかチェック
                                                                if !emojis.iter().any(|e| e.name == emoji_name) {
                                                                    // APIから取得を試みる
                                                                    if let Ok(response) = reqwest::blocking::get(format!("https://{}/api/emoji?name={}", host, emoji_name)) {
                                                                        if let Ok(emoji_data) = response.json::<serde_json::Value>() {
                                                                            if let Some(url) = emoji_data.get("url").and_then(|v| v.as_str()) {
                                                                                if debug_mode { println!("DEBUG: Fetched emoji from API {} -> {}", emoji_name, url); }
                                                                                emojis.push(EmojiInfo {
                                                                                    name: emoji_name.clone(),
                                                                                    url: url.to_string(),
                                                                                });
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                            
                                                            // リノートの場合は元の投稿情報とテキストを取得
                                                            let renote_info = if let Some(renote) = note_body.get("renote") {
                                                                let orig_user = renote.get("user");
                                                                let orig_name = orig_user.and_then(|u| u.get("name")).and_then(|v| v.as_str()).unwrap_or("Unknown").to_string();
                                                                let orig_username = orig_user.and_then(|u| u.get("username")).and_then(|v| v.as_str()).unwrap_or("Unknown").to_string();
                                                                let orig_host = orig_user.and_then(|u| u.get("host")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                                                                
                                                                // リノート元の絵文字も取得
                                                                if let Some(renote_emojis_obj) = renote.get("emojis") {
                                                                    if let Some(emoji_map) = renote_emojis_obj.as_object() {
                                                                        for (emoji_name, emoji_url) in emoji_map {
                                                                            if let Some(url) = emoji_url.as_str() {
                                                                                if !emojis.iter().any(|e| e.name == *emoji_name) {
                                                                                    if debug_mode { println!("DEBUG: Renote Emoji {} -> {}", emoji_name, url); }
                                                                                    emojis.push(EmojiInfo {
                                                                                        name: emoji_name.clone(),
                                                                                        url: url.to_string(),
                                                                                    });
                                                                                }
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                                
                                                                // リノート元のテキスト（CW優先）
                                                                let orig_text_raw = if let Some(cw) = renote.get("cw").and_then(|v| v.as_str()) {
                                                                    if !cw.is_empty() {
                                                                        format!("CW: {}", cw)
                                                                    } else {
                                                                        renote.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string()
                                                                    }
                                                                } else {
                                                                    renote.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string()
                                                                };
                                                                
                                                                // リノートのテキストも切り詰める
                                                                let orig_text = if orig_text_raw.chars().count() > 80 {
                                                                    format!("{}...", orig_text_raw.chars().take(80).collect::<String>())
                                                                } else {
                                                                    orig_text_raw
                                                                };
                                                                
                                                                Some((orig_name, orig_username, orig_host, orig_text))
                                                            } else {
                                                                None
                                                            };
                                                            
                                                            // CWがある場合はCWの内容を、ない場合は本文を表示
                                                            let text_content = if let Some((_, _, _, ref rn_text)) = renote_info {
                                                                // リノートの場合はリノート元のテキストを使用
                                                                rn_text.clone()
                                                            } else if let Some(cw) = note_body.get("cw").and_then(|v| v.as_str()) {
                                                                if !cw.is_empty() {
                                                                    format!("CW: {}", cw)
                                                                } else {
                                                                    note_body.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string()
                                                                }
                                                            } else {
                                                                note_body.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string()
                                                            };
                                                            
                                                            // テキストを一定の文字数で切り詰める（100文字まで）
                                                            let truncated_text = if text_content.chars().count() > 100 {
                                                                format!("{}...", text_content.chars().take(100).collect::<String>())
                                                            } else {
                                                                text_content.clone()
                                                            };
                                                            
                                                            if debug_mode { println!("New Note from @{}: {}", username, truncated_text); }

                                                            if !text_content.is_empty() || renote_info.is_some() {
                                                                // ランダムなY座標と速度を生成
                                                                use rand::Rng;
                                                                let mut rng = rand::rng();
                                                                let y = rng.random_range(50.0..800.0); // 画面の高さに応じて調整が必要だが一旦固定
                                                                let speed = rng.random_range(4.0..8.0); // 速度を上げる

                                                                let comment = Comment {
                                                                    text: truncated_text,
                                                                    x: 2000.0, // 初期位置（画面右外）
                                                                    y,
                                                                    speed,
                                                                    name,
                                                                    username,
                                                                    user_host,
                                                                    renote_info,
                                                                    emojis,
                                                                };
                                                                let _ = tx.send(comment);
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    eprintln!("WebSocket error: {}", e);
                                    break;
                                }
                            }
                            } else {
                                break;
                            }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[ERROR] Connection failed (attempt #{}): {}", consecutive_failures + 1, e);
                        consecutive_failures += 1;
                        
                        // 手動再接続で失敗が3回以上連続したら通常モードに切り替え
                        if is_manual_reconnect && consecutive_failures >= 3 {
                            eprintln!("[WARN] Manual reconnect failed 3 times, switching to normal retry mode");
                            is_manual_reconnect = false;
                        }
                    }
                    }
                }
                
                // 再接続前に待機
                if is_manual_reconnect && consecutive_failures < 3 {
                    // 手動再接続で失敗が3回未満なら即座にリトライ
                    println!("[MANUAL] Retrying immediately... (failure #{})", consecutive_failures);
                } else if consecutive_failures > 0 {
                    // 指数バックオフ: 1秒 → 2秒 → 4秒 (最大5秒)
                    let wait_secs = std::cmp::min(2u64.pow(consecutive_failures.saturating_sub(1)), 5);
                    println!("[BACKOFF] Waiting {} seconds before retry (failure #{})", wait_secs, consecutive_failures);
                    tokio::time::sleep(tokio::time::Duration::from_secs(wait_secs)).await;
                }
                // consecutive_failures == 0 の場合は即座に再接続
            }
        });

        let active_account = config.get_active_account().cloned();
        let edit_account_name = active_account.as_ref().map(|a| a.name.clone()).unwrap_or_default();
        let edit_account_host = active_account.as_ref().map(|a| a.host.clone()).unwrap_or_default();
        let edit_account_token = active_account.as_ref().and_then(|a| a.token.clone()).unwrap_or_default();
        
        Self {
            comments: VecDeque::new(),
            rx,
            tray_rx,
            tray_event_flag,
            reconnect_tx,
            _runtime: runtime,
            window_configured: false,
            show_settings: false,
            config: config.clone(),
            is_connected,
            edit_account_name,
            edit_account_host,
            edit_account_token,
            selected_account_index: None,
            emoji_cache: HashMap::new(),
            animated_emoji_cache: HashMap::new(),
            emoji_downloading: HashMap::new(),
            emoji_rx,
            emoji_tx,
        }
    }
    
    fn load_emoji(&mut self, _ctx: &egui::Context, url: &str) -> Option<TextureHandle> {
        // アニメーションキャッシュをチェック
        if self.animated_emoji_cache.contains_key(url) {
            if let Some(anim) = self.animated_emoji_cache.get(url) {
                return Some(anim.textures[anim.current_frame].clone());
            }
        }
        
        // 静止画キャッシュをチェック
        if let Some(cached) = self.emoji_cache.get(url) {
            return cached.clone();
        }
        
        // ダウンロード中かチェック
        if self.emoji_downloading.contains_key(url) {
            return None; // ダウンロード中は表示しない
        }
        
        // ダウンロード開始
        self.emoji_downloading.insert(url.to_string(), true);
        let url_clone = url.to_string();
        let emoji_tx = self.emoji_tx.clone();
        
        // 別スレッドでダウンロード
        std::thread::spawn(move || {
            if let Ok(response) = reqwest::blocking::get(&url_clone) {
                if let Ok(bytes) = response.bytes() {
                    let _ = emoji_tx.send((url_clone.clone(), bytes.to_vec()));
                }
            }
        });
        
        None
    }
    
    fn load_gif_frames(&mut self, ctx: &egui::Context, url: &str, bytes: &[u8]) -> Result<Vec<TextureHandle>, Box<dyn std::error::Error>> {
        use image::AnimationDecoder;
        use image::codecs::gif::GifDecoder;
        use std::io::Cursor;
        
        let cursor = Cursor::new(bytes);
        let decoder = GifDecoder::new(cursor)?;
        let frames = decoder.into_frames().collect_frames()?;
        
        let mut color_images = Vec::new();
        let mut durations = Vec::new();
        let mut textures = Vec::new();
        
        for (i, frame) in frames.iter().enumerate() {
            let img = frame.buffer();
            let size = [img.width() as usize, img.height() as usize];
            let pixels = img.as_flat_samples();
            let color_image = ColorImage::from_rgba_unmultiplied(size, pixels.as_slice());
            
            let texture = ctx.load_texture(
                format!("{}_frame_{}", url, i),
                color_image.clone(),
                egui::TextureOptions::LINEAR
            );
            
            color_images.push(color_image);
            textures.push(texture);
            
            let (numer, denom) = frame.delay().numer_denom_ms();
            durations.push(numer / denom.max(1));
        }
        
        if !textures.is_empty() {
            self.animated_emoji_cache.insert(url.to_string(), AnimatedEmoji {
                frames: color_images,
                frame_durations: durations,
                textures: textures.clone(),
                current_frame: 0,
                elapsed_ms: 0,
            });
        }
        
        Ok(textures)
    }
    
    fn update_animations(&mut self, dt: f32) {
        let dt_ms = (dt * 1000.0) as u32;
        
        for anim in self.animated_emoji_cache.values_mut() {
            anim.elapsed_ms += dt_ms;
            
            if anim.current_frame < anim.frame_durations.len() {
                let current_duration = anim.frame_durations[anim.current_frame];
                
                if anim.elapsed_ms >= current_duration {
                    anim.elapsed_ms = 0;
                    anim.current_frame = (anim.current_frame + 1) % anim.textures.len();
                }
            }
        }
    }

    fn configure_window_clickthrough(&mut self, frame: &eframe::Frame) {
        if let Ok(handle) = frame.window_handle() {
             if let RawWindowHandle::Win32(handle) = handle.as_raw() {
                let hwnd = HWND(handle.hwnd.get() as _);
                unsafe {
                    // 毎フレーム強制的にクリックスルーを設定
                    let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                    let new_style = ex_style | (WS_EX_LAYERED.0 as isize) | (WS_EX_TRANSPARENT.0 as isize);
                    
                    if !self.window_configured {
                        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_style);
                        println!("Window configured: WS_EX_LAYERED | WS_EX_TRANSPARENT");
                        self.window_configured = true;
                    } else {
                        // 毎フレーム確認して、必要なら再設定
                        if ex_style != new_style {
                            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_style);
                            println!("Window style reset!");
                        }
                    }
                }
            }
        }
    }
    
    fn disable_window_clickthrough(&mut self, frame: &eframe::Frame) {
        if let Ok(handle) = frame.window_handle() {
             if let RawWindowHandle::Win32(handle) = handle.as_raw() {
                let hwnd = HWND(handle.hwnd.get() as _);
                unsafe {
                    // クリックスルーを無効化（WS_EX_TRANSPARENTを削除）
                    let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                    let new_style = (ex_style | (WS_EX_LAYERED.0 as isize)) & !(WS_EX_TRANSPARENT.0 as isize);
                    SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_style);
                }
            }
        }
    }
    
    fn bring_to_foreground(&self, frame: &eframe::Frame) {
        if let Ok(handle) = frame.window_handle() {
             if let RawWindowHandle::Win32(handle) = handle.as_raw() {
                let hwnd = HWND(handle.hwnd.get() as _);
                unsafe {
                    let _ = SetForegroundWindow(hwnd);
                }
            }
        }
    }
}

impl eframe::App for MisskeyViewerApp {
    #[allow(deprecated)]
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let debug_mode = self.config.debug;
        // デルタタイムを取得
        let dt = ctx.input(|i| i.stable_dt);
        
        // アニメーション絵文字を更新
        self.update_animations(dt);
        
        // フラグをチェックしてイベント処理をトリガー
        if let Ok(mut flag) = self.tray_event_flag.try_lock() {
            if *flag {
                *flag = false;
                // フラグがtrueの場合、イベントを処理
                ctx.request_repaint();
            }
        }
        
        // トレイアイコンのメニューイベントを処理
        while let Ok(event) = self.tray_rx.try_recv() {
            match event {
                TrayEvent::Settings => {
                    println!("Opening settings... (before: {})", self.show_settings);
                    self.show_settings = true;
                    println!("Opening settings... (after: {})", self.show_settings);
                    // 設定ウィンドウを開くときのみフォアグラウンドに
                    self.bring_to_foreground(frame);
                    ctx.request_repaint();
                }
                TrayEvent::Quit => {
                    println!("Quitting...");
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
        
        // Ctrl+Sで設定画面をトグル
        ctx.input(|i| {
            if i.modifiers.ctrl && i.key_pressed(egui::Key::S) {
                self.show_settings = !self.show_settings;
            }
        });

        // 設定画面を表示
        if self.show_settings {
            // 設定画面表示中はクリックスルーを無効化
            self.disable_window_clickthrough(frame);
            
            egui::Window::new("設定")
                .collapsible(false)
                .resizable(true)
                .default_width(500.0)
                .show(ctx, |ui| {
                    ui.heading("Misskey Post Viewer 設定");
                    ui.separator();
                    
                    ui.label("アカウント一覧:");
                    ui.add_space(5.0);
                    
                    // アカウント一覧表示
                    egui::ScrollArea::vertical()
                        .max_height(150.0)
                        .show(ui, |ui| {
                            for (idx, account) in self.config.accounts.iter().enumerate() {
                                let is_active = idx == self.config.active_account_index;
                                let label_text = if is_active {
                                    format!("★ {} ({})", account.name, account.host)
                                } else {
                                    format!("  {} ({})", account.name, account.host)
                                };
                                
                                ui.horizontal(|ui| {
                                    if ui.selectable_label(self.selected_account_index == Some(idx), &label_text).clicked() {
                                        self.selected_account_index = Some(idx);
                                    }
                                });
                            }
                        });
                    
                    ui.add_space(10.0);
                    ui.separator();
                    
                    // タイムライン設定（グローバル）
                    ui.label("タイムライン設定:");
                    ui.add_space(5.0);
                    
                    if !self.config.accounts.is_empty() {
                        let active_idx = self.config.active_account_index;
                        if active_idx < self.config.accounts.len() {
                            let account = &mut self.config.accounts[active_idx];
                            ui.horizontal(|ui| {
                                ui.label("現在のアカウント:");
                                ui.label(format!("{} ({})", account.name, account.host));
                            });
                            ui.add_space(5.0);
                            egui::ComboBox::from_id_salt("timeline_selector")
                                .selected_text(account.timeline.display_name())
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut account.timeline, TimelineType::Hybrid, TimelineType::Hybrid.display_name());
                                    ui.selectable_value(&mut account.timeline, TimelineType::Local, TimelineType::Local.display_name());
                                    ui.selectable_value(&mut account.timeline, TimelineType::Home, TimelineType::Home.display_name());
                                    ui.selectable_value(&mut account.timeline, TimelineType::Global, TimelineType::Global.display_name());
                                });
                            
                            ui.add_space(5.0);
                            if ui.button("タイムライン変更を適用").clicked() {
                                // config.tomlに保存
                                if let Err(e) = self.config.save() {
                                    eprintln!("設定の保存に失敗: {}", e);
                                } else {
                                    // 再接続シグナルを送信
                                    println!("Sending reconnect signal for timeline change...");
                                    if let Err(e) = self.reconnect_tx.send(self.config.clone()) {
                                        eprintln!("再接続シグナルの送信に失敗: {}", e);
                                    } else {
                                        // コメントをクリア
                                        self.comments.clear();
                                    }
                                }
                            }
                        }
                    }
                    
                    ui.add_space(10.0);
                    ui.separator();
                    
                    // 選択中のアカウント操作
                    ui.horizontal(|ui| {
                        if ui.button("切り替え").clicked() {
                            if let Some(idx) = self.selected_account_index {
                                if idx < self.config.accounts.len() {
                                    self.config.active_account_index = idx;
                                    // config.tomlに保存
                                    if let Err(e) = self.config.save() {
                                        eprintln!("設定の保存に失敗: {}", e);
                                    } else {
                                        // 再接続シグナルを送信（再起動不要）
                                        println!("Sending reconnect signal...");
                                        if let Err(e) = self.reconnect_tx.send(self.config.clone()) {
                                            eprintln!("再接続シグナルの送信に失敗: {}", e);
                                        } else {
                                            // コメントをクリア
                                            self.comments.clear();
                                            self.show_settings = false;
                                        }
                                    }
                                }
                            }
                        }
                        
                        if ui.button("削除").clicked() {
                            if let Some(idx) = self.selected_account_index {
                                if idx < self.config.accounts.len() && self.config.accounts.len() > 1 {
                                    self.config.accounts.remove(idx);
                                    if self.config.active_account_index >= self.config.accounts.len() {
                                        self.config.active_account_index = 0;
                                    }
                                    self.selected_account_index = None;
                                }
                            }
                        }
                    });
                    
                    ui.add_space(10.0);
                    ui.separator();
                    
                    // 新規アカウント追加
                    ui.label("新規アカウント追加:");
                    ui.add_space(5.0);
                    
                    ui.label("アカウント名:");
                    ui.text_edit_singleline(&mut self.edit_account_name);
                    
                    ui.add_space(5.0);
                    
                    ui.label("サーバー (例: misskey.io):");
                    ui.text_edit_singleline(&mut self.edit_account_host);
                    
                    ui.add_space(5.0);
                    
                    ui.label("アクセストークン (オプション):");
                    ui.text_edit_singleline(&mut self.edit_account_token);
                    
                    ui.add_space(10.0);
                    
                    if ui.button("アカウントを追加").clicked() {
                        if !self.edit_account_name.is_empty() && !self.edit_account_host.is_empty() {
                            let new_account = Account {
                                name: self.edit_account_name.clone(),
                                host: self.edit_account_host.clone(),
                                token: if self.edit_account_token.is_empty() { None } else { Some(self.edit_account_token.clone()) },
                                timeline: TimelineType::default(),
                            };
                            self.config.accounts.push(new_account);
                            self.edit_account_name.clear();
                            self.edit_account_host.clear();
                            self.edit_account_token.clear();
                        }
                    }
                    
                    ui.add_space(10.0);
                    ui.separator();
                    
                    ui.horizontal(|ui| {
                        if ui.button("保存").clicked() {
                            if let Err(e) = self.config.save() {
                                eprintln!("設定の保存に失敗: {}", e);
                            }
                        }
                        
                        if ui.button("閉じる").clicked() {
                            self.show_settings = false;
                            self.selected_account_index = None;
                            self.edit_account_name.clear();
                            self.edit_account_host.clear();
                            self.edit_account_token.clear();
                        }
                    });
                    
                    ui.add_space(10.0);
                    ui.separator();
                    ui.label("ショートカット: Ctrl+S で設定画面を開く/閉じる");
                });
        } else {
            // 設定画面が閉じている時のみクリックスルーを有効化
            // eguiの入力処理を完全に無効化
            ctx.input_mut(|i| {
                i.events.clear();
                i.pointer = Default::default();
                i.raw.hovered_files.clear();
                i.raw.dropped_files.clear();
            });

            // ウィンドウ設定
            self.configure_window_clickthrough(frame);
        }

        // ダウンロード完了した絵文字を処理
        while let Ok((url, bytes)) = self.emoji_rx.try_recv() {
            self.emoji_downloading.remove(&url);
            
            // GIFかどうかチェック
            if url.ends_with(".gif") {
                if let Ok(_frames) = self.load_gif_frames(ctx, &url, &bytes) {
                    // 既にload_gif_framesでキャッシュに追加されている
                    ctx.request_repaint(); // 再描画をリクエスト
                    continue;
                }
            }
            
            // 通常の画像として読み込み
            if let Ok(img) = image::load_from_memory(&bytes) {
                let size = [img.width() as usize, img.height() as usize];
                let rgba = img.to_rgba8();
                let pixels = rgba.as_flat_samples();
                let color_image = ColorImage::from_rgba_unmultiplied(size, pixels.as_slice());
                
                let texture = ctx.load_texture(
                    &url,
                    color_image,
                    egui::TextureOptions::LINEAR
                );
                
                self.emoji_cache.insert(url, Some(texture));
                ctx.request_repaint(); // 再描画をリクエスト
            } else {
                self.emoji_cache.insert(url, None);
            }
        }
        
        // 新しいコメントを受信
        while let Ok(mut comment) = self.rx.try_recv() {
            // 画面サイズに合わせて初期X座標を調整
            let rect = ctx.viewport_rect();
            comment.x = rect.width();

            // Y座標も画面内に収まるように再調整（簡易的）
            if comment.y > rect.height() - 50.0 {
                comment.y = rect.height() / 2.0;
            }
            self.comments.push_back(comment);
        }

        // コメントの位置更新と描画
        let dt = ctx.input(|i| i.stable_dt).min(0.1); // デルタタイム
        let _screen_rect = ctx.screen_rect();

        // デバッグ: 先頭コメントが画面に入るとき
        if let Some(first) = self.comments.front() {
            if first.x > _screen_rect.width() - 10.0 {
                 if debug_mode { println!("First comment is entering screen: x={}", first.x); }
            }
        }

        // 接続状態をチェック
        if !*self.is_connected.lock().unwrap() {
            // 接続中メッセージを表示
            let painter = ctx.layer_painter(egui::LayerId::background());
            let rect = ctx.screen_rect();
            let center = rect.center();
            let font_id = egui::FontId::proportional(48.0);
            let text = "接続中...";
            painter.text(
                center,
                egui::Align2::CENTER_CENTER,
                text,
                font_id.clone(),
                egui::Color32::WHITE,
            );
            ctx.request_repaint();
            return;
        }
        
        // 絵文字を事前にロード
        let emoji_urls: Vec<String> = self.comments.iter()
            .flat_map(|c| c.emojis.iter().map(|e| e.url.clone()))
            .collect();
        for url in emoji_urls {
            self.load_emoji(ctx, &url);
        }
        
        // レイヤーペインターを使って直接描画
        let painter = ctx.layer_painter(egui::LayerId::background());

        let mut retain_indices = Vec::new();
        for (i, comment) in self.comments.iter_mut().enumerate() {
            comment.x -= comment.speed * 60.0 * dt; // 60fps基準で速度調整

            // 描画
            // 名前(@id)の形式で表示（リノートの場合は元投稿情報も含む）
            let text = if let Some((orig_name, orig_username, orig_host, _)) = &comment.renote_info {
                // リノートの場合
                let orig_display = if orig_host.is_empty() {
                    format!("{}(@{})", orig_name, orig_username)
                } else {
                    format!("{}(@{}@{})", orig_name, orig_username, orig_host)
                };
                let user_display = if let Some(host) = &comment.user_host {
                    format!("{}(@{}@{})", comment.name, comment.username, host)
                } else {
                    format!("{}(@{})", comment.name, comment.username)
                };
                format!("{}: Rn({}): {}", user_display, orig_display, comment.text)
            } else {
                // 通常の投稿
                let user_display = if let Some(host) = &comment.user_host {
                    format!("{}(@{}@{})", comment.name, comment.username, host)
                } else {
                    format!("{}(@{})", comment.name, comment.username)
                };
                format!("{}: {}", user_display, comment.text)
            };
            
            // 絵文字を含むテキストを処理
            // テキストを分割して、テキスト部分と絵文字部分を識別
            let mut segments = Vec::new(); // (is_emoji, content, emoji_info)
            let mut current_text = String::new();
            let mut chars = text.chars().peekable();
            
            while let Some(ch) = chars.next() {
                if ch == ':' {
                    // 絵文字タグの可能性をチェック
                    let mut emoji_name = String::new();
                    let mut temp_chars = chars.clone();
                    let mut found_emoji = false;
                    
                    while let Some(&next_ch) = temp_chars.peek() {
                        if next_ch == ':' {
                            // 絵文字が存在するかチェック
                            if let Some(emoji_info) = comment.emojis.iter().find(|e| e.name == emoji_name) {
                                // テキスト部分を保存
                                if !current_text.is_empty() {
                                    segments.push((false, current_text.clone(), None));
                                    current_text.clear();
                                }
                                // 絵文字部分を保存
                                segments.push((true, emoji_name.clone(), Some(emoji_info.clone())));
                                // チャーイテレータを進める
                                for _ in 0..emoji_name.len() + 1 {
                                    chars.next();
                                }
                                found_emoji = true;
                                break;
                            } else {
                                break;
                            }
                        } else if next_ch.is_alphanumeric() || next_ch == '_' || next_ch == '-' {
                            emoji_name.push(next_ch);
                            temp_chars.next();
                        } else {
                            break;
                        }
                    }
                    
                    if !found_emoji {
                        current_text.push(ch);
                    }
                } else {
                    current_text.push(ch);
                }
            }
            
            if !current_text.is_empty() {
                segments.push((false, current_text, None));
            }
            
            // セグメントごとに描画
            let font_id = egui::FontId::proportional(24.0);
            let mut current_x = comment.x;
            
            for (is_emoji, content, emoji_info) in segments {
                if is_emoji {
                    // 絵文字を画像として描画
                    if let Some(emoji_info) = emoji_info {
                        // アニメーション絵文字をチェック
                        let texture = if let Some(anim) = self.animated_emoji_cache.get(&emoji_info.url) {
                            Some(&anim.textures[anim.current_frame])
                        } else {
                            // 静止画絵文字をチェック
                            self.emoji_cache.get(&emoji_info.url).and_then(|opt| opt.as_ref())
                        };
                        
                        if let Some(texture) = texture {
                            let emoji_height = 24.0;
                            let texture_size = texture.size();
                            let aspect_ratio = texture_size[0] as f32 / texture_size[1] as f32;
                            let emoji_width = emoji_height * aspect_ratio;
                            
                            // テキストのベースラインに合わせるため、少し下にオフセット
                            let emoji_y_offset = 3.0; // フォントのディセンダーを考慮した調整
                            
                            let emoji_rect = egui::Rect::from_min_size(
                                egui::pos2(current_x, comment.y + emoji_y_offset),
                                egui::vec2(emoji_width, emoji_height)
                            );
                            painter.image(
                                texture.id(),
                                emoji_rect,
                                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                                egui::Color32::WHITE
                            );
                            current_x += emoji_width;
                        }
                    }
                } else {
                    // テキストを描画
                    // 影
                    painter.text(
                        egui::pos2(current_x, comment.y) + egui::vec2(2.0, 2.0),
                        egui::Align2::LEFT_TOP,
                        &content,
                        font_id.clone(),
                        egui::Color32::BLACK,
                    );
                    // 本体
                    let galley = painter.layout_no_wrap(
                        content.clone(),
                        font_id.clone(),
                        egui::Color32::WHITE
                    );
                    painter.text(
                        egui::pos2(current_x, comment.y),
                        egui::Align2::LEFT_TOP,
                        &content,
                        font_id.clone(),
                        egui::Color32::WHITE,
                    );
                    current_x += galley.rect.width();
                }
            }
            
            // テキストの幅を推定して、完全に画面外に出てから削除
            // current_xが最終的な右端位置なので、それを使用
            let total_width = current_x - comment.x;
            if comment.x + total_width > -10.0 { // テキストが完全に左に出たら消す
                retain_indices.push(i);
            }
        }

        // 不要なコメントを削除
        if retain_indices.len() != self.comments.len() {
             // 簡易実装：先頭のX座標が画面外ならpop_front
             while let Some(front) = self.comments.front() {
                 // テキストの推定幅を計算（1文字約15ピクセルと仮定）
                 let estimated_width = front.text.chars().count() as f32 * 15.0 
                     + front.name.chars().count() as f32 * 15.0 
                     + front.username.chars().count() as f32 * 15.0 
                     + 200.0; // ユーザー情報の追加分
                 if front.x + estimated_width < -10.0 {
                     self.comments.pop_front();
                 } else {
                     break;
                 }
             }
        }

        // アニメーションのために常時再描画をリクエスト
        // バックグラウンドでもイベントを処理できるように短い間隔で再描画
        ctx.request_repaint_after(std::time::Duration::from_millis(16)); // 約60fps
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // 背景を完全に透明にする
        [0.0, 0.0, 0.0, 0.0]
    }
}

fn trigger_window_update() {
    use windows::Win32::Foundation::{WPARAM, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_SHOW};
    unsafe {
        // ウィンドウタイトルからウィンドウを探す
        let title = windows::core::w!("Misskey Post Viewer");
        if let Ok(hwnd) = FindWindowW(None, title) {
            if !hwnd.is_invalid() {
                // ウィンドウを表示状態にして、フォアグラウンドに持ってくる
                let _ = ShowWindow(hwnd, SW_SHOW);
                let _ = SetForegroundWindow(hwnd);
                // カスタムメッセージを送信して更新をトリガー
                let _ = PostMessageW(Some(hwnd), WM_USER + 1, WPARAM(0), LPARAM(0));
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // トレイアイコンのメニュー作成
    let tray_menu = Menu::new();
    let settings_item = MenuItem::with_id("settings", "設定", true, None);
    let quit_item = MenuItem::with_id("quit", "終了", true, None);
    let settings_id = settings_item.id().clone();
    let quit_id = quit_item.id().clone();
    tray_menu.append(&settings_item)?;
    tray_menu.append(&quit_item)?;

    // トレイイベント用のチャネルとフラグを作成
    let (tray_tx, tray_rx) = unbounded();
    let tray_event_flag = Arc::new(Mutex::new(false));
    let tray_event_flag_clone = tray_event_flag.clone();
    
    // 別スレッドでトレイアイコンのイベントを監視
    std::thread::spawn(move || {
        let menu_receiver = tray_icon::menu::MenuEvent::receiver();
        loop {
            if let Ok(event) = menu_receiver.recv() {
                println!("Tray event received: {:?}", event.id);
                let tray_event = if event.id == settings_id {
                    println!("Sending Settings event...");
                    TrayEvent::Settings
                } else if event.id == quit_id {
                    println!("Sending Quit event...");
                    TrayEvent::Quit
                } else {
                    println!("Unknown event, skipping");
                    continue;
                };
                match tray_tx.send(tray_event) {
                    Ok(_) => {
                        println!("Event sent successfully!");
                        // フラグを立てる
                        if let Ok(mut flag) = tray_event_flag_clone.lock() {
                            *flag = true;
                        }
                        // ウィンドウを強制的に更新
                        trigger_window_update();
                    }
                    Err(e) => {
                        println!("Failed to send event: {:?}", e);
                        break;
                    }
                }
            }
        }
    });
    
    // トレイアイコン作成（シンプルなアイコン画像を生成）
    let icon_rgba = {
        let size = 32;
        let mut rgba = vec![0u8; size * size * 4];
        for y in 0..size {
            for x in 0..size {
                let idx = (y * size + x) * 4;
                // 青い円を描画
                let dx = x as i32 - size as i32 / 2;
                let dy = y as i32 - size as i32 / 2;
                let dist = ((dx * dx + dy * dy) as f32).sqrt();
                if dist < size as f32 / 2.0 {
                    rgba[idx] = 50;      // R
                    rgba[idx + 1] = 150; // G
                    rgba[idx + 2] = 255; // B
                    rgba[idx + 3] = 255; // A
                }
            }
        }
        rgba
    };
    
    let icon = tray_icon::Icon::from_rgba(icon_rgba, 32, 32)?;
    
    let _tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_tooltip("Misskey Post Viewer")
        .with_icon(icon)
        .build()?;
    
    // 設定読み込み
    let config = match AppConfig::new() {
        Ok(c) => {
            if let Some(account) = c.get_active_account() {
                println!("Loaded configuration: {} ({})", account.name, account.host);
            }
            c
        },
        Err(e) => {
            eprintln!("Failed to load configuration: {}", e);
            eprintln!("Using default configuration (misskey.io)");
            AppConfig {
                accounts: vec![Account {
                    name: "Default".to_string(),
                    host: "misskey.io".to_string(),
                    token: None,
                    timeline: TimelineType::default(),
                }],
                active_account_index: 0,
                debug: false,
                fallback_font: None,
                host: None,
                token: None,
            }
        }
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_decorations(false) // 枠なし
            .with_transparent(true) // 透明化を有効
            .with_always_on_top() // 最前面
            .with_maximized(true) // 最大化
            .with_position([0.0, 0.0])
            .with_mouse_passthrough(false)
            .with_visible(true) // 明示的に可視化
            .with_active(true), // アクティブ状態を維持
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };

    let config_clone = config.clone();
    eframe::run_native(
        "Misskey Post Viewer",
        options,
        Box::new(move |cc| Ok(Box::new(MisskeyViewerApp::new(cc, config_clone, tray_rx, tray_event_flag)))),
    )?;

    Ok(())
}
