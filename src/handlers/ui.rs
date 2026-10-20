//! The embedded category manager UI and its JSON API.

use crate::handlers::{json_error, Rsp};
use crate::state::{AppState, CATEGORY_TYPES};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

const UI_HTML: &str = include_str!("../../assets/ui.html");

/// One row of the category manager.
#[derive(Serialize)]
pub struct CategoryInfo {
    pub id: String,
    pub name: String,
    pub r#type: String,
    pub enabled: bool,
}

#[derive(Serialize)]
pub struct CategoriesResponse {
    pub live: Vec<CategoryInfo>,
    pub movies: Vec<CategoryInfo>,
    pub series: Vec<CategoryInfo>,
}

/// Body of `POST /api/categories`: `{ "enabled": { "live": { "12": true } } }`.
#[derive(Deserialize)]
pub struct UpdateCategoriesRequest {
    pub enabled: BTreeMap<String, BTreeMap<String, bool>>,
}

/// Serves the single-page UI.
pub async fn serve_web_ui() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        UI_HTML,
    )
}

/// Lists every provider category with its current on/off state.
pub async fn get_categories(State(state): State<Arc<AppState>>, _request: Request) -> Rsp {
    let Some(client) = state.xtream.as_ref() else {
        return json_error(StatusCode::BAD_REQUEST, "Xtream backend is not configured");
    };

    let enabled = state.filter.snapshot();

    let live = match crate::generate::categories(&state, client, "live").await {
        Ok(categories) => into_infos(categories, "live", &enabled),
        Err(err) => {
            tracing::warn!("unable to fetch live categories: {err}");
            Vec::new()
        }
    };
    let movies = match crate::generate::categories(&state, client, "vod").await {
        Ok(categories) => into_infos(categories, "movies", &enabled),
        Err(err) => {
            tracing::warn!("unable to fetch vod categories: {err}");
            Vec::new()
        }
    };
    let series = match crate::generate::categories(&state, client, "series").await {
        Ok(categories) => into_infos(categories, "series", &enabled),
        Err(err) => {
            tracing::warn!("unable to fetch series categories: {err}");
            Vec::new()
        }
    };

    json(
        StatusCode::OK,
        &CategoriesResponse {
            live,
            movies,
            series,
        },
    )
}

/// Stores the enabled categories, persists them, and drops every cached playlist so the next
/// download reflects the new selection.
pub async fn update_categories(State(state): State<Arc<AppState>>, request: Request) -> Rsp {
    let body = match axum::body::to_bytes(request.into_body(), 8 * 1024 * 1024).await {
        Ok(body) => body,
        Err(err) => {
            tracing::warn!("cannot read categories body: {err}");
            return json_error(StatusCode::BAD_REQUEST, "cannot read request body");
        }
    };

    let payload: UpdateCategoriesRequest = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(err) => {
            tracing::warn!("invalid categories payload: {err}");
            return json_error(StatusCode::BAD_REQUEST, format!("invalid JSON body: {err}"));
        }
    };

    let mut incoming = BTreeMap::new();
    for (category_type, entries) in payload.enabled {
        if CATEGORY_TYPES.contains(&category_type.as_str()) {
            incoming.insert(category_type, entries);
        } else {
            tracing::warn!("ignoring unknown category type \"{category_type}\"");
        }
    }

    state.filter.set_enabled(incoming);

    if !state.config.category_filters_path.as_os_str().is_empty() {
        if let Err(err) = state
            .filter
            .save_to_file(&state.config.category_filters_path)
        {
            tracing::error!("unable to persist category filters: {err}");
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("unable to save category filters: {err}"),
            );
        }
    }

    state.m3u_cache.clear().await;
    state.clear_catalog_cache();

    json(
        StatusCode::OK,
        &serde_json::json!({ "message": "Categories updated successfully" }),
    )
}

fn into_infos(
    categories: Vec<crate::xtream::models::Category>,
    category_type: &str,
    enabled: &BTreeMap<String, BTreeMap<String, bool>>,
) -> Vec<CategoryInfo> {
    let active = enabled.get(category_type);
    categories
        .into_iter()
        .map(|category| CategoryInfo {
            id: category.id.get().to_string(),
            name: category.name,
            r#type: category_type.to_string(),
            enabled: active
                .and_then(|ids| ids.get(&category.id.get().to_string()).copied())
                .unwrap_or(false),
        })
        .collect()
}

fn json<T: Serialize>(status: StatusCode, value: &T) -> Rsp {
    match serde_json::to_vec(value) {
        Ok(bytes) => axum::response::Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(bytes))
            .unwrap_or_else(|_| json_error(StatusCode::INTERNAL_SERVER_ERROR, "encoding failed")),
        Err(err) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cannot encode response: {err}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_serialise_with_the_go_field_names() {
        let info = CategoryInfo {
            id: "12".to_string(),
            name: "Sports".to_string(),
            r#type: "live".to_string(),
            enabled: true,
        };
        assert_eq!(
            serde_json::to_string(&info).unwrap(),
            r#"{"id":"12","name":"Sports","type":"live","enabled":true}"#
        );
    }

    #[test]
    fn update_request_matches_the_ui_payload() {
        let parsed: UpdateCategoriesRequest =
            serde_json::from_str(r#"{"enabled":{"live":{"3":true,"4":false}}}"#).unwrap();
        assert!(parsed.enabled["live"]["3"]);
        assert!(!parsed.enabled["live"]["4"]);
    }

    #[test]
    fn ui_html_is_embedded() {
        assert!(UI_HTML.contains("IPTV Proxy - Category Manager"));
        assert!(UI_HTML.contains("api/categories"));
    }
}
