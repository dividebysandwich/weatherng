//! Model Context Protocol (MCP) server exposing this weather station's data.
//!
//! Transport: the classic MCP "HTTP + SSE" transport (protocol revision
//! `2024-11-05`), which is what the user requested.
//!
//!   1. The client opens a long-lived `GET /mcp/sse` stream. The first event we
//!      emit is an `endpoint` event whose data is the URL the client must POST
//!      its JSON-RPC requests to (it carries the per-connection `sessionId`).
//!   2. The client POSTs JSON-RPC 2.0 messages to `POST /mcp/message?sessionId=…`.
//!      We acknowledge with `202 Accepted` and deliver the actual JSON-RPC
//!      response asynchronously back over that session's SSE stream as a
//!      `message` event.
//!
//! The three data domains this station owns are surfaced as three tools with
//! self-describing names, descriptions, and field names (including physical
//! units) so any MCP client / LLM can interpret them without external context:
//!
//!   * `get_current_live_weather_observation`      — the latest measured reading
//!   * `get_historical_measured_weather_timeseries` — the last ~7 hours measured
//!   * `get_hourly_weather_forecast`               — next 48 h modeled forecast

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::{
        IntoResponse,
        sse::{Event, KeepAlive, Sse},
    },
};
use chrono::Utc;
use log::info;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_stream::Stream;

use crate::{AppError, AppState, query_es, query_forecast};

/// Protocol revision implemented by this server (the SSE transport revision).
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Per-connection SSE senders, keyed by `sessionId`. A POST handler looks up the
/// session here and pushes the JSON-RPC response onto the matching SSE stream.
pub type McpSessions = Arc<RwLock<HashMap<String, UnboundedSender<Result<Event, Infallible>>>>>;

// --- SSE stream plumbing ---------------------------------------------------

/// Removes a session from the registry when its SSE stream is dropped (i.e. the
/// client disconnected), so the map does not leak senders for dead connections.
struct SessionGuard {
    sessions: McpSessions,
    session_id: String,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if let Ok(mut map) = self.sessions.write() {
            map.remove(&self.session_id);
        }
        info!("MCP SSE session closed: {}", self.session_id);
    }
}

/// An SSE event stream that owns a [`SessionGuard`], tying session cleanup to
/// the lifetime of the stream. Implemented directly over the mpsc receiver so we
/// don't need the `tokio-stream` wrapper feature.
struct GuardedEventStream {
    rx: UnboundedReceiver<Result<Event, Infallible>>,
    _guard: SessionGuard,
}

impl Stream for GuardedEventStream {
    type Item = Result<Event, Infallible>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // `Self` is `Unpin` (all fields are), so projecting through `get_mut` is sound.
        self.get_mut().rx.poll_recv(cx)
    }
}

// --- HTTP handlers ---------------------------------------------------------

/// `GET /mcp/sse` — establish an MCP SSE session and announce the POST endpoint.
pub async fn mcp_sse_handler(State(state): State<AppState>) -> impl IntoResponse {
    let counter = state.mcp_session_counter.fetch_add(1, Ordering::Relaxed);
    let session_id = format!("{}-{}", Utc::now().timestamp_millis(), counter);
    info!("MCP SSE session opened: {}", session_id);

    let (tx, rx) = mpsc::unbounded_channel::<Result<Event, Infallible>>();

    // Per the SSE transport, the very first event tells the client where to POST.
    // A root-relative URL is resolved by the client against the SSE origin.
    let endpoint = format!("/mcp/message?sessionId={}", session_id);
    let _ = tx.send(Ok(Event::default().event("endpoint").data(endpoint)));

    state
        .mcp_sessions
        .write()
        .expect("mcp_sessions lock poisoned")
        .insert(session_id.clone(), tx);

    let stream = GuardedEventStream {
        rx,
        _guard: SessionGuard {
            sessions: state.mcp_sessions.clone(),
            session_id,
        },
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Deserialize)]
pub struct MessageQuery {
    #[serde(rename = "sessionId")]
    session_id: String,
}

