use egui::{ColorImage, TextureHandle, Context};
use std::collections::HashMap;

#[derive(Clone)]
pub struct EmojiInfo {
    pub name: String,
    pub url: String,
}

pub struct AnimatedEmoji {
    pub frames: Vec<ColorImage>,
    pub frame_durations: Vec<u32>, // ミリ秒
    pub textures: Vec<TextureHandle>,
    pub current_frame: usize,
    pub elapsed_ms: u32,
}

pub struct EmojiCache {
    pub static_cache: HashMap<String, Option<TextureHandle>>,
    pub animated_cache: HashMap<String, AnimatedEmoji>,
    pub downloading: HashMap<String, bool>,
    pub rx: std::sync::mpsc::Receiver<(String, Vec<u8>)>,
    pub tx: std::sync::mpsc::Sender<(String, Vec<u8>)>,
}

impl EmojiCache {
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<(String, Vec<u8>)>();
        Self {
            static_cache: HashMap::new(),
            animated_cache: HashMap::new(),
            downloading: HashMap::new(),
            rx,
            tx,
        }
    }

    pub fn load_emoji(&mut self, _ctx: &Context, url: &str, debug_mode: bool) -> Option<TextureHandle> {
        // アニメーションキャッシュをチェック
        if self.animated_cache.contains_key(url) {
            if let Some(anim) = self.animated_cache.get(url) {
                return Some(anim.textures[anim.current_frame].clone());
            }
        }
        
        // 静止画キャッシュをチェック
        if let Some(cached) = self.static_cache.get(url) {
            return cached.clone();
        }
        
        // ダウンロード中かチェック
        if self.downloading.contains_key(url) {
            return None; // ダウンロード中は表示しない
        }
        
        // ダウンロード開始
        self.downloading.insert(url.to_string(), true);
        let url_clone = url.to_string();
        let emoji_tx = self.tx.clone();
        
        // 別スレッドでダウンロード
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
                                        let _ = emoji_tx.send((url_clone.clone(), bytes.to_vec()));
                                    }
                                    Err(e) => {
                                        if debug_mode { eprintln!("Failed to read emoji bytes from {}: {}", url_clone, e); }
                                        // 失敗をキャッシュに記録（空のVecで）
                                        let _ = emoji_tx.send((url_clone.clone(), Vec::new()));
                                    }
                                }
                            } else {
                                if debug_mode { eprintln!("Failed to download emoji (HTTP {}): {}", response.status(), url_clone); }
                                let _ = emoji_tx.send((url_clone.clone(), Vec::new()));
                            }
                        }
                        Err(e) => {
                            if debug_mode { eprintln!("Failed to download emoji from {}: {}", url_clone, e); }
                            let _ = emoji_tx.send((url_clone.clone(), Vec::new()));
                        }
                    }
                }
                Err(e) => {
                    if debug_mode { eprintln!("Failed to create HTTP client: {}", e); }
                    let _ = emoji_tx.send((url_clone.clone(), Vec::new()));
                }
            }
        });
        
        None
    }

    pub fn load_gif_frames(&mut self, ctx: &Context, url: &str, bytes: &[u8]) -> Result<Vec<TextureHandle>, Box<dyn std::error::Error>> {
        use image::AnimationDecoder;
        use image::codecs::gif::GifDecoder;
        use std::io::Cursor;
        
        let cursor = Cursor::new(bytes);
        let decoder = GifDecoder::new(cursor)?;
        let frames = decoder.into_frames().collect_frames()?;
        
        let mut textures = Vec::new();
        let mut frame_durations = Vec::new();
        
        for frame in frames {
            let delay = frame.delay();
            let duration_ms = (delay.numer_denom_ms().0 as f32 / delay.numer_denom_ms().1 as f32) as u32;
            frame_durations.push(duration_ms);
            
            let img = frame.into_buffer();
            let size = [img.width() as usize, img.height() as usize];
            let pixels = img.into_raw();
            let color_image = ColorImage::from_rgba_unmultiplied(size, &pixels);
            
            let texture = ctx.load_texture(
                &format!("{}_frame_{}", url, textures.len()),
                color_image.clone(),
                egui::TextureOptions::LINEAR,
            );
            textures.push(texture);
        }
        
        if !textures.is_empty() {
            self.animated_cache.insert(url.to_string(), AnimatedEmoji {
                frames: Vec::new(),
                frame_durations,
                textures: textures.clone(),
                current_frame: 0,
                elapsed_ms: 0,
            });
        }
        
        Ok(textures)
    }

    pub fn load_apng_frames(&mut self, ctx: &Context, url: &str, bytes: &[u8]) -> Result<Vec<TextureHandle>, Box<dyn std::error::Error>> {
        use image::codecs::png::PngDecoder;
        use image::AnimationDecoder;
        use std::io::Cursor;
        
        let cursor = Cursor::new(bytes);
        let decoder = PngDecoder::new(cursor)?;
        
        // APNGかどうかをチェック
        if !decoder.is_apng()? {
            return Err("Not an APNG file".into());
        }
        
        let frames = decoder.apng()?
            .into_frames()
            .collect_frames()?;
        
        let mut textures = Vec::new();
        let mut frame_durations = Vec::new();
        
        for frame in frames {
            let delay = frame.delay();
            let duration_ms = (delay.numer_denom_ms().0 as f32 / delay.numer_denom_ms().1 as f32) as u32;
            frame_durations.push(duration_ms.max(10)); // 最小10ms
            
            let img = frame.into_buffer();
            let size = [img.width() as usize, img.height() as usize];
            let pixels = img.into_raw();
            let color_image = ColorImage::from_rgba_unmultiplied(size, &pixels);
            
            let texture = ctx.load_texture(
                &format!("{}_frame_{}", url, textures.len()),
                color_image,
                egui::TextureOptions::LINEAR,
            );
            textures.push(texture);
        }
        
        if !textures.is_empty() {
            self.animated_cache.insert(url.to_string(), AnimatedEmoji {
                frames: Vec::new(),
                frame_durations,
                textures: textures.clone(),
                current_frame: 0,
                elapsed_ms: 0,
            });
        }
        
        Ok(textures)
    }

    pub fn update_animations(&mut self, dt_ms: u32) {
        for anim in self.animated_cache.values_mut() {
            anim.elapsed_ms += dt_ms;
            
            if anim.current_frame < anim.frame_durations.len() {
                let current_duration = anim.frame_durations[anim.current_frame];
                if anim.elapsed_ms >= current_duration {
                    anim.elapsed_ms -= current_duration;
                    anim.current_frame = (anim.current_frame + 1) % anim.textures.len();
                }
            }
        }
    }

    pub fn process_downloads(&mut self, ctx: &Context, debug_mode: bool) {
        while let Ok((url, bytes)) = self.rx.try_recv() {
            self.downloading.remove(&url);
            
            // 空のbytes配列は失敗を意味する
            if bytes.is_empty() {
                if debug_mode { eprintln!("Emoji download failed (empty bytes): {}", url); }
                self.static_cache.insert(url, None);
                continue;
            }
            
            // GIFかどうかチェック
            if url.ends_with(".gif") {
                match self.load_gif_frames(ctx, &url, &bytes) {
                    Ok(_frames) => {
                        // 既にload_gif_framesでキャッシュに追加されている
                        ctx.request_repaint(); // 再描画をリクエスト
                        continue;
                    }
                    Err(e) => {
                        if debug_mode { eprintln!("Failed to load GIF emoji {}: {}", url, e); }
                        self.static_cache.insert(url, None);
                        continue;
                    }
                }
            }
            
            // APNGかどうかチェック（PNGマジックナンバーとAPNGシグネチャ）
            if bytes.len() > 8 && &bytes[0..8] == b"\x89PNG\r\n\x1a\n" {
                // APNGとして試行
                match self.load_apng_frames(ctx, &url, &bytes) {
                    Ok(_frames) => {
                        // 既にload_apng_framesでキャッシュに追加されている
                        ctx.request_repaint(); // 再描画をリクエスト
                        continue;
                    }
                    Err(_e) => {
                        // APNGではない、または読み込み失敗 -> 通常のPNGとして処理
                        if debug_mode { eprintln!("Not an APNG or failed to load, treating as static PNG: {}", url); }
                    }
                }
            }
            
            // 通常の画像として読み込み
            match image::load_from_memory(&bytes) {
                Ok(img) => {
                    let size = [img.width() as usize, img.height() as usize];
                    let rgba = img.to_rgba8();
                    let pixels = rgba.as_flat_samples();
                    let color_image = ColorImage::from_rgba_unmultiplied(size, pixels.as_slice());
                    
                    let texture = ctx.load_texture(
                        &url,
                        color_image,
                        egui::TextureOptions::LINEAR
                    );
                    
                    self.static_cache.insert(url, Some(texture));
                    ctx.request_repaint(); // 再描画をリクエスト
                }
                Err(e) => {
                    if debug_mode { eprintln!("Failed to load image emoji {}: {}", url, e); }
                    self.static_cache.insert(url, None);
                }
            }
        }
    }
}
