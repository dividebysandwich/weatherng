use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json, Response},
    routing::{get, post},
};
use chrono::Utc;
use elasticsearch::{Elasticsearch, IndexParts, SearchParts, http::transport::Transport};
use log::{debug, error, info};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt; // Changed from tracing

mod web;

// --- Error Handling ---

pub enum AppError {
    Elastic(elasticsearch::Error),
    Io(std::io::Error),
    LockPoisoned(String),
    MissingData(String),
}

impl From<elasticsearch::Error> for AppError {
    fn from(inner: elasticsearch::Error) -> Self {
        AppError::Elastic(inner)
    }
}
impl From<std::io::Error> for AppError {
    fn from(inner: std::io::Error) -> Self {
        AppError::Io(inner)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            AppError::Elastic(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Elasticsearch error: {}", e),
            ),
            AppError::Io(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("I/O error: {}", e),
            ),
            AppError::LockPoisoned(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Lock error: {}", e),
            ),
            AppError::MissingData(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Data error: {}", e),
            ),
        };
        error!("Server Error: {}", error_message);
        (status, error_message).into_response()
    }
}

// --- Data Models ---

#[derive(Clone)]
struct AppState {
    client: Elasticsearch,
    cache: Arc<RwLock<Option<CacheEntry>>>,
    energy_state: Arc<RwLock<String>>,
}