/// `POST /mcp/message?sessionId=…` — receive JSON-RPC, reply over the SSE stream.
pub async fn mcp_message_handler(
    State(state): State<AppState>,
    Query(query): Query<MessageQuery>,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    // Clone the sender out of the lock so we never hold the guard across an await.
    let sender = state
        .mcp_sessions
        .read()
        .expect("mcp_sessions lock poisoned")
        .get(&query.session_id)
        .cloned();

    let Some(sender) = sender else {
        return (StatusCode::NOT_FOUND, "Unknown or expired MCP sessionId").into_response();
    };

    // A POST may carry a single JSON-RPC message or a batch (array).
    let messages = match payload {
        Value::Array(batch) => batch,
        single => vec![single],
    };

    for message in messages {
        if let Some(response) = handle_jsonrpc(&state, message).await {
            let data = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
            // If the receiver is gone the client disconnected; nothing to do.
            let _ = sender.send(Ok(Event::default().event("message").data(data)));
        }
    }

    StatusCode::ACCEPTED.into_response()
}

// --- JSON-RPC dispatch -----------------------------------------------------

/// Handle one JSON-RPC message. Returns `Some(response)` for requests (which
/// carry an `id`) and `None` for notifications (which must not be answered).
async fn handle_jsonrpc(state: &AppState, message: Value) -> Option<Value> {
    let method = message.get("method").and_then(Value::as_str)?;
    let id = message.get("id").cloned();
    let params = message.get("params").cloned().unwrap_or_else(|| json!({}));

    match method {
        "initialize" => {
            let id = id?;
            // Echo the client's requested protocol version when we recognise it,
            // otherwise advertise the revision we implement.
            let protocol_version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(MCP_PROTOCOL_VERSION)
                .to_string();
            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": protocol_version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": {
                        "name": "weatherng-station-mcp",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "instructions": "Tools expose one physical weather station's \
                        measured live and historical data plus a modeled hourly \
                        forecast for the station's location. All field names carry \
                        explicit physical units."
                }
            }))
        }

        // Notifications — acknowledged by absence of a response.
        "notifications/initialized" | "notifications/cancelled" => None,

        "ping" => Some(json!({ "jsonrpc": "2.0", "id": id?, "result": {} })),

        "tools/list" => Some(json!({
            "jsonrpc": "2.0",
            "id": id?,
            "result": { "tools": tools_list(state) }
        })),

        "tools/call" => {
            let id = id?;
            let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
            let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let response = match call_tool(state, name, &arguments).await {
                Ok(payload) => {
                    let text =
                        serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
                    json!({
                        "content": [{ "type": "text", "text": text }],
                        "isError": false
                    })
                }
                Err(err) => json!({
                    "content": [{ "type": "text", "text": format!("Tool error: {}", err) }],
                    "isError": true
                }),
            };
            Some(json!({ "jsonrpc": "2.0", "id": id, "result": response }))
        }

        _ => id.map(|id| {
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("Method not found: {}", method) }
            })
        }),
    }
}

// --- Tool catalog ----------------------------------------------------------

/// JSON Schema for tools that take no arguments.
fn no_arguments_schema() -> Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

fn tools_list(state: &AppState) -> Vec<Value> {
    let location = location_description(state);
    vec![
        json!({
            "name": "get_current_live_weather_observation",
            "description": format!(
                "Return the single most recent live weather measurement from the physical \
                 weather station located at {location}. Values are the latest readings from \
                 the station's own sensors (NOT a forecast). Includes outdoor and indoor \
                 temperature (°C), relative humidity (%), barometric pressure reduced to sea \
                 level / QNH (hPa), wind speed and gust (km/h), wind direction (degrees, \
                 meteorological, 0=N/90=E/180=S/270=W), rainfall (mm) and solar radiation \
                 (W/m²). Takes no arguments."
            ),
            "inputSchema": no_arguments_schema(),
        }),
        json!({
            "name": "get_historical_measured_weather_timeseries",
            "description": format!(
                "Return the recent measured-weather time series (approximately the last 7 \
                 hours, up to 600 samples taken roughly every 2 minutes) from the physical \
                 weather station at {location}. These are real sensor measurements, NOT a \
                 forecast. Each variable is returned as a parallel array ordered oldest-first; \
                 element i of every array corresponds to the same sample, and the last element \
                 is the most recent reading. Units: temperatures in °C, humidity in %, pressure \
                 (QNH) in hPa, wind speed/gust in km/h, wind direction in meteorological \
                 degrees, rainfall in mm, solar radiation in W/m². Takes no arguments."
            ),
            "inputSchema": no_arguments_schema(),
        }),
        json!({
            "name": "get_hourly_weather_forecast",
            "description": format!(
                "Return the modeled hourly weather forecast for the next 48 hours at the \
                 weather station's location ({location}), sourced from the Open-Meteo numerical \
                 weather model. This is a FORECAST (predicted values), not measured data. \
                 Returns an array of hourly entries, each with a UTC timestamp and: forecast \
                 air temperature at 2 m (°C), precipitation (mm), wind speed and gusts at 10 m \
                 (km/h), wind direction at 10 m (meteorological degrees) and total cloud cover \
                 (%). Takes no arguments. Requires the station's latitude/longitude to be \
                 configured on the server."
            ),
            "inputSchema": no_arguments_schema(),
        }),
    ]
}

