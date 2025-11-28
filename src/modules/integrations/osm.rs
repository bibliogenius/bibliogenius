use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Serialize, Deserialize)]
pub struct OsmNode {
    pub id: i64,
    pub lat: f64,
    pub lon: f64,
    pub tags: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OverpassResponse {
    pub elements: Vec<OsmNode>,
}

pub async fn find_nearby_libraries(
    lat: f64,
    lon: f64,
    radius_meters: u32,
) -> Result<Vec<OsmNode>, String> {
    let query = format!(
        r#"[out:json];node(around:{},{},{})["amenity"="library"];out;"#,
        radius_meters, lat, lon
    );
    query_overpass(&query).await
}

pub async fn find_nearby_bookstores(
    lat: f64,
    lon: f64,
    radius_meters: u32,
) -> Result<Vec<OsmNode>, String> {
    let query = format!(
        r#"[out:json];node(around:{},{},{})["shop"="books"];out;"#,
        radius_meters, lat, lon
    );
    query_overpass(&query).await
}

async fn query_overpass(query: &str) -> Result<Vec<OsmNode>, String> {
    let client = reqwest::Client::new();
    let res = client
        .post("https://overpass-api.de/api/interpreter")
        .body(query.to_string())
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !res.status().is_success() {
        return Err(format!("Overpass API error: {}", res.status()));
    }

    let data: OverpassResponse = res.json().await.map_err(|e| e.to_string())?;
    Ok(data.elements)
}