#[derive(Clone)]
struct CacheEntry {
    data: CachedResult,
    timestamp: Instant,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct CachedResult {
    time: String,
    winddirs: Vec<f64>,
    windspeeds: Vec<f64>,
    windgusts: Vec<f64>,
    rain: Vec<f64>,
    qnh: Vec<f64>,
    temperature: Vec<f64>,
    temperatureindoor: Vec<f64>,
    humidity: Vec<f64>,
    solarradiation: Vec<f64>,

    curwinddir: f64,
    curwindspeed: f64,
    curwindgust: f64,
    currain: f64,
    curqnh: f64,
    curtemperature: f64,
    curtemperatureindoor: f64,
    curhumidity: f64,
    cursolarradiation: f64,
}

// --- Helper Functions ---

fn convert_f_to_c(f: f64) -> f64 {
    (f - 32.0) * 5.0 / 9.0
}

fn halve_resolution(data: &[f64]) -> Vec<f64> {
    data.chunks(4)
        .filter(|chunk| chunk.len() == 4)
        .map(|chunk| chunk.iter().sum::<f64>() / 4.0)
        .collect()
}

// Safely averages wind directions using vector math
fn halve_resolution_degrees(data: &[f64]) -> Vec<f64> {
    data.chunks(4)
        .filter(|chunk| chunk.len() == 4)
        .map(|chunk| {
            let mut sum_sin = 0.0;
            let mut sum_cos = 0.0;

            for &deg in chunk {
                let rad = deg.to_radians();
                sum_sin += rad.sin();
                sum_cos += rad.cos();
            }

            // Calculate the average angle from the summed vectors
            let avg_rad = sum_sin.atan2(sum_cos);
            let mut avg_deg = avg_rad.to_degrees();

            // Ensure the result is between 0 and 360
            if avg_deg < 0.0 {
                avg_deg += 360.0;
            }

            avg_deg.round()
        })
        .collect()
}

async fn write_file(filename: &str, data: String) -> Result<(), AppError> {
    let base_path =
        std::env::var("BASE_PATH").unwrap_or_else(|_| "/var/www/weatherstation".to_string());
    let filepath = format!("{}/{}", base_path, filename);
    fs::write(&filepath, format!("{}\n", data)).await?;
    Ok(())
}

// --- Core Logic ---

async fn query_es(state: &AppState) -> Result<CachedResult, AppError> {
    {
        let cache_lock = state
            .cache
            .read()
            .map_err(|e| AppError::LockPoisoned(e.to_string()))?;
        if let Some(entry) = cache_lock.as_ref() {
            if entry.timestamp.elapsed().as_secs() < 60 {
                debug!("Returning cached Elasticsearch data");
                return Ok(entry.data.clone());
            }
        }
    }

    info!("Querying Elasticsearch for fresh data");

    let query_body = json!({
        "size": 600,
        "sort": [{"time": {"order": "desc"}}],
        "query": { "range": { "time": { "gte": "now-7h" } } }
    });

    let response = state
        .client
        .search(SearchParts::Index(&["weather"]))
        .body(query_body)
        .send()
        .await?;

    let response_body: Value = response.json().await?;
    let hits = response_body["hits"]["hits"]
        .as_array()
        .ok_or_else(|| AppError::MissingData("Malformed hits array in ES response".into()))?;

    let mut result = CachedResult {
        time: Utc::now().to_rfc3339(),
        ..Default::default()
    };

    let mut count = 0;
    for hit in hits {
        if count >= 600 {
            break;
        }
        let source = &hit["_source"];
        if source.get("winddir").is_none() {
            continue;
        }

        result
            .winddirs
            .push(source["winddir"].as_f64().unwrap_or(0.0).round());
        result
            .windspeeds
            .push((source["windspeedkph"].as_f64().unwrap_or(0.0) * 10.0).round() / 10.0);
        result
            .windgusts
            .push((source["windgustkph"].as_f64().unwrap_or(0.0) * 10.0).round() / 10.0);
        result
            .rain
            .push((source["rrain_piezo"].as_f64().unwrap_or(0.0) * 10.0).round() / 10.0);
        result
            .qnh
            .push((source["baromrelhpa"].as_f64().unwrap_or(0.0) * 100.0).round() / 100.0);
        result
            .temperature
            .push((source["tempc"].as_f64().unwrap_or(0.0) * 100.0).round() / 100.0);
        result
            .temperatureindoor
            .push((source["tempinc"].as_f64().unwrap_or(0.0) * 100.0).round() / 100.0);
        result
            .humidity
            .push((source["humidity"].as_f64().unwrap_or(0.0) * 10.0).round() / 10.0);
        result
            .solarradiation
            .push((source["solarradiation"].as_f64().unwrap_or(0.0) * 100.0).round() / 100.0);
        count += 1;
    }

    result.winddirs.reverse();
    result.windspeeds.reverse();
    result.windgusts.reverse();
    result.rain.reverse();
    result.qnh.reverse();
    result.temperature.reverse();
    result.temperatureindoor.reverse();
    result.humidity.reverse();
    result.solarradiation.reverse();

    let base_path =
        std::env::var("BASE_PATH").unwrap_or_else(|_| "/var/www/weatherstation".to_string());

    let mut speed_file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(format!("{}/lastwindspeeds.txt", base_path))
        .await?;
    let mut gust_file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(format!("{}/lastgusts.txt", base_path))
        .await?;
    let mut dir_file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(format!("{}/lastdirections.txt", base_path))
        .await?;

    for (i, &dir) in result.winddirs.iter().enumerate() {
        if i >= 410 {
            dir_file
                .write_all(format!("{:.0}\n", dir).as_bytes())
                .await?;
        }
    }
    for (i, &speed) in result.windspeeds.iter().enumerate() {
        if i >= 410 {
            speed_file
                .write_all(format!("{:.1}\n", speed).as_bytes())
                .await?;
        }
    }
    for (i, &gust) in result.windgusts.iter().enumerate() {
        if i >= 410 {
            gust_file
                .write_all(format!("{:.1}\n", gust).as_bytes())
                .await?;
        }
    }

    if let Some(&last_dir) = result.winddirs.last() {
        result.curwinddir = last_dir;
        write_file("lastdirection.txt", result.curwinddir.to_string()).await?;
    }
    if let Some(&last_speed) = result.windspeeds.last() {
        result.curwindspeed = last_speed;
        write_file("lastwindspeed.txt", format!("{:.1}", result.curwindspeed)).await?;
    }
    if let Some(&last_gust) = result.windgusts.last() {
        result.curwindgust = last_gust;
        write_file("lastgust.txt", format!("{:.1}", result.curwindgust)).await?;
    }
    if let Some(&last_rain) = result.rain.last() {
        result.currain = last_rain;
        write_file("lastrain.txt", result.currain.to_string()).await?;
    }
    if let Some(&last_qnh) = result.qnh.last() {
        result.curqnh = last_qnh;
    }

    if let Some(&last_temp) = result.temperature.last() {
        result.curtemperature = last_temp;
        write_file("lasttemperature.txt", result.curtemperature.to_string()).await?;
    }
    if let Some(&last_itemp) = result.temperatureindoor.last() {
        result.curtemperatureindoor = last_itemp;
        write_file(
            "lastindoortemp.txt",
            result.curtemperatureindoor.to_string(),
        )
        .await?;
    }
    if let Some(&last_hum) = result.humidity.last() {
        result.curhumidity = last_hum;
        write_file("lasthumid.txt", result.curhumidity.to_string()).await?;
    }
    if let Some(&last_solar) = result.solarradiation.last() {
        result.cursolarradiation = last_solar;
    }

    let mut cache_lock = state
        .cache
        .write()
        .map_err(|e| AppError::LockPoisoned(e.to_string()))?;
    *cache_lock = Some(CacheEntry {
        data: result.clone(),
        timestamp: Instant::now(),
    });

    Ok(result)
}

// --- Route Handlers ---

async fn handle_weather_report(
    State(state): State<AppState>,
    body: String,
) -> Result<impl IntoResponse, AppError> {
    info!("POST Request: /weather/report");

    let params: HashMap<String, String> = serde_urlencoded::from_str(&body).unwrap_or_default();
    let mut fieldset: HashMap<String, Value> = HashMap::new();
    let ignore_list = ["PASSKEY", "stationtype", "dateutc", "freq"];

    for (key, val) in &params {
        debug!("{}: {}", key, val);

        if ignore_list.contains(&key.as_str()) {
            continue;
        }

        if let Ok(num_val) = val.parse::<f64>() {
            let mut final_val = num_val;
            let mut final_key = key.clone();

            if key.starts_with("temp") && key.ends_with('f') {
                final_val = convert_f_to_c(num_val);
                final_key = format!("{}c", &key[..key.len() - 1]);
            } else if key.starts_with("barom") && key.ends_with("in") {
                final_val = num_val * 33.6585;
                final_key = format!("{}hpa", &key[..key.len() - 2]);
            } else if key.ends_with("_piezo") || key == "rainratein" {
                final_val = num_val * 25.4;
            } else if key.ends_with("mph") {
                // Convert mph to km/h, then immediately divide by 3.6 to get m/s
                final_val = (num_val * 1.60934) / 3.6;
                final_key = format!("{}kph", &key[..key.len() - 3]);
            } else if key == "maxdailygust" {
                // Same conversion for the max daily gust
                final_val = (num_val * 1.60934) / 3.6;
                final_key = format!("{}kph", key);
            }

            fieldset.insert(final_key, json!(final_val));
        }
    }

    fieldset.insert("time".to_string(), json!(Utc::now().to_rfc3339()));

    state
        .client
        .index(IndexParts::Index("weather"))
        .body(json!(fieldset))
        .send()
        .await?;

    info!("Successfully indexed weather data to Elasticsearch");

    if let Ok(mut cache) = state.cache.write() {
        *cache = None;
    }

    Ok(StatusCode::OK)
}

async fn handle_query_weather(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    info!("GET Request: /queryWeather");
    let cached = query_es(&state).await?;

    let response = json!({
        "time": cached.time,
        // Using the special degree averager to prevent the 350° + 10° = 180° bug
        "winddirs": halve_resolution_degrees(&cached.winddirs).into_iter().skip(13).collect::<Vec<_>>(),
        "windspeeds": halve_resolution(&cached.windspeeds).into_iter().skip(13).collect::<Vec<_>>(),
        "windgusts": halve_resolution(&cached.windgusts).into_iter().skip(13).collect::<Vec<_>>(),
        "rain": halve_resolution(&cached.rain).into_iter().skip(13).collect::<Vec<_>>(),
        "temperature": halve_resolution(&cached.temperature).into_iter().skip(13).collect::<Vec<_>>(),
        "temperatureindoor": halve_resolution(&cached.temperatureindoor).into_iter().skip(13).collect::<Vec<_>>(),
        "curwinddir": cached.curwinddir,
        "curwindspeed": cached.curwindspeed,
        "curwindgust": cached.curwindgust,
        "currain": cached.currain,
        "curqnh": cached.curqnh,
        "curtemperature": cached.curtemperature,
        "curhumidity": cached.curhumidity,
        "cursolarradiation": cached.cursolarradiation,
    });

    Ok(Json(response))
}
async fn handle_query_es(State(state): State<AppState>) -> Result<Json<CachedResult>, AppError> {
    info!("GET Request: /query");
    let cached = query_es(&state).await?;
    Ok(Json(cached))
}

// --- Main Server ---

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize env_logger instead of tracing_subscriber
    dotenvy::dotenv().ok();
    env_logger::init();

