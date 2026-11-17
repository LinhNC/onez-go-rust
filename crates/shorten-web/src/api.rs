//! The JSON API under `/api`, protected by the administrator API key.
//!
//! Field names, status codes, validation messages and the 16 KiB body cap
//! match the previous implementation exactly, so existing API clients keep
//! working against the Rust service.

#![allow(clippy::result_large_err)] // axum handlers use `Response` as the error type.
#![allow(clippy::needless_pass_by_value)] // extractors take their inputs by value.
#![allow(clippy::cast_possible_truncation)] // guarded by the integral check in `go_int`.
use axum::extract::{Path, Request, State};
use axum::http::header::{self, HeaderMap};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

use crate::context::Values;
use crate::models::short_url;
use crate::state::App;
use shorten::link::{self, CreateInput, DomainError, Link, Status, ValidationErrors};
use shorten::store::ListOptions;

/// Maximum request body for the create and patch endpoints, 16 KiB.
const API_BODY_LIMIT: usize = 16 << 10;
/// Default page size for the list endpoint.
const DEFAULT_LIMIT: i64 = 30;

/// Reject a request without a valid API key.
fn api_authorized(app: &App, headers: &HeaderMap) -> Option<Response> {
    let supplied = headers
        .get("X-API-Key")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if app.api_key().valid(supplied) {
        None
    } else {
        Some(crate::state::write_json(
            StatusCode::UNAUTHORIZED,
            &error_body("Invalid API key."),
        ))
    }
}

