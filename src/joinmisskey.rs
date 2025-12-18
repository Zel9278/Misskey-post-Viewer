use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GlobalStats {
    #[serde(rename = "notesCount")]
    pub notes_count: Option<i64>,
    #[serde(rename = "usersCount")]
    pub users_count: Option<i64>,
    pub npd15: Option<f64>,
    #[serde(rename = "druYesterday")]
    pub dru_yesterday: Option<i64>,
    pub dru15: Option<f64>,
    #[serde(rename = "instancesCount")]
    pub instances_count: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InstanceInfo {
    pub url: String,
    pub name: Option<String>,
    pub langs: Option<Vec<String>>,
    pub description: Option<String>,
    #[serde(rename = "isAlive")]
    pub is_alive: Option<bool>,
    pub value: Option<f64>,
    pub banner: Option<bool>,
    pub background: Option<bool>,
    pub icon: Option<bool>,
    pub nodeinfo: Option<serde_json::Value>,
    pub meta: Option<serde_json::Value>,
    pub npd15: Option<f64>,
    #[serde(rename = "druYesterday")]
    pub dru_yesterday: Option<i64>,
    pub dru15: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InstancesResponse {
    pub date: Option<String>,
    pub stats: Option<GlobalStats>,
    pub langs: Option<Vec<String>>,
    #[serde(rename = "instancesInfos")]
    pub instances_infos: Option<Vec<InstanceInfo>>,
}

/// JoinMisskey APIからインスタンス一覧を取得
pub async fn fetch_instances() -> Result<Vec<InstanceInfo>, Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let response = client
        .get("https://instanceapp.misskey.page/instances.json")
        .send()
        .await?;
    
    if !response.status().is_success() {
        return Err(format!("Failed to fetch instances: HTTP {}", response.status()).into());
    }
    
    let data: InstancesResponse = response.json().await?;
    let mut instances = data.instances_infos.unwrap_or_default();
    
    // npd15（15日平均のノート数）とdru15（15日平均のアクティブユーザー数）でソート
    instances.sort_by(|a, b| {
        let score_a = a.npd15.unwrap_or(0.0) + (a.dru15.unwrap_or(0.0) * 10.0);
        let score_b = b.npd15.unwrap_or(0.0) + (b.dru15.unwrap_or(0.0) * 10.0);
        
        score_b.partial_cmp(&score_a).unwrap_or(std::cmp::Ordering::Equal)
    });
    
    // 上位10件のみ返す
    instances.truncate(10);
    
    Ok(instances)
}