    info!("Initializing Elasticsearch connection...");
    let es_url = std::env::var("ES_URL")
        .unwrap_or_else(|_| "http://elastic:elastic@127.0.0.1:9200".to_string());
    let transport = Transport::single_node(&es_url)?;
    let client = Elasticsearch::new(transport);

    let state = AppState {
        client,
        cache: Arc::new(RwLock::new(None)),
        energy_state: Arc::new(RwLock::new("{}".to_string())),
    };

    let context_path = std::env::var("APP_CONTEXT_PATH").unwrap_or_else(|_| "/weather".to_string());
    let context_path = if !context_path.starts_with("/") {
        format!("/{}", context_path)
    } else {
        context_path
    };
    let context_path = if context_path.ends_with("/") && context_path.len() > 1 {
        context_path[..context_path.len() - 1].to_string()
    } else {
        context_path
    };

    let mut app = Router::new()
        .route("/weather/report", post(handle_weather_report))
        .route("/queryWeather", get(handle_query_weather))
        .route("/query", get(handle_query_es))
        .route(
            "/getEnergy",
            get(|State(s): State<AppState>| async move {
                info!("GET Request: /getEnergy");
                let energy = s.energy_state.read().unwrap().clone();
                axum::response::Html(energy)
            }),
        )
        .route(
            "/updateEnergy",
            post(|State(s): State<AppState>, body: String| async move {
                info!("POST Request: /updateEnergy");
                if body.len() > 100 {
                    if let Ok(mut lock) = s.energy_state.write() {
                        *lock = body;
                    }
                }
                StatusCode::OK
            }),
        );

    if context_path == "/" {
        app = app.route("/", get(web::ui_handler));
    } else {
        app = app.route(&context_path, get(web::ui_handler));
        app = app.route(&format!("{}/", context_path), get(web::ui_handler));
    }

    let app = app.with_state(state);

    let listen_uri = std::env::var("LISTEN_URI").unwrap_or_else(|_| "0.0.0.0:8200".to_string());
    let listener = tokio::net::TcpListener::bind(&listen_uri).await?;
    info!("Rust Weather Server running on {}...", listen_uri);

    axum::serve(listener, app).await?;

    Ok(())
}
