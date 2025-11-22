#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use misskey_post_viewer::{MisskeyClient, AppConfig, Account, TimelineType, EmojiInfo, EmojiCache};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::collections::VecDeque;
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

#[derive(Clone)]
struct UrlPreview {
    url: String,
    title: String,
    description: Option<String>,
    image_url: Option<String>,
    site_name: Option<String>,
    favicon_url: Option<String>,
}

struct PreviewImageCache {
    cache: std::collections::HashMap<String, Option<egui::TextureHandle>>,
    downloading: std::collections::HashMap<String, bool>,
    rx: std::sync::mpsc::Receiver<(String, egui::ColorImage)>,
    tx: std::sync::mpsc::Sender<(String, egui::ColorImage)>,
}

impl PreviewImageCache {
    fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<(String, egui::ColorImage)>();
        Self {
            cache: std::collections::HashMap::new(),
            downloading: std::collections::HashMap::new(),
            rx,
            tx,
        }
    }

    fn load_image(&mut self, url: &str, debug_mode: bool) -> Option<egui::TextureHandle> {
        // キャッシュをチェック
        if let Some(cached) = self.cache.get(url) {
            return cached.clone();
        }
        
        // ダウンロード中かチェック
        if self.downloading.contains_key(url) {
            return None;
        }
        
        // ダウンロード開始
        self.downloading.insert(url.to_string(), true);
        let url_clone = url.to_string();
        let tx = self.tx.clone();
        
        std::thread::spawn(move || {
            use std::time::Duration;
            
            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(10))
                .build();
            
            match client {
                Ok(client) => {
                    match client.get(&url_clone).send() {
                        Ok(response) => {
                            if response.status().is_success() {
                                match response.bytes() {
                                    Ok(bytes) => {
                                        // 画像デコードもこのスレッドで実行
                                        match image::load_from_memory(&bytes) {
                                            Ok(img) => {
                                                let size = [img.width() as usize, img.height() as usize];
                                                let rgba = img.to_rgba8();
                                                let pixels = rgba.as_flat_samples();
                                                let color_image = egui::ColorImage::from_rgba_unmultiplied(
                                                    size, 
                                                    pixels.as_slice()
                                                );
                                                let _ = tx.send((url_clone.clone(), color_image));
                                            }
                                            Err(e) => {
                                                if debug_mode { 
                                                    eprintln!("Failed to decode preview image {}: {}", url_clone, e); 
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        if debug_mode { eprintln!("Failed to read preview image bytes: {}", e); }
                                    }
                                }
                            } else {
                                if debug_mode { eprintln!("Failed to download preview image (HTTP {})", response.status()); }
                            }
                        }
                        Err(e) => {
                            if debug_mode { eprintln!("Failed to download preview image: {}", e); }
                        }
                    }
                }
                Err(e) => {
                    if debug_mode { eprintln!("Failed to create HTTP client: {}", e); }
                }
            }
        });
        
        None
    }

    fn process_downloads(&mut self, ctx: &egui::Context, _debug_mode: bool) {
        while let Ok((url, color_image)) = self.rx.try_recv() {
            self.downloading.remove(&url);
            
            // デコード済みの画像をテクスチャに変換（軽い処理）
            let texture = ctx.load_texture(
                &url,
                color_image,
                egui::TextureOptions::LINEAR
            );
            
            self.cache.insert(url, Some(texture));
            ctx.request_repaint();
        }
    }
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
    url_preview: Option<UrlPreview>, // URLプレビュー情報
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
    // 絵文字キャッシュ
    emoji_cache: EmojiCache,
    // プレビュー画像キャッシュ
    preview_image_cache: PreviewImageCache,
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
                                                            
                                                            // 正規表現で:emoji_name:パターンを抽出
                                                            use regex::Regex;
                                                            let emoji_pattern = Regex::new(r":([a-zA-Z0-9_]+):").unwrap();
                                                            let emoji_names: Vec<String> = emoji_pattern
                                                                .captures_iter(&all_text)
                                                                .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_string()))
                                                                .collect();
                                                            
                                                            for emoji_name in emoji_names {
                                                                // 既に取得済みかチェック
                                                                if !emojis.iter().any(|e| e.name == emoji_name) {
                                                                    // APIから取得を試みる（非同期）
                                                                    if let Ok(response) = reqwest::get(format!("https://{}/api/emoji?name={}", host, emoji_name)).await {
                                                                        if let Ok(emoji_data) = response.json::<serde_json::Value>().await {
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
                                                                
                                                                // リノート元のテキストから絵文字を抽出
                                                                let mut renote_text_for_emoji = String::new();
                                                                if let Some(text) = renote.get("text").and_then(|v| v.as_str()) {
                                                                    renote_text_for_emoji.push_str(text);
                                                                }
                                                                if let Some(cw) = renote.get("cw").and_then(|v| v.as_str()) {
                                                                    renote_text_for_emoji.push(' ');
                                                                    renote_text_for_emoji.push_str(cw);
                                                                }
                                                                // リノート元の投稿者名も追加
                                                                renote_text_for_emoji.push(' ');
                                                                renote_text_for_emoji.push_str(&orig_name);
                                                                
                                                                // 正規表現で:emoji_name:パターンを抽出
                                                                let renote_emoji_names: Vec<String> = emoji_pattern
                                                                    .captures_iter(&renote_text_for_emoji)
                                                                    .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_string()))
                                                                    .collect();
                                                                
                                                                for emoji_name in renote_emoji_names {
                                                                    // 既に取得済みかチェック
                                                                    if !emojis.iter().any(|e| e.name == emoji_name) {
                                                                        // APIから取得を試みる
                                                                        if let Ok(response) = reqwest::get(format!("https://{}/api/emoji?name={}", host, emoji_name)).await {
                                                                            if let Ok(emoji_data) = response.json::<serde_json::Value>().await {
                                                                                if let Some(url) = emoji_data.get("url").and_then(|v| v.as_str()) {
                                                                                    if debug_mode { println!("DEBUG: Fetched renote emoji from API {} -> {}", emoji_name, url); }
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
                                                                // URL検出してOGPメタデータを取得（非同期）
                                                                let url_preview = if let Some(url) = detect_url(&text_content) {
                                                                    if debug_mode {
                                                                        println!("DEBUG: Detected URL: {}", url);
                                                                    }
                                                                    // OGPメタデータを非同期で取得
                                                                    fetch_ogp_metadata(&url, debug_mode).await
                                                                } else {
                                                                    None
                                                                };
                                                                
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
                                                                    url_preview,
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
            emoji_cache: EmojiCache::new(),
            preview_image_cache: PreviewImageCache::new(),
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
        let dt_ms = (dt * 1000.0) as u32;
        self.emoji_cache.update_animations(dt_ms);
        
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
        self.emoji_cache.process_downloads(ctx, debug_mode);
        
        // ダウンロード完了したプレビュー画像を処理
        self.preview_image_cache.process_downloads(ctx, debug_mode);
        
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
            self.emoji_cache.load_emoji(ctx, &url, debug_mode);
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
            
            // セグメントごとに描画（改行を考慮）
            let font_id = egui::FontId::proportional(24.0);
            let line_height = 28.0; // 行の高さ
            let mut current_x = comment.x;
            let mut current_line = 0;
            
            for (is_emoji, content, emoji_info) in segments {
                if is_emoji {
                    // 絵文字を画像として描画
                    if let Some(emoji_info) = emoji_info {
                        // アニメーション絵文字をチェック
                        let texture = if let Some(anim) = self.emoji_cache.animated_cache.get(&emoji_info.url) {
                            Some(&anim.textures[anim.current_frame])
                        } else {
                            // 静止画絵文字をチェック
                            self.emoji_cache.static_cache.get(&emoji_info.url).and_then(|opt| opt.as_ref())
                        };
                        
                        if let Some(texture) = texture {
                            let emoji_height = 24.0;
                            let texture_size = texture.size();
                            let aspect_ratio = texture_size[0] as f32 / texture_size[1] as f32;
                            let emoji_width = emoji_height * aspect_ratio;
                            
                            // テキストのベースラインに合わせるため、少し下にオフセット
                            let emoji_y_offset = 3.0; // フォントのディセンダーを考慮した調整
                            
                            let emoji_rect = egui::Rect::from_min_size(
                                egui::pos2(current_x, comment.y + (current_line as f32 * line_height) + emoji_y_offset),
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
                    // テキストを改行ごとに分割して描画
                    for (line_idx, line) in content.split('\n').enumerate() {
                        if line_idx > 0 {
                            // 改行があった場合
                            current_line += 1;
                            current_x = comment.x; // X座標をリセット
                        }
                        
                        if !line.is_empty() {
                            let current_y = comment.y + (current_line as f32 * line_height);
                            
                            // 影
                            painter.text(
                                egui::pos2(current_x, current_y) + egui::vec2(2.0, 2.0),
                                egui::Align2::LEFT_TOP,
                                line,
                                font_id.clone(),
                                egui::Color32::BLACK,
                            );
                            // 本体
                            let galley = painter.layout_no_wrap(
                                line.to_string(),
                                font_id.clone(),
                                egui::Color32::WHITE
                            );
                            painter.text(
                                egui::pos2(current_x, current_y),
                                egui::Align2::LEFT_TOP,
                                line,
                                font_id.clone(),
                                egui::Color32::WHITE,
                            );
                            current_x += galley.rect.width();
                        }
                    }
                }
            }
            
            // URLプレビューを表示
            if let Some(preview) = &comment.url_preview {
                // プレビューカードをすべての行の下に表示
                let card_y = comment.y + ((current_line + 1) as f32 * line_height); // 最終行の下に表示
                let card_x = comment.x; // テキストの開始位置と同じX座標
                let thumbnail_size = 80.0; // サムネイルのサイズ
                
                // 画像URLの有無でレイアウトを変更
                let has_image = preview.image_url.is_some();
                let card_width = if has_image { 350.0 } else { 280.0 };
                let left_offset = if has_image { thumbnail_size + 8.0 } else { 8.0 };
                let text_max_width = card_width - left_offset - 8.0;
                
                // 内容に応じてカードの高さを計算
                let mut content_height: f32 = 10.0; // 上下の余白
                let has_description = preview.description.is_some();
                
                // タイトル: 16px
                content_height += 16.0;
                // 説明: 13px (ある場合のみ)
                if has_description {
                    content_height += 13.0;
                }
                // URL: 13px
                content_height += 13.0;
                // サイト名/Favicon: 16px (常に表示)
                content_height += 16.0;
                
                // 画像がある場合は最低80pxを確保
                let card_height = if has_image {
                    content_height.max(thumbnail_size)
                } else {
                    content_height
                };
                
                // カード背景
                let card_rect = egui::Rect::from_min_size(
                    egui::pos2(card_x, card_y),
                    egui::vec2(card_width, card_height)
                );
                painter.rect_filled(
                    card_rect,
                    egui::Rounding::same(4),
                    egui::Color32::from_rgba_premultiplied(30, 30, 30, 240)
                );
                
                // サムネイル画像（左側）- 画像URLがある場合のみ
                if let Some(image_url) = &preview.image_url {
                    let thumbnail_rect = egui::Rect::from_min_size(
                        egui::pos2(card_x, card_y),
                        egui::vec2(thumbnail_size, card_height)
                    );
                    
                    // 画像をロードして表示
                    if let Some(texture) = self.preview_image_cache.load_image(image_url, self.config.debug) {
                        // アスペクト比を維持してサムネイルに収める
                        let img_size = texture.size_vec2();
                        let aspect = img_size.x / img_size.y;
                        let (draw_width, draw_height) = if aspect > (thumbnail_size / card_height) {
                            // 横長画像
                            (thumbnail_size, thumbnail_size / aspect)
                        } else {
                            // 縦長画像
                            (card_height * aspect, card_height)
                        };
                        
                        let offset_x = (thumbnail_size - draw_width) / 2.0;
                        let offset_y = (card_height - draw_height) / 2.0;
                        
                        let draw_rect = egui::Rect::from_min_size(
                            egui::pos2(card_x + offset_x, card_y + offset_y),
                            egui::vec2(draw_width, draw_height)
                        );
                        
                        painter.image(
                            texture.id(),
                            draw_rect,
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            egui::Color32::WHITE
                        );
                    } else {
                        // 画像読み込み中は背景色を表示
                        painter.rect_filled(
                            thumbnail_rect,
                            egui::Rounding::same(4),
                            egui::Color32::from_rgb(60, 60, 60)
                        );
                    }
                }
                
                // タイトルと説明を表示
                let title_font = egui::FontId::proportional(12.0);
                let desc_font = egui::FontId::proportional(9.0);
                let url_font = egui::FontId::proportional(9.0);
                
                let text_x = card_x + left_offset; // 画像の有無で位置を調整
                let mut text_y = card_y + 5.0;
                
                // タイトル（幅で切り詰め）
                let title_galley = painter.layout_no_wrap(
                    preview.title.clone(),
                    title_font.clone(),
                    egui::Color32::WHITE
                );
                let title_text = if title_galley.rect.width() > text_max_width {
                    let mut truncated = preview.title.clone();
                    while !truncated.is_empty() {
                        let test_galley = painter.layout_no_wrap(
                            format!("{}...", truncated),
                            title_font.clone(),
                            egui::Color32::WHITE
                        );
                        if test_galley.rect.width() <= text_max_width {
                            break;
                        }
                        truncated.pop();
                    }
                    format!("{}...", truncated)
                } else {
                    preview.title.clone()
                };
                
                painter.text(
                    egui::pos2(text_x, text_y),
                    egui::Align2::LEFT_TOP,
                    title_text,
                    title_font,
                    egui::Color32::WHITE
                );
                text_y += 16.0;
                
                // 説明（幅で切り詰め）
                if let Some(description) = &preview.description {
                    let desc_galley = painter.layout_no_wrap(
                        description.clone(),
                        desc_font.clone(),
                        egui::Color32::from_rgb(180, 180, 180)
                    );
                    let desc_text = if desc_galley.rect.width() > text_max_width {
                        let mut truncated = description.clone();
                        while !truncated.is_empty() {
                            let test_galley = painter.layout_no_wrap(
                                format!("{}...", truncated),
                                desc_font.clone(),
                                egui::Color32::from_rgb(180, 180, 180)
                            );
                            if test_galley.rect.width() <= text_max_width {
                                break;
                            }
                            truncated.pop();
                        }
                        format!("{}...", truncated)
                    } else {
                        description.clone()
                    };
                    
                    painter.text(
                        egui::pos2(text_x, text_y),
                        egui::Align2::LEFT_TOP,
                        desc_text,
                        desc_font.clone(),
                        egui::Color32::from_rgb(180, 180, 180)
                    );
                    text_y += 13.0;
                }
                
                // URL（幅で切り詰め）
                let url_galley = painter.layout_no_wrap(
                    preview.url.clone(),
                    url_font.clone(),
                    egui::Color32::from_rgb(120, 140, 180)
                );
                let url_text = if url_galley.rect.width() > text_max_width {
                    let mut truncated = preview.url.clone();
                    while !truncated.is_empty() {
                        let test_galley = painter.layout_no_wrap(
                            format!("{}...", truncated),
                            url_font.clone(),
                            egui::Color32::from_rgb(120, 140, 180)
                        );
                        if test_galley.rect.width() <= text_max_width {
                            break;
                        }
                        truncated.pop();
                    }
                    format!("{}...", truncated)
                } else {
                    preview.url.clone()
                };
                
                painter.text(
                    egui::pos2(text_x, text_y),
                    egui::Align2::LEFT_TOP,
                    url_text,
                    url_font,
                    egui::Color32::from_rgb(120, 140, 180)
                );
                text_y += 13.0;
                
                // サイト名とFavicon（下部）
                let site_font = egui::FontId::proportional(9.0);
                let favicon_size = 12.0;
                
                // Faviconがあるかチェック
                let mut has_favicon = false;
                if let Some(favicon_url) = &preview.favicon_url {
                    if let Some(favicon_texture) = self.preview_image_cache.load_image(favicon_url, self.config.debug) {
                        let favicon_rect = egui::Rect::from_min_size(
                            egui::pos2(text_x, text_y),
                            egui::vec2(favicon_size, favicon_size)
                        );
                        painter.image(
                            favicon_texture.id(),
                            favicon_rect,
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            egui::Color32::WHITE
                        );
                        has_favicon = true;
                    }
                }
                
                // サイト名の表示位置（Faviconがあれば右側、なければ左端）
                let site_text_x = if has_favicon {
                    text_x + favicon_size + 4.0
                } else {
                    text_x
                };
                
                // サイト名を表示
                let site_display_name = if let Some(site_name) = &preview.site_name {
                    site_name.clone()
                } else {
                    // サイト名がない場合はURLのホスト部分を表示
                    if let Ok(parsed_url) = reqwest::Url::parse(&preview.url) {
                        parsed_url.host_str().unwrap_or(&preview.url).to_string()
                    } else {
                        preview.url.clone()
                    }
                };
                
                painter.text(
                    egui::pos2(site_text_x, text_y),
                    egui::Align2::LEFT_TOP,
                    site_display_name,
                    site_font,
                    egui::Color32::from_rgb(150, 150, 150)
                );
                
                current_x = card_x + card_width; // プレビューカードの右端まで幅を拡張
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

// URLを検出する（軽量な処理）
fn detect_url(text: &str) -> Option<String> {
    use regex::Regex;
    let url_regex = Regex::new(r"https?://[^\s]+").ok()?;
    url_regex.find(text).map(|m| m.as_str().to_string())
}

// OGPメタデータを非同期で取得
async fn fetch_ogp_metadata(url: &str, debug_mode: bool) -> Option<UrlPreview> {
    use scraper::{Html, Selector};
    use std::time::Duration;
    
    if debug_mode {
        println!("DEBUG: Fetching OGP metadata for: {}", url);
    }
    
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
        .build()
        .ok()?;
    
    let response = client.get(url).send().await.ok()?;
    
    if debug_mode {
        println!("DEBUG: HTTP status: {}", response.status());
    }
    
    let html_content = response.text().await.ok()?;
    let document = Html::parse_document(&html_content);
    
    // OGPタグとフォールバック用のセレクター
    let og_title_selector = Selector::parse(r#"meta[property="og:title"]"#).ok()?;
    let og_description_selector = Selector::parse(r#"meta[property="og:description"]"#).ok()?;
    let og_image_selector = Selector::parse(r#"meta[property="og:image"]"#).ok()?;
    let og_site_name_selector = Selector::parse(r#"meta[property="og:site_name"]"#).ok()?;
    let title_selector = Selector::parse("title").ok()?;
    let description_selector = Selector::parse(r#"meta[name="description"]"#).ok()?;
    let favicon_selector = Selector::parse(r#"link[rel="icon"], link[rel="shortcut icon"]"#).ok()?;
    
    // タイトル取得
    let title = document
        .select(&og_title_selector)
        .next()
        .and_then(|el| el.value().attr("content"))
        .map(|s| s.to_string())
        .or_else(|| {
            document
                .select(&title_selector)
                .next()
                .map(|el| el.text().collect::<String>())
        });
    
    // 説明取得
    let description = document
        .select(&og_description_selector)
        .next()
        .and_then(|el| el.value().attr("content"))
        .map(|s| s.to_string())
        .or_else(|| {
            document
                .select(&description_selector)
                .next()
                .and_then(|el| el.value().attr("content"))
                .map(|s| s.to_string())
        });
    
    // 画像URL取得
    let image_url = document
        .select(&og_image_selector)
        .next()
        .and_then(|el| el.value().attr("content"))
        .map(|s| s.to_string());
    
    // サイト名取得
    let site_name = document
        .select(&og_site_name_selector)
        .next()
        .and_then(|el| el.value().attr("content"))
        .map(|s| s.to_string());
    
    // Favicon URL取得
    let favicon_url = document
        .select(&favicon_selector)
        .next()
        .and_then(|el| el.value().attr("href"))
        .map(|href| {
            // 相対URLを絶対URLに変換
            if href.starts_with("http://") || href.starts_with("https://") {
                href.to_string()
            } else if href.starts_with("//") {
                format!("https:{}", href)
            } else if href.starts_with('/') {
                // URLのホスト部分を抽出
                if let Ok(parsed_url) = reqwest::Url::parse(url) {
                    format!("{}://{}{}", parsed_url.scheme(), parsed_url.host_str().unwrap_or(""), href)
                } else {
                    href.to_string()
                }
            } else {
                // 相対パス
                if let Ok(parsed_url) = reqwest::Url::parse(url) {
                    format!("{}://{}/{}", parsed_url.scheme(), parsed_url.host_str().unwrap_or(""), href)
                } else {
                    href.to_string()
                }
            }
        })
        .or_else(|| {
            // Faviconが見つからない場合はデフォルトの/favicon.icoを試す
            if let Ok(parsed_url) = reqwest::Url::parse(url) {
                Some(format!("{}://{}/favicon.ico", parsed_url.scheme(), parsed_url.host_str().unwrap_or("")))
            } else {
                None
            }
        });
    
    // サイト名がない場合はホスト名を使用
    let site_name = site_name.or_else(|| {
        if let Ok(parsed_url) = reqwest::Url::parse(url) {
            parsed_url.host_str().map(|h| h.to_string())
        } else {
            None
        }
    });
    
    if debug_mode {
        println!("DEBUG: OGP - title: {:?}, description: {:?}, image: {:?}, site_name: {:?}, favicon: {:?}", 
                 title, description, image_url, site_name, favicon_url);
    }
    
    Some(UrlPreview {
        url: url.to_string(),
        title: title.unwrap_or_else(|| url.to_string()),
        description,
        image_url,
        site_name,
        favicon_url,
    })
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
    
    // トレイアイコン作成（icon.icoファイルから読み込み）
    let icon = {
        let icon_bytes = include_bytes!("../icon.ico");
        let icon_image = image::load_from_memory(icon_bytes)?;
        let icon_rgba = icon_image.to_rgba8();
        let (width, height) = icon_rgba.dimensions();
        tray_icon::Icon::from_rgba(icon_rgba.into_raw(), width, height)?
    };
    
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
            }
        }
    };

    // ウィンドウアイコン用の画像を読み込み
    let window_icon = {
        let icon_bytes = include_bytes!("../icon.ico");
        let icon_image = image::load_from_memory(icon_bytes).ok();
        icon_image.map(|img| {
            let rgba = img.to_rgba8();
            let (width, height) = rgba.dimensions();
            egui::IconData {
                rgba: rgba.into_raw(),
                width: width,
                height: height,
            }
        })
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
            .with_active(true) // アクティブ状態を維持
            .with_icon(window_icon.unwrap_or_else(|| {
                // フォールバック: デフォルトアイコン
                egui::IconData {
                    rgba: vec![0; 32 * 32 * 4],
                    width: 32,
                    height: 32,
                }
            })),
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