fn location_description(state: &AppState) -> String {
    match (state.lat, state.lon) {
        (Some(lat), Some(lon)) => format!("latitude {lat:.5}, longitude {lon:.5}"),
        _ => "the station's configured location".to_string(),
    }
}

// --- Tool implementations --------------------------------------------------

async fn call_tool(state: &AppState, name: &str, _arguments: &Value) -> Result<Value, AppError> {
    match name {
        "get_current_live_weather_observation" => tool_current_observation(state).await,
        "get_historical_measured_weather_timeseries" => tool_historical_timeseries(state).await,
        "get_hourly_weather_forecast" => tool_hourly_forecast(state).await,
        other => Err(AppError::MissingData(format!("Unknown tool: {}", other))),
    }
}

async fn tool_current_observation(state: &AppState) -> Result<Value, AppError> {
    let data = query_es(state).await?;
    Ok(json!({
        "location": {
            "latitude_degrees": state.lat,
            "longitude_degrees": state.lon,
        },
        "data_retrieved_at_utc": data.time,
        "measurement_kind": "live_sensor_observation",
        "observation": {
            "outdoor_temperature_celsius": data.curtemperature,
            "indoor_temperature_celsius": data.curtemperatureindoor,
            "relative_humidity_percent": data.curhumidity,
            "barometric_pressure_sea_level_qnh_hpa": data.curqnh,
            "wind_speed_kmh": data.curwindspeed,
            "wind_gust_kmh": data.curwindgust,
            "wind_direction_meteorological_degrees": data.curwinddir,
            "rainfall_mm": data.currain,
            "solar_radiation_watts_per_square_meter": data.cursolarradiation,
        }
    }))
}

async fn tool_historical_timeseries(state: &AppState) -> Result<Value, AppError> {
    let data = query_es(state).await?;
    Ok(json!({
        "location": {
            "latitude_degrees": state.lat,
            "longitude_degrees": state.lon,
        },
        "data_retrieved_at_utc": data.time,
        "measurement_kind": "live_sensor_observation",
        "sampling": {
            "window": "approximately_last_7_hours",
            "approximate_interval_seconds": 120,
            "ordering": "oldest_first_last_element_is_most_recent",
            "sample_count": data.temperature.len(),
        },
        "series": {
            "outdoor_temperature_celsius": data.temperature,
            "indoor_temperature_celsius": data.temperatureindoor,
            "relative_humidity_percent": data.humidity,
            "barometric_pressure_sea_level_qnh_hpa": data.qnh,
            "wind_speed_kmh": data.windspeeds,
            "wind_gust_kmh": data.windgusts,
            "wind_direction_meteorological_degrees": data.winddirs,
            "rainfall_mm": data.rain,
            "solar_radiation_watts_per_square_meter": data.solarradiation,
        }
    }))
}

async fn tool_hourly_forecast(state: &AppState) -> Result<Value, AppError> {
    let forecast = query_forecast(state).await?;
    let hourly = &forecast.hourly;

    // Re-shape the parallel arrays into self-describing per-hour objects.
    let hours: Vec<Value> = hourly
        .time
        .iter()
        .enumerate()
        .map(|(i, timestamp)| {
            json!({
                "time_utc": timestamp,
                "forecast_air_temperature_2m_celsius": hourly.temperature_2m.get(i),
                "forecast_precipitation_mm": hourly.precipitation.get(i),
                "forecast_wind_speed_10m_kmh": hourly.wind_speed_10m.get(i),
                "forecast_wind_gust_10m_kmh": hourly.wind_gusts_10m.get(i),
                "forecast_wind_direction_10m_meteorological_degrees": hourly.wind_direction_10m.get(i),
                "forecast_cloud_cover_percent": hourly.cloud_cover.get(i),
            })
        })
        .collect();

    Ok(json!({
        "location": {
            "latitude_degrees": state.lat,
            "longitude_degrees": state.lon,
        },
        "measurement_kind": "numerical_model_forecast",
        "forecast_source": "open-meteo",
        "timezone": "UTC",
        "hour_count": hours.len(),
        "hours": hours,
    }))
}