/// `POST /api/shorten`.
pub async fn create(State(app): State<App>, headers: HeaderMap, request: Request) -> Response {
    if let Some(response) = api_authorized(&app, &headers) {
        return response;
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !content_type.starts_with("application/json") {
        // `net/http` wrote the JSON envelope with 415, not 400.
        return crate::state::write_json(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            &error_body("Content-Type must be application/json."),
        );
    }
    let Ok(body) = crate::context::read_body(request, API_BODY_LIMIT).await else {
        return invalid_json();
    };
    let Ok(parsed) = serde_json::from_slice::<Value>(&body) else {
        return invalid_json();
    };
    // A top-level JSON null decoded into the zero struct for Go's decoder, so
    // validation ran over empty fields; any other non-object was a decode
    // failure with the invalid-JSON envelope.
    let empty = serde_json::Map::new();
    let object = match &parsed {
        Value::Object(object) => object,
        Value::Null => &empty,
        _ => return invalid_json(),
    };
    // `url` and `slug` are read leniently: a non-string value is treated as
    // absent, matching the previous `any` fields.
    let destination = object
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let slug = object
        .get("slug")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let title = match object.get("title") {
        Some(Value::Null) | None => String::new(),
        Some(value) => match value.as_str() {
            Some(value) => value.to_string(),
            None => return invalid_json(),
        },
    };
    let notes = match object.get("notes") {
        Some(Value::Null) | None => String::new(),
        Some(value) => match value.as_str() {
            Some(value) => value.to_string(),
            None => return invalid_json(),
        },
    };
    let status_text = match object.get("status") {
        Some(Value::Null) | None => String::new(),
        Some(value) => match value.as_str() {
            Some(value) => value.to_string(),
            None => return invalid_json(),
        },
    };
    let redirect_status = match object.get("redirectStatus") {
        Some(Value::Null) | None => 0,
        Some(value) => match go_int(value) {
            Some(value) => value,
            None => return invalid_json(),
        },
    };
    let expires_at = match object.get("expiresAt") {
        Some(Value::Null) | None => None,
        Some(value) => match value.as_str() {
            Some(value) => Some(value.to_string()),
            None => return invalid_json(),
        },
    };
    let expires_at = match parse_expiration(expires_at.as_deref()) {
        Ok(value) => value,
        Err(message) => return bad_request(message),
    };
    let status = match parse_status(&status_text) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let input = CreateInput {
        slug,
        destination_url: destination,
        title,
        notes,
        status,
        redirect_status,
        expires_at,
    };
    match app.db("api create", move |store| store.create(input)).await {
        Ok(created) => {
            app.cache().invalidate(&[created.slug.as_str()]);
            app.metrics().mutation("create");
            crate::state::write_json(StatusCode::CREATED, &representation(&app, &created))
        }
        Err(error) => api_error(error),
    }
}

/// `GET /api/links`.
pub async fn list(State(app): State<App>, headers: HeaderMap, request: Request) -> Response {
    if let Some(response) = api_authorized(&app, &headers) {
        return response;
    }
    let query = Values::from_query(request.uri());
    let limit = match query.get("limit") {
        "" => DEFAULT_LIMIT,
        raw => match raw.parse::<i64>() {
            Ok(value) => value,
            Err(_) => return bad_request("Invalid limit."),
        },
    };
    let sort = value_or(query.get("sort"), "updated");
    let dir = value_or(query.get("dir"), "desc");
    let search = query.get("q").to_string();
    let filter = query.get("status").to_string();
    let cursor = query.get("cursor").to_string();
    let result = app
        .db("api list", move |store| {
            store.list(ListOptions {
                search: &search,
                filter: &filter,
                sort: &sort,
                dir: &dir,
                cursor: &cursor,
                limit,
                now: Some(Utc::now()),
            })
        })
        .await;
    match result {
        Ok(page) => {
            let links: Vec<Value> = page
                .links
                .iter()
                .map(|item| representation(&app, item))
                .collect();
            let mut body = Map::new();
            body.insert("links".to_string(), Value::Array(links));
            body.insert("cursor".to_string(), nullable_string(&page.next_cursor));
            body.insert(
                "total".to_string(),
                if page.total_known {
                    Value::from(page.total)
                } else {
                    Value::Null
                },
            );
            crate::state::write_json(StatusCode::OK, &body)
        }
        Err(error) => bad_request(&error.to_string()),
    }
}

/// `GET /api/links/{slug}`.
pub async fn get(State(app): State<App>, headers: HeaderMap, Path(slug): Path<String>) -> Response {
    if let Some(response) = api_authorized(&app, &headers) {
        return response;
    }
    match app
        .db("api get", move |store| store.get_by_slug(&slug))
        .await
    {
        Ok(item) => crate::state::write_json(StatusCode::OK, &representation(&app, &item)),
        Err(error) => api_error(error),
    }
}

/// `PATCH /api/links/{slug}`.
pub async fn patch(
    State(app): State<App>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    request: Request,
) -> Response {
    if let Some(response) = api_authorized(&app, &headers) {
        return response;
    }
    let current = match app
        .db("api patch", move |store| store.get_by_slug(&slug))
        .await
    {
        Ok(item) => item,
        Err(error) => return api_error(error),
    };
    let Ok(body) = crate::context::read_body(request, API_BODY_LIMIT).await else {
        return invalid_json();
    };
    let Ok(parsed) = serde_json::from_slice::<Value>(&body) else {
        return invalid_json();
    };
    let Some(object) = parsed.as_object() else {
        return invalid_json();
    };
    let mut input = CreateInput {
        slug: current.slug.clone(),
        destination_url: current.destination_url.clone(),
        title: current.title.clone(),
        notes: current.notes.clone(),
        status: Some(current.status),
        redirect_status: current.redirect_status,
        expires_at: current.expires_at,
    };
    for (key, value) in object {
        match key.as_str() {
            "slug" => match value.as_str() {
                Some(value) => input.slug = value.to_string(),
                None if value.is_null() => {}
                None => return bad_request("slug must be a string."),
            },
            "url" => match value.as_str() {
                Some(value) => input.destination_url = value.to_string(),
                None if value.is_null() => {}
                None => return bad_request("url must be a string."),
            },
            "title" => match value.as_str() {
                Some(value) => input.title = value.to_string(),
                None if value.is_null() => {}
                None => return bad_request("title must be a string."),
            },
            "notes" => match value.as_str() {
                Some(value) => input.notes = value.to_string(),
                None if value.is_null() => {}
                None => return bad_request("notes must be a string."),
            },
            "status" => match value.as_str() {
                Some(value) => match parse_status(value) {
                    Ok(status) => input.status = status,
                    Err(response) => return response,
                },
                // Go decoded a null into the pointer field and left the
                // stored status untouched.
                None if value.is_null() => {}
                None => return bad_request("status must be a string."),
            },
            "redirectStatus" => match go_int(value) {
                Some(value) => input.redirect_status = value,
                // Go decoded a null into the pointer field and left it unset,
                // which is a no-op for the stored value.
                None if value.is_null() => {}
                None => return bad_request("redirectStatus must be a number."),
            },
            "expiresAt" => match value {
                Value::Null => input.expires_at = None,
                value => match value.as_str() {
                    Some(text) => match shorten::json::parse_rfc3339(text) {
                        Some(parsed) => input.expires_at = Some(parsed),
                        None => return bad_request("expiresAt must be an RFC3339 string or null."),
                    },
                    None => return bad_request("expiresAt must be an RFC3339 string or null."),
                },
            },
            // Unknown fields are ignored, as the previous decoder did.
            _ => {}
        }
    }
    let current_slug = current.slug.clone();
    let patch_slug = current_slug.clone();
    match app
        .db("api patch", move |store| store.update(&patch_slug, input))
        .await
    {
        Ok(updated) => {
            // The previous implementation invalidated both the old and the new
            // slug, because a rename changes two cache keys.
            app.cache()
                .invalidate(&[current_slug.as_str(), updated.slug.as_str()]);
            app.metrics().mutation("update");
            crate::state::write_json(StatusCode::OK, &representation(&app, &updated))
        }
        Err(error) => api_error(error),
    }
}

/// `DELETE /api/links/{slug}`.
pub async fn remove(
    State(app): State<App>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Response {
    if let Some(response) = api_authorized(&app, &headers) {
        return response;
    }
    if !link::valid_slug(&slug) {
        return bad_request("Invalid short link code.");
    }
    let slug_for_store = slug.clone();
    match app
        .db("api delete", move |store| store.delete(&slug_for_store))
        .await
    {
        Ok(()) => {
            app.cache().invalidate(&[slug.as_str()]);
            app.metrics().mutation("delete");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => api_error(error),
    }
}

/// The public representation of one link, with the field names the API has
/// always used.
#[must_use]
pub fn representation(app: &App, item: &Link) -> Value {
    let mut body = Map::new();
    body.insert("id".to_string(), Value::from(item.id));
    body.insert("slug".to_string(), Value::from(item.slug.as_str()));
    body.insert(
        "shortUrl".to_string(),
        Value::from(short_url(&app.config().public_base_url, &item.slug)),
    );
    body.insert(
        "destination".to_string(),
        Value::from(item.destination_url.as_str()),
    );
    body.insert(
        "url".to_string(),
        Value::from(item.destination_url.as_str()),
    );
    body.insert("title".to_string(), Value::from(item.title.as_str()));
    body.insert("notes".to_string(), Value::from(item.notes.as_str()));
    body.insert("status".to_string(), Value::from(item.status.as_str()));
    body.insert(
        "redirectStatus".to_string(),
        Value::from(i64::from(item.redirect_status)),
    );
    body.insert("expiresAt".to_string(), nullable_time(item.expires_at));
    body.insert("visits".to_string(), Value::from(item.visit_count));
    body.insert(
        "lastVisitedAt".to_string(),
        nullable_time(item.last_visited_at),
    );
    body.insert(
        "createdAt".to_string(),
        Value::from(shorten::json::format_go_time(item.created_at)),
    );
    body.insert(
        "updatedAt".to_string(),
        Value::from(shorten::json::format_go_time(item.updated_at)),
    );
    Value::Object(body)
}

/// Translate a store failure into the API's error contract.
fn api_error(error: DomainError) -> Response {
    if let Some(fields) = error.validation() {
        let mut body = Map::new();
        body.insert("error".to_string(), Value::from("Validation failed."));
        let fields: Map<String, Value> = fields
            .iter()
            .map(|(key, value)| (key.clone(), Value::from(value.as_str())))
            .collect();
        body.insert("fields".to_string(), Value::Object(fields));
        return crate::state::write_json(StatusCode::BAD_REQUEST, &body);
    }
    if error.is_conflict() {
        return crate::state::write_json(
            StatusCode::CONFLICT,
            &error_body("This custom slug is already in use. Please choose another one."),
        );
    }
    if matches!(error, DomainError::NotFound) {
        return crate::state::write_json(StatusCode::NOT_FOUND, &error_body("Link not found."));
    }
    tracing::error!(error = %error, "api operation failed");
    crate::state::write_json(
        StatusCode::INTERNAL_SERVER_ERROR,
        &error_body("The operation could not be completed."),
    )
}

fn parse_expiration(value: Option<&str>) -> Result<Option<DateTime<Utc>>, &'static str> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    shorten::json::parse_rfc3339(value)
        .map(Some)
        .ok_or("expiresAt must be an RFC3339 timestamp")
}

/// An unrecognised status string is a field error, matching the previous
/// service's `Choose active or disabled.` message.
fn parse_status(value: &str) -> Result<Option<Status>, Response> {
    if value.is_empty() {
        return Ok(None);
    }
    if let Some(status) = Status::parse(value) {
        Ok(Some(status))
    } else {
        let mut fields = ValidationErrors::new();
        shorten::link::status_error(&mut fields);
        let mut body = Map::new();
        body.insert("error".to_string(), Value::from("Validation failed."));
        let fields: Map<String, Value> = fields
            .iter()
            .map(|(key, value)| (key.clone(), Value::from(value.as_str())))
            .collect();
        body.insert("fields".to_string(), Value::Object(fields));
        Err(crate::state::write_json(
            StatusCode::BAD_REQUEST,
            &Value::Object(body),
        ))
    }
}

/// Go decodes JSON numbers into `int`, accepting `302.0` and rejecting
/// fractional values.
fn go_int(value: &Value) -> Option<i32> {
    let number = value.as_f64()?;
    if number.fract() != 0.0 || !number.is_finite() {
        return None;
    }
    i32::try_from(number.trunc() as i64).ok()
}

fn nullable_time(value: Option<DateTime<Utc>>) -> Value {
    value.map_or(Value::Null, |value| {
        Value::from(shorten::json::format_go_time(value))
    })
}

fn nullable_string(value: &str) -> Value {
    if value.is_empty() {
        Value::Null
    } else {
        Value::from(value)
    }
}

fn value_or<'a>(value: &'a str, fallback: &'a str) -> String {
    if value.is_empty() {
        fallback.to_string()
    } else {
        value.to_string()
    }
}

fn error_body(message: &str) -> Value {
    let mut body = Map::new();
    body.insert("error".to_string(), Value::from(message));
    Value::Object(body)
}

fn bad_request(message: &str) -> Response {
    crate::state::write_json(StatusCode::BAD_REQUEST, &error_body(message))
}

fn invalid_json() -> Response {
    bad_request("The request body must be valid JSON.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_int_semantics_match_json_decode() {
        assert_eq!(go_int(&Value::from(302)), Some(302));
        assert_eq!(go_int(&serde_json::json!(302.0)), Some(302));
        assert_eq!(go_int(&serde_json::json!(302.5)), None);
        assert_eq!(go_int(&serde_json::json!("302")), None);
        assert_eq!(go_int(&serde_json::json!(true)), None);
    }

    #[test]
    fn expiration_parsing() {
        assert!(parse_expiration(None).unwrap().is_none());
        assert!(parse_expiration(Some("")).unwrap().is_none());
        assert!(parse_expiration(Some("nope")).is_err());
        let parsed = parse_expiration(Some("2026-08-01T10:00:00Z"))
            .unwrap()
            .expect("expiration");
        assert_eq!(
            shorten::json::format_go_time(parsed),
            "2026-08-01T10:00:00Z"
        );
    }

    #[test]
    fn nullable_fields_match_the_go_shape() {
        assert_eq!(nullable_string(""), Value::Null);
        assert_eq!(nullable_string("abc"), Value::from("abc"));
        assert_eq!(nullable_time(None), Value::Null);
    }

    #[test]
    fn value_or_falls_back() {
        assert_eq!(value_or("", "updated"), "updated");
        assert_eq!(value_or("slug", "updated"), "slug");
    }
}
