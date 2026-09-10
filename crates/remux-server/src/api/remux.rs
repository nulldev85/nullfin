use anyhow::Result as AnyResult;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Duration, Utc};
use remux_macros::{delete, get, post, query};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};

use crate::{
    AppState, OptionExt,
    db::{self, auth},
    sdks,
};
use axum_anyhow::ApiResult as Result;
use uuid::Uuid;

const CACHE_KEY_PREFIX: &str = "remux:cache:";

#[query]
#[derive(Debug, Default)]
pub struct NamespaceQuery {
    #[serde(default)]
    pub ns: String,
}

#[derive(Debug, Deserialize)]
pub struct CacheSetRequest {
    #[serde(rename = "Value", alias = "value")]
    pub value: String,
    #[serde(rename = "TtlSeconds", alias = "ttlSeconds", default)]
    pub ttl_seconds: i64,
}

#[derive(Debug, Deserialize, Default)]
pub struct CacheBulkRequest {
    #[serde(default, alias = "Keys")]
    pub keys: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct CacheGetResponse {
    pub value: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct CacheStatsResponse {
    pub total_keys: usize,
    pub active_keys: usize,
    pub expired_keys: usize,
    pub namespaces: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheRecord {
    value: String,
    expires_at: Option<DateTime<Utc>>,
}

fn encode_component(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn decode_component(value: &str) -> String {
    let wrapped = format!("v={value}");
    url::form_urlencoded::parse(wrapped.as_bytes())
        .find_map(|(k, v)| if k == "v" { Some(v.into_owned()) } else { None })
        .unwrap_or_else(|| value.to_string())
}

fn cache_storage_key(ns: &str, key: &str) -> String {
    format!(
        "{CACHE_KEY_PREFIX}{}:{}",
        encode_component(ns),
        encode_component(key)
    )
}

fn parse_cache_storage_key(storage_key: &str) -> Option<(String, String)> {
    if !storage_key.starts_with(CACHE_KEY_PREFIX) {
        return None;
    }

    let rest = &storage_key[CACHE_KEY_PREFIX.len()..];
    let (ns_enc, key_enc) = rest.split_once(':')?;
    Some((decode_component(ns_enc), decode_component(key_enc)))
}

fn normalize_registration_id(id: &str) -> String {
    id.strip_prefix("request-")
        .unwrap_or(id)
        .to_string()
}

fn compute_expiration(ttl_seconds: i64) -> Option<DateTime<Utc>> {
    if ttl_seconds <= 0 {
        return None;
    }

    // Clamp to avoid pathological values.
    let ttl = ttl_seconds.clamp(1, 60 * 60 * 24 * 365 * 10);
    Utc::now().checked_add_signed(Duration::seconds(ttl))
}

fn is_expired(record: &CacheRecord) -> bool {
    match record.expires_at {
        Some(ts) => ts <= Utc::now(),
        None => false,
    }
}

async fn delete_cache_record(
    db_pool: &sqlx::SqlitePool,
    ns: &str,
    key: &str,
) -> AnyResult<()> {
    let storage_key = cache_storage_key(ns, key);
    sqlx::query("DELETE FROM settings WHERE key = ?1")
        .bind(storage_key)
        .execute(db_pool)
        .await?;
    Ok(())
}

async fn load_cache_record(
    db_pool: &sqlx::SqlitePool,
    ns: &str,
    key: &str,
) -> AnyResult<Option<CacheRecord>> {
    let storage_key = cache_storage_key(ns, key);
    let Some(raw) = db::Settings::get(db_pool, &storage_key).await? else {
        return Ok(None);
    };

    let record = match serde_json::from_str::<CacheRecord>(&raw) {
        Ok(parsed) => parsed,
        Err(_) => CacheRecord {
            value: raw,
            expires_at: None,
        },
    };

    if is_expired(&record) {
        delete_cache_record(db_pool, ns, key).await?;
        return Ok(None);
    }

    Ok(Some(record))
}

async fn save_cache_record(
    db_pool: &sqlx::SqlitePool,
    ns: &str,
    key: &str,
    value: &str,
    ttl_seconds: i64,
) -> AnyResult<()> {
    let record = CacheRecord {
        value: value.to_string(),
        expires_at: compute_expiration(ttl_seconds),
    };

    let storage_key = cache_storage_key(ns, key);
    let raw = serde_json::to_string(&record)?;
    db::Settings::set(db_pool, &storage_key, &raw).await
}

#[get("/remux/cache/stats")]
pub async fn remux_cache_stats(
    State(state): State<AppState>,
    _session: auth::AuthSession,
) -> Result<Response> {
    let pattern = format!("{CACHE_KEY_PREFIX}%");
    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT key, value FROM settings WHERE key LIKE ?1",
    )
    .bind(pattern)
    .fetch_all(
        &state
            .ctx
            .db,
    )
    .await?;

    let mut namespaces = HashSet::new();
    let mut active_keys = 0usize;
    let mut expired_keys = 0usize;
    let mut expired_storage_keys = Vec::new();

    for (storage_key, raw_value) in &rows {
        if let Some((ns, _)) = parse_cache_storage_key(storage_key) {
            namespaces.insert(ns);
        }

        let parsed = serde_json::from_str::<CacheRecord>(raw_value).ok();
        let expired = parsed
            .as_ref()
            .map(is_expired)
            .unwrap_or(false);

        if expired {
            expired_keys += 1;
            expired_storage_keys.push(storage_key.clone());
        } else {
            active_keys += 1;
        }
    }

    for storage_key in expired_storage_keys {
        sqlx::query("DELETE FROM settings WHERE key = ?1")
            .bind(storage_key)
            .execute(
                &state
                    .ctx
                    .db,
            )
            .await?;
    }

    let payload = CacheStatsResponse {
        total_keys: rows.len(),
        active_keys,
        expired_keys,
        namespaces: namespaces.len(),
    };

    Ok((StatusCode::OK, Json(payload)).into_response())
}

#[get("/remux/cache/{key}")]
pub async fn remux_cache_get(
    State(state): State<AppState>,
    _session: auth::AuthSession,
    Path(key): Path<String>,
    Query(query): Query<NamespaceQuery>,
) -> Result<Response> {
    let ns = query.ns;

    let record = load_cache_record(
        &state
            .ctx
            .db,
        &ns,
        &key,
    )
    .await?
    .context_not_found("not found")?;

    Ok((
        StatusCode::OK,
        Json(CacheGetResponse {
            value: record.value,
        }),
    )
        .into_response())
}

#[post("/remux/cache/{key}")]
pub async fn remux_cache_set(
    State(state): State<AppState>,
    _session: auth::AuthSession,
    Path(key): Path<String>,
    Query(query): Query<NamespaceQuery>,
    Json(payload): Json<CacheSetRequest>,
) -> Result<Response> {
    save_cache_record(
        &state
            .ctx
            .db,
        &query.ns,
        &key,
        &payload.value,
        payload.ttl_seconds,
    )
    .await?;

    Ok(StatusCode::NO_CONTENT.into_response())
}

#[delete("/remux/cache/{key}")]
pub async fn remux_cache_delete(
    State(state): State<AppState>,
    _session: auth::AuthSession,
    Path(key): Path<String>,
    Query(query): Query<NamespaceQuery>,
) -> Result<Response> {
    delete_cache_record(
        &state
            .ctx
            .db,
        &query.ns,
        &key,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[post("/remux/cache/bulk")]
pub async fn remux_cache_bulk(
    State(state): State<AppState>,
    _session: auth::AuthSession,
    Query(query): Query<NamespaceQuery>,
    Json(payload): Json<CacheBulkRequest>,
) -> Result<Response> {
    let mut out = HashMap::<String, String>::new();

    for key in payload.keys {
        if let Some(record) = load_cache_record(
            &state
                .ctx
                .db,
            &query.ns,
            &key,
        )
        .await?
        {
            out.insert(key, record.value);
        }
    }

    Ok((StatusCode::OK, Json(out)).into_response())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct StreamMetadataDto {
    pub id: Uuid,
    pub name: Option<String>,
    pub description: Option<String>,
    pub index: i64,
    pub size: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct StreamsResponse {
    pub streams: Vec<StreamMetadataDto>,
}

fn source_to_dto(s: &db::Media) -> StreamMetadataDto {
    // Title in the DB carries the merged "name\ndescription" string —
    // split it back so legacy clients that only render `Name` still
    // see something useful and ones that consume both get the pair.
    let (name, description) = match s
        .title
        .split_once('\n')
    {
        Some((n, d)) => (
            Some(
                n.trim()
                    .to_string(),
            ),
            Some(
                d.trim()
                    .to_string(),
            ),
        ),
        None if !s
            .title
            .is_empty() =>
        {
            (
                Some(
                    s.title
                        .clone(),
                ),
                None,
            )
        }
        _ => (None, None),
    };
    StreamMetadataDto {
        id: s.id,
        name,
        description,
        index: s
            .idx
            .unwrap_or(0),
        size: s
            .probe_data
            .as_ref()
            .and_then(|p| p.size),
    }
}

/// Source-level metadata (binge group, name, description) for an item.
/// Returns stream metadata from `/gelato/streams/{id}` so the
/// client can match versions across episodes without re-issuing playback info.
async fn streams_metadata(state: &AppState, id: Uuid) -> AnyResult<StreamsResponse> {
    let Some(media) = db::Media::get_by_id(
        &state
            .ctx
            .db,
        &id,
    )
    .await?
    else {
        return Ok(StreamsResponse { streams: vec![] });
    };

    // Accept both the parent (Movie/Episode) and the Source itself.
    let mut parent = if media.kind == db::MediaKind::Stream {
        media
            .parent(
                &state
                    .ctx
                    .db,
            )
            .await?
            .unwrap_or(media)
    } else {
        media
    };

    if !matches!(
        parent.kind,
        db::MediaKind::Movie | db::MediaKind::Episode | db::MediaKind::Track
    ) {
        return Ok(StreamsResponse { streams: vec![] });
    }

    let mut sources = parent
        .streams(
            &state
                .ctx
                .db,
        )
        .await
        .unwrap_or_default();
    sources.sort_by_key(|s| {
        s.idx
            .unwrap_or(0)
    });

    let groups = db::StreamGroup::list(
        &state
            .ctx
            .db,
    )
    .await
    .unwrap_or_default();
    let enabled_groups: Vec<&db::StreamGroup> = groups
        .iter()
        .filter(|g| g.enabled)
        .collect();

    if enabled_groups.is_empty() {
        let streams = sources
            .iter()
            .map(source_to_dto)
            .collect();
        return Ok(StreamsResponse { streams });
    }

    let config = db::Settings::get_config_or_default(
        &state
            .ctx
            .db,
    )
    .await;
    let show_ungrouped = config
        .stream_groups_show_ungrouped
        .unwrap_or(true);

    let mut result: Vec<StreamMetadataDto> = vec![];
    let mut matched_ids: HashSet<Uuid> = HashSet::new();

    for group in &enabled_groups {
        let matching: Vec<&db::Media> = sources
            .iter()
            .filter(|s| {
                s.stream_info
                    .as_ref()
                    .map_or(false, |info| {
                        group.match_outcome(
                            info,
                            s.probe_data
                                .as_ref(),
                        ) == db::MatchOutcome::Match
                    })
            })
            .collect();

        if matching.is_empty() {
            continue;
        }

        for s in &matching {
            matched_ids.insert(s.id);
        }

        let best = matching[0];
        let description = {
            use remux_sdks::remux::{StreamRule, format_size_rule, language_label};
            let parts: Vec<String> = group
                .filter
                .rules
                .iter()
                .map(|r| match r {
                    StreamRule::Resolution { values, .. } => values
                        .iter()
                        .map(|v| v.label())
                        .collect::<Vec<_>>()
                        .join("/"),
                    StreamRule::Quality { values, .. } => values
                        .iter()
                        .map(|v| v.label())
                        .collect::<Vec<_>>()
                        .join("/"),
                    StreamRule::Codec { values, .. } => values
                        .iter()
                        .map(|v| v.label())
                        .collect::<Vec<_>>()
                        .join("/"),
                    StreamRule::AudioLanguage { values, .. } => values
                        .iter()
                        .map(|c| language_label(c))
                        .collect::<Vec<_>>()
                        .join("/"),
                    StreamRule::Size { op, value } => format_size_rule(*op, *value),
                })
                .filter(|s| !s.is_empty())
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join(" · "))
            }
        };
        result.push(StreamMetadataDto {
            id: best.id,
            name: Some(group.display_name()),
            description,
            index: result.len() as i64,
            size: best
                .probe_data
                .as_ref()
                .and_then(|p| p.size),
        });
    }

    if show_ungrouped {
        for s in &sources {
            if !matched_ids.contains(&s.id) {
                result.push(source_to_dto(s));
            }
        }
    }

    Ok(StreamsResponse { streams: result })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct MetricsStatusResponse {
    pub daily_days: i64,
    pub last_updated_days_ago: Option<i64>,
    pub item_count: i64,
}

#[get("/remux/metrics/status")]
pub async fn remux_metrics_status(
    State(state): State<AppState>,
    _session: auth::AuthSession,
) -> Result<impl IntoResponse> {
    let row = sqlx::query_as::<_, (i64, Option<i64>, i64)>(
        "SELECT COUNT(DISTINCT period_key), \
                CAST(julianday('now') - julianday(MAX(period_key)) AS INTEGER), \
                COUNT(DISTINCT media_id) \
         FROM popularity_agg \
         WHERE period = 'daily'",
    )
    .fetch_one(
        &state
            .ctx
            .db,
    )
    .await?;

    Ok(Json(MetricsStatusResponse {
        daily_days: row.0,
        last_updated_days_ago: row.1,
        item_count: row.2,
    }))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ItemMetric {
    pub source: String,
    pub value: f64,
    pub date: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ItemMetricsResponse {
    pub metrics: Vec<ItemMetric>,
}

/// Each source's most recent score for one item.
///
/// A source is sampled daily, so the newest row per source is the current
/// score; the older rows stay for the aggregate and are not wanted here.
async fn latest_metrics(
    db: &sqlx::SqlitePool,
    id: Uuid,
) -> sqlx::Result<Vec<ItemMetric>> {
    let rows = sqlx::query_as::<_, (String, f64, String)>(
        "SELECT p.source, p.value, p.date \
         FROM popularity_raw p \
         WHERE p.media_id = ? \
           AND p.date = (SELECT MAX(q.date) FROM popularity_raw q \
                         WHERE q.media_id = p.media_id AND q.source = p.source) \
         ORDER BY p.source",
    )
    .bind(id)
    .fetch_all(db)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(source, value, date)| ItemMetric {
            source,
            value,
            date,
        })
        .collect())
}

/// Every score a metrics addon holds for one item, one row per source.
///
/// The values were already being collected -- each `MetricsAddon` writes a
/// source and a 0-100 value into `popularity_raw` -- but until now they were
/// only ever read back averaged into `popularity_agg`, which exists to sort
/// shelves. A client that wants to show where a score came from needs them
/// unaveraged.
#[get("/remux/metrics/{id}")]
pub async fn remux_item_metrics(
    State(state): State<AppState>,
    _session: auth::AuthSession,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse> {
    Ok(Json(ItemMetricsResponse {
        metrics: latest_metrics(
            &state
                .ctx
                .db,
            id,
        )
        .await?,
    }))
}

#[get("/remux/streams/{id}")]
pub async fn remux_streams(
    State(state): State<AppState>,
    _session: auth::AuthSession,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse> {
    let payload = streams_metadata(&state, id)
        .await
        .unwrap_or(StreamsResponse { streams: vec![] });
    Ok(Json(payload))
}

#[get("/remux/meta/{kind}/{id}")]
pub async fn remux_meta(
    State(state): State<AppState>,
    _session: auth::AuthSession,
    Path((kind, id)): Path<(String, String)>,
) -> Result<impl IntoResponse> {
    let media_type = match kind.as_str() {
        "series" => remux_sdks::stremio::MediaType::Series,
        "movie" => remux_sdks::stremio::MediaType::Movie,
        _ => remux_sdks::stremio::MediaType::Movie,
    };

    let manifest_url = match crate::addons::Addon::list(
        &state
            .ctx
            .db,
    )
    .await
    {
        Ok(addons) => addons
            .into_iter()
            .filter(|a| {
                a.enabled
                    && a.preset
                        .kind
                        == "stremio"
            })
            .find_map(|a| {
                a.preset
                    .config
                    .expose()
                    .get("manifest_url")
                    .and_then(|v| {
                        v.as_str()
                            .map(str::to_string)
                    })
            }),
        Err(_) => None,
    };

    let svc = match manifest_url
        .as_deref()
        .and_then(|u| crate::services::stremio::StremioService::from_url(u).ok())
    {
        Some(a) => a,
        None => {
            return Ok(
                (StatusCode::NOT_FOUND, Json(serde_json::Value::Null)).into_response()
            );
        }
    };

    match svc
        .get_meta(media_type, id)
        .await
    {
        Ok(meta) => Ok(Json::<remux_sdks::stremio::Meta>(meta).into_response()),
        Err(_) => {
            Ok((StatusCode::NOT_FOUND, Json(serde_json::Value::Null)).into_response())
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use sqlx::SqlitePool;

    async fn test_db() -> SqlitePool {
        let db = db::connect("sqlite::memory:", 10_000)
            .await
            .unwrap();
        db::migrate(&db)
            .await
            .unwrap();
        db
    }

    async fn insert_metric(
        db: &SqlitePool,
        source: &str,
        media_id: Uuid,
        value: f64,
        date: &str,
    ) {
        sqlx::query(
            "INSERT INTO popularity_raw (source, external_id, media_id, value, date) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(source)
        // The table is keyed by (source, external_id, date), so each item needs
        // an external id of its own; media_id is the later back-reference.
        .bind(media_id.to_string())
        .bind(media_id)
        .bind(value)
        .bind(date)
        .execute(db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn latest_metrics_returns_one_current_row_per_source() {
        let db = test_db().await;
        let item = Uuid::new_v4();

        insert_metric(&db, "trakt", item, 61.0, "2026-09-08").await;
        insert_metric(&db, "trakt", item, 78.0, "2026-09-09").await;
        insert_metric(&db, "tmdb", item, 80.0, "2026-09-09").await;

        let metrics = latest_metrics(&db, item)
            .await
            .unwrap();

        // One row per source, each carrying that source's newest sample.
        assert_eq!(metrics.len(), 2);
        assert_eq!(metrics[0].source, "tmdb");
        assert_eq!(metrics[0].value, 80.0);
        assert_eq!(metrics[1].source, "trakt");
        assert_eq!(metrics[1].value, 78.0);
        assert_eq!(metrics[1].date, "2026-09-09");
    }

    #[tokio::test]
    async fn latest_metrics_keeps_items_apart_and_answers_empty_for_unknown_ones() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let other = Uuid::new_v4();

        insert_metric(&db, "tmdb", item, 80.0, "2026-09-09").await;
        insert_metric(&db, "tmdb", other, 12.0, "2026-09-09").await;

        let metrics = latest_metrics(&db, item)
            .await
            .unwrap();
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].value, 80.0);

        assert!(
            latest_metrics(&db, Uuid::new_v4())
                .await
                .unwrap()
                .is_empty()
        );
    }
}
