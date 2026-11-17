//! The cookie-session management console.
//!
//! Routes, form field names, CSRF and origin gates, HTMX fragment rendering,
//! rate limits and redirect targets are those of the previous service.

#![allow(clippy::result_large_err)] // axum handlers use `Response` as the error type.
#![allow(clippy::missing_panics_doc)] // the panics are fixed-width conversions guarded at the call site.
use axum::extract::{Path, Request, State};
use axum::http::header::{self, HeaderMap};
use axum::http::{StatusCode, Uri};
use axum::response::Response;
use chrono::{DateTime, Utc};

use crate::context::{self, after_mutation, ClientIp, Session, Values};
use crate::models::{
    self, form_from_link, CleanupModel, ConfirmModel, LinkFormModel, LinksModel, PageMeta,
};
use crate::state::{self, App};
use crate::templates;
use shorten::json;
use shorten::link::{self, CreateInput, DomainError, Status, ValidationErrors};
use shorten::store::ListOptions;

/// Login form body cap, matching the previous `MaxBytesReader` size.
const LOGIN_BODY_LIMIT: usize = 8 << 10;
/// Management form body cap.
const MUTATION_BODY_LIMIT: usize = 64 << 10;
/// Links per console page.
const PAGE_SIZE: i64 = 30;
/// Links listed in a cleanup preview.
const CLEANUP_PREVIEW: i64 = 10;

/// `GET /manage/login`.
pub async fn login_page(State(app): State<App>, headers: HeaderMap) -> Response {
    // A valid session skips the sign-in page.
    if read_session_cookie(&app, &headers) {
        return context::go_redirect(StatusCode::SEE_OTHER, "/manage/links", &headers, true);
    }
    render(StatusCode::OK, templates::LoginPage::default())
}

/// `POST /manage/login`.
pub async fn login(
    State(app): State<App>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let now = Utc::now();
    if !app.login_limiter().allow(&ip, now) {
        return render(
            StatusCode::TOO_MANY_REQUESTS,
            templates::LoginPage::with_error("Too many attempts. Wait a minute and try again."),
        );
    }
    let Ok(body) = context::read_body(request, LOGIN_BODY_LIMIT).await else {
        return render(
            StatusCode::BAD_REQUEST,
            templates::LoginPage::with_error("Unable to read the sign-in form."),
        );
    };
    let values = Values::default().with_body(&String::from_utf8_lossy(&body));
    if !context::same_origin(&headers, &app) {
        tracing::warn!(origin = %origin_of(&headers), "login rejected: origin mismatch");
        return render(
            StatusCode::UNAUTHORIZED,
            templates::LoginPage::with_error("That API key is not valid."),
        );
    }
    if !app.api_key().valid(values.get("api_key")) {
        tracing::warn!("login rejected: invalid credential");
        return render(
            StatusCode::UNAUTHORIZED,
            templates::LoginPage::with_error("That API key is not valid."),
        );
    }
    let Ok((token, claims)) = app.sessions().issue(now) else {
        return render(
            StatusCode::INTERNAL_SERVER_ERROR,
            templates::LoginPage::with_error("Unable to start a secure session."),
        );
    };
    let expires = chrono::DateTime::from_timestamp(claims.expires_at, 0).unwrap_or(now);
    let spec = app.sessions().cookie_attributes(&token, expires, now);
    context::redirect_with_cookie(
        StatusCode::SEE_OTHER,
        "/manage/links",
        &headers,
        Some(&context::build_cookie(&spec)),
    )
}

/// `POST /manage/logout`, session required.
pub async fn logout(
    State(app): State<App>,
    session: Session,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let body = match context::read_body(request, MUTATION_BODY_LIMIT).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let values = Values::default().with_body(&String::from_utf8_lossy(&body));
    if !context::valid_csrf(Some(&session.0), values.get("csrf")) {
        return state::text_error(StatusCode::FORBIDDEN, "Invalid security token.");
    }
    let spec = app.sessions().clear_cookie_attributes();
    context::redirect_with_cookie(
        StatusCode::SEE_OTHER,
        "/manage/login",
        &headers,
        Some(&context::build_cookie(&spec)),
    )
}

/// `GET /manage/links`, full page or HTMX fragment.
pub async fn links_page(
    State(app): State<App>,
    session: Session,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let values = Values::from_query(&uri);
    let search = values.get("q").to_string();
    let filter = value_or(values.get("status"), "all");
    let sort = value_or(values.get("sort"), "updated");
    let dir = value_or(values.get("dir"), "desc");
    let cursor = values.get("cursor").to_string();
    let base_url = app.config().public_base_url.clone();
    let notice = notice_text(values.get("notice"));
    let now = Utc::now();
    let query = (
        search.clone(),
        filter.clone(),
        sort.clone(),
        dir.clone(),
        cursor.clone(),
    );
    let page = app
        .db("list links", move |store| {
            store.list(ListOptions {
                search: &query.0,
                filter: &query.1,
                sort: &query.2,
                dir: &query.3,
                cursor: &query.4,
                limit: PAGE_SIZE,
                now: Some(now),
            })
        })
        .await;
    let page = match page {
        Ok(page) => page,
        Err(error) => {
            return state::text_error(
                StatusCode::BAD_REQUEST,
                &format!("Invalid list request: {error}"),
            );
        }
    };
    let model = LinksModel {
        meta: PageMeta {
            title: "Links".to_string(),
            active: "links".to_string(),
            csrf: session.csrf().to_string(),
            notice,
            error: String::new(),
        },
        links: page.links,
        total: page.total,
        total_known: page.total_known,
        next_cursor: page.next_cursor,
        search,
        filter,
        sort,
        dir,
        base_url,
        now,
    };
    if context::is_htmx(&headers) {
        return render(StatusCode::OK, templates::LinksSurface::new(&model));
    }
    render(StatusCode::OK, templates::LinksPage::new(&model))
}

/// `GET /manage/links/new`.
pub async fn create_page(State(app): State<App>, session: Session) -> Response {
    let model = LinkFormModel {
        meta: PageMeta {
            title: "Create link".to_string(),
            active: "new".to_string(),
            csrf: session.csrf().to_string(),
            notice: String::new(),
            error: String::new(),
        },
        link: link::Link::empty(),
        original_slug: String::new(),
        destination: String::new(),
        slug: String::new(),
        title: String::new(),
        notes: String::new(),
        status: "active".to_string(),
        redirect_status: "302".to_string(),
        expires_at: String::new(),
        base_url: app.config().public_base_url.clone(),
        errors: ValidationErrors::new(),
        is_create: true,
        now: Utc::now(),
    };
    render(StatusCode::OK, templates::CreatePage::new(&model))
}

/// `POST /manage/links`.
pub async fn create_link(
    State(app): State<App>,
    session: Session,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let values = match read_form(&app, &headers, &ip, &session, request, MUTATION_BODY_LIMIT).await
    {
        Ok(values) => values,
        Err(response) => return response,
    };
    let (mut model, input) = parse_link_form(&app, &session, &values, true);
    if !model.errors.is_empty() {
        return render_form_error(&model, &headers, StatusCode::UNPROCESSABLE_ENTITY);
    }
    match app
        .db("create link", move |store| store.create(input))
        .await
    {
        Ok(created) => {
            app.cache().invalidate(&[created.slug.as_str()]);
            app.metrics().mutation("create");
            after_mutation(
                &headers,
                &format!(
                    "/manage/links/{}?notice=created",
                    models::path_escape(&created.slug)
                ),
            )
        }
        Err(error) => {
            if fill_form_error(&mut model, &error) {
                return render_form_error(&model, &headers, error_status(&error));
            }
            tracing::error!(error = %error, "unable to create link");
            state::text_error(StatusCode::INTERNAL_SERVER_ERROR, "Unable to create link.")
        }
    }
}

/// `GET /manage/links/{slug}`.
pub async fn edit_page(
    State(app): State<App>,
    session: Session,
    Path(slug): Path<String>,
    uri: Uri,
) -> Response {
    let Ok(item) = app
        .db("read link", move |store| store.get_by_slug(&slug))
        .await
    else {
        return state::not_found();
    };
    let values = Values::from_query(&uri);
    let model = form_from_link(
        PageMeta {
            title: "Edit link".to_string(),
            active: "links".to_string(),
            csrf: session.csrf().to_string(),
            notice: notice_text(values.get("notice")),
            error: String::new(),
        },
        item,
        &app.config().public_base_url,
        Utc::now(),
    );
    render(StatusCode::OK, templates::EditPage::new(&model))
}

/// `POST /manage/links/{slug}`.
pub async fn update_link(
    State(app): State<App>,
    session: Session,
    ClientIp(ip): ClientIp,
    Path(slug): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let values = match read_form(&app, &headers, &ip, &session, request, MUTATION_BODY_LIMIT).await
    {
        Ok(values) => values,
        Err(response) => return response,
    };
    let (mut model, mut input) = parse_link_form(&app, &session, &values, false);
    // The slug of an edited link is fixed by the route, as in the previous
    // service: renaming goes through the dedicated rename flow.
    input.slug = slug.clone();
    if !model.errors.is_empty() {
        return render_form_error(&model, &headers, StatusCode::UNPROCESSABLE_ENTITY);
    }
    let current = slug.clone();
    match app
        .db("update link", move |store| store.update(&current, input))
        .await
    {
        Ok(updated) => {
            app.cache()
                .invalidate(&[slug.as_str(), updated.slug.as_str()]);
            app.metrics().mutation("update");
            after_mutation(
                &headers,
                &format!(
                    "/manage/links/{}?notice=saved",
                    models::path_escape(&updated.slug)
                ),
            )
        }
        Err(error) => {
            if fill_form_error(&mut model, &error) {
                return render_form_error(&model, &headers, error_status(&error));
            }
            tracing::error!(error = %error, "unable to update link");
            state::text_error(StatusCode::INTERNAL_SERVER_ERROR, "Unable to update link.")
        }
    }
}

/// `GET /manage/links/{slug}/rename`.
pub async fn rename_page(
    State(app): State<App>,
    session: Session,
    Path(slug): Path<String>,
) -> Response {
    let Ok(item) = app
        .db("read link", move |store| store.get_by_slug(&slug))
        .await
    else {
        return state::not_found();
    };
    let model = confirm_model(&app, &session, item, true, String::new());
    render(StatusCode::OK, templates::ConfirmPage::new(&model))
}

/// `POST /manage/links/{slug}/rename`.
pub async fn rename_link(
    State(app): State<App>,
    session: Session,
    ClientIp(ip): ClientIp,
    Path(slug): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let values = match read_form(&app, &headers, &ip, &session, request, MUTATION_BODY_LIMIT).await
    {
        Ok(values) => values,
        Err(response) => return response,
    };
    let Ok(current) = app
        .db("read link", move |store| store.get_by_slug(&slug))
        .await
    else {
        return state::not_found();
    };
    let input = CreateInput {
        slug: values.get("slug").trim().to_string(),
        destination_url: current.destination_url.clone(),
        title: current.title.clone(),
        notes: current.notes.clone(),
        status: Some(current.status),
        redirect_status: current.redirect_status,
        expires_at: current.expires_at,
    };
    let current_slug = current.slug.clone();
    let rename_slug = current_slug.clone();
    match app
        .db("rename link", move |store| {
            store.update(&rename_slug, input)
        })
        .await
    {
        Ok(updated) => {
            app.cache()
                .invalidate(&[current_slug.as_str(), updated.slug.as_str()]);
            app.metrics().mutation("rename");
            after_mutation(
                &headers,
                &format!(
                    "/manage/links/{}?notice=renamed",
                    models::path_escape(&updated.slug)
                ),
            )
        }
        Err(error) => {
            let model = confirm_model(&app, &session, current, true, human_error(&error));
            render(error_status(&error), templates::ConfirmPage::new(&model))
        }
    }
}

/// `GET /manage/links/{slug}/delete`.
pub async fn delete_page(
    State(app): State<App>,
    session: Session,
    Path(slug): Path<String>,
) -> Response {
    let Ok(item) = app
        .db("read link", move |store| store.get_by_slug(&slug))
        .await
    else {
        return state::not_found();
    };
    let model = confirm_model(&app, &session, item, false, String::new());
    render(StatusCode::OK, templates::ConfirmPage::new(&model))
}

/// `POST /manage/links/{slug}/delete`.
pub async fn delete_link(
    State(app): State<App>,
    session: Session,
    ClientIp(ip): ClientIp,
    Path(slug): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    match read_form(&app, &headers, &ip, &session, request, MUTATION_BODY_LIMIT).await {
        Ok(_values) => {}
        Err(response) => return response,
    }
    let delete_slug = slug.clone();
    match app
        .db("delete link", move |store| store.delete(&delete_slug))
        .await
    {
        Ok(()) => {
            app.cache().invalidate(&[slug.as_str()]);
            app.metrics().mutation("delete");
            after_mutation(&headers, "/manage/links?notice=deleted")
        }
        Err(_) => state::not_found(),
    }
}

/// `GET /manage/cleanup`.
pub async fn cleanup_page(State(app): State<App>, session: Session, uri: Uri) -> Response {
    let values = Values::from_query(&uri);
    let now = Utc::now();
    let Ok((months, scope, cutoff)) =
        cleanup_options(values.get("months"), values.get("scope"), now)
    else {
        return state::text_error(StatusCode::BAD_REQUEST, "Invalid cleanup request.");
    };
    let preview_scope = scope.clone();
    let preview = app
        .db("preview cleanup", move |store| {
            store.preview_cleanup(cutoff, now, &preview_scope, CLEANUP_PREVIEW)
        })
        .await;
    let preview = match preview {
        Ok(preview) => preview,
        Err(error) => {
            tracing::error!(error = %error, "unable to preview old links");
            return state::text_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Unable to preview old links.",
            );
        }
    };
    let mut model = CleanupModel {
        meta: PageMeta {
            title: "Cleanup old links".to_string(),
            active: "links".to_string(),
            csrf: session.csrf().to_string(),
            notice: String::new(),
            error: String::new(),
        },
        links: preview.links,
        base_url: app.config().public_base_url.clone(),
        scope,
        months,
        count: preview.count,
        cutoff,
        deleted: 0,
        form_error: String::new(),
        now,
    };
    if values.get("notice") == "cleaned" {
        model.deleted = values.get("deleted").parse::<i64>().unwrap_or(model.count);
        model.meta.notice = format!("{} old links deleted.", models::format_count(model.deleted));
    }
    render(StatusCode::OK, templates::CleanupPage::new(&model))
}

/// `POST /manage/cleanup`.
pub async fn cleanup_links(
    State(app): State<App>,
    session: Session,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let values = match read_form(&app, &headers, &ip, &session, request, MUTATION_BODY_LIMIT).await
    {
        Ok(values) => values,
        Err(response) => return response,
    };
    let now = Utc::now();
    let options = cleanup_options(values.get("months"), values.get("scope"), now);
    if values.get("confirm") != "yes" || options.is_err() {
        let (months, scope, cutoff, message) = match options {
            Ok((months, scope, cutoff)) => (
                months,
                scope,
                cutoff,
                "confirm the permanent deletion before continuing".to_string(),
            ),
            Err(message) => {
                let (months, scope, cutoff) =
                    cleanup_options("6", "inactive", now).expect("defaults are valid");
                (months, scope, cutoff, message)
            }
        };
        return render_cleanup_error(&app, &session, months, &scope, cutoff, &message).await;
    }
    let (months, scope, cutoff) = options.expect("checked above");
    let delete_scope = scope.clone();
    match app
        .db("delete old links", move |store| {
            store.delete_before(cutoff, now, &delete_scope)
        })
        .await
    {
        Ok(deleted) => {
            app.cache().clear();
            app.metrics().mutation("cleanup");
            after_mutation(
                &headers,
                &format!(
                    "/manage/cleanup?months={months}&scope={}&notice=cleaned&deleted={deleted}",
                    context::query_escape(&scope),
                ),
            )
        }
        Err(error) => {
            tracing::error!(error = %error, "unable to delete old links");
            state::text_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Unable to delete old links.",
            )
        }
    }
}

/// Validate a mutation form: rate limit, body size, origin and CSRF.
async fn read_form(
    app: &App,
    headers: &HeaderMap,
    ip: &str,
    session: &Session,
    request: Request,
    limit: usize,
) -> Result<Values, Response> {
    if !app.mutation_limiter().allow(ip, Utc::now()) {
        return Err(state::text_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many changes. Try again shortly.",
        ));
    }
    let body = context::read_body(request, limit)
        .await
        .map_err(|_| state::text_error(StatusCode::BAD_REQUEST, "Invalid form."))?;
    let values = Values::default().with_body(&String::from_utf8_lossy(&body));
    if !context::same_origin(headers, app)
        || !context::valid_csrf(Some(&session.0), values.get("csrf"))
    {
        return Err(state::text_error(
            StatusCode::FORBIDDEN,
            "Invalid security token.",
        ));
    }
    Ok(values)
}

/// Turn a submitted link form into a model and an input, collecting the same
/// field errors the previous service collected.
fn parse_link_form(
    app: &App,
    session: &Session,
    values: &Values,
    create: bool,
) -> (LinkFormModel, CreateInput) {
    let meta = PageMeta {
        title: if create { "Create link" } else { "Edit link" }.to_string(),
        active: if create { "new" } else { "links" }.to_string(),
        csrf: session.csrf().to_string(),
        notice: String::new(),
        error: String::new(),
    };
    let mut model = LinkFormModel {
        meta,
        link: link::Link::empty(),
        original_slug: String::new(),
        destination: values.get("destination").to_string(),
        slug: values.get("slug").to_string(),
        title: values.get("title").to_string(),
        notes: values.get("notes").to_string(),
        status: values.get("status").to_string(),
        redirect_status: values.get("redirect_status").to_string(),
        expires_at: values.get("expires_at").to_string(),
        base_url: app.config().public_base_url.clone(),
        errors: ValidationErrors::new(),
        is_create: create,
        now: Utc::now(),
    };
    let status = if model.status.is_empty() {
        None
    } else if let Some(status) = Status::parse(&model.status) {
        Some(status)
    } else {
        link::status_error(&mut model.errors);
        None
    };
    let redirect_status = if let Ok(value) = model.redirect_status.parse::<i32>() {
        value
    } else {
        model.errors.insert(
            "redirectStatus".to_string(),
            "Choose a redirect type.".to_string(),
        );
        0
    };
    let expires_at = match json::parse_local_input(&model.expires_at) {
        Some(value) => Some(value),
        None if model.expires_at.is_empty() => None,
        None => {
            model.errors.insert(
                "expiresAt".to_string(),
                "Enter a valid UTC date and time.".to_string(),
            );
            None
        }
    };
    let mut input = CreateInput {
        slug: model.slug.clone(),
        destination_url: model.destination.clone(),
        title: model.title.clone(),
        notes: model.notes.clone(),
        status,
        redirect_status,
        expires_at,
    };
    if let Err(error) = link::validate_create(&mut input) {
        if let Some(fields) = error.validation() {
            for (key, value) in fields {
                model.errors.insert(key.clone(), value.clone());
            }
        }
    }
    // Keep the model aligned with the validated, normalized input.
    model.slug.clone_from(&input.slug);
    model.destination.clone_from(&input.destination_url);
    model.title.clone_from(&input.title);
    model.notes.clone_from(&input.notes);
    (model, input)
}

/// Render a rejected form: HTMX gets the 200 fragment the console swaps in,
/// browsers get the full page with the failure status.
fn render_form_error(model: &LinkFormModel, headers: &HeaderMap, status: StatusCode) -> Response {
    if context::is_htmx(headers) {
        let status = StatusCode::OK;
        return if model.is_create {
            render(status, templates::CreateContent::new(model))
        } else {
            render(status, templates::EditContent::new(model))
        };
    }
    if model.is_create {
        render(status, templates::CreatePage::new(model))
    } else {
        render(status, templates::EditPage::new(model))
    }
}

async fn render_cleanup_error(
    app: &App,
    session: &Session,
    months: i64,
    scope: &str,
    cutoff: DateTime<Utc>,
    message: &str,
) -> Response {
    let now = Utc::now();
    let preview = app
        .db("preview cleanup", {
            let scope = scope.to_string();
            move |store| store.preview_cleanup(cutoff, now, &scope, CLEANUP_PREVIEW)
        })
        .await;
    let preview = match preview {
        Ok(preview) => preview,
        Err(error) => {
            tracing::error!(error = %error, "unable to preview old links");
            return state::text_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Unable to preview old links.",
            );
        }
    };
    let model = CleanupModel {
        meta: PageMeta {
            title: "Cleanup old links".to_string(),
            active: "links".to_string(),
            csrf: session.csrf().to_string(),
            notice: String::new(),
            error: String::new(),
        },
        links: preview.links,
        base_url: app.config().public_base_url.clone(),
        scope: (if scope.is_empty() { "inactive" } else { scope }).to_string(),
        months: if months == 0 { 6 } else { months },
        count: preview.count,
        cutoff,
        deleted: 0,
        form_error: message.to_string(),
        now,
    };
    render(
        StatusCode::UNPROCESSABLE_ENTITY,
        templates::CleanupPage::new(&model),
    )
}

/// Cleanup controls, validated exactly like the previous helper: 3, 6 or 12
/// months, one of four scopes, and a cutoff of `now - months`.
fn cleanup_options(
    months_value: &str,
    scope_value: &str,
    now: DateTime<Utc>,
) -> Result<(i64, String, DateTime<Utc>), String> {
    let months = if months_value.is_empty() {
        6
    } else {
        match months_value.parse::<i64>() {
            Ok(value) => value,
            Err(_) => return Err("cleanup age must be 3, 6, or 12 months".to_string()),
        }
    };
    if months != 3 && months != 6 && months != 12 {
        return Err("cleanup age must be 3, 6, or 12 months".to_string());
    }
    let scope = value_or(scope_value, "inactive");
    if !matches!(scope.as_str(), "inactive" | "disabled" | "expired" | "all") {
        return Err("unsupported cleanup scope".to_string());
    }
    // Go used calendar arithmetic (`AddDate(0, -months, 0)`), so the cutoff is
    // the same wall-clock time `months` calendar months earlier.
    let months_u32 = u32::try_from(months).unwrap_or(6);
    let date = now
        .date_naive()
        .checked_sub_months(chrono::Months::new(months_u32))
        .unwrap_or(now.date_naive());
    let cutoff = date.and_time(now.time()).and_utc();
    Ok((months, scope, cutoff))
}

/// Map a store failure onto the console's form-error contract.
fn fill_form_error(model: &mut LinkFormModel, error: &DomainError) -> bool {
    if let Some(fields) = error.validation() {
        model.errors = fields.clone();
        return true;
    }
    if error.is_conflict() {
        model.errors = ValidationErrors::new();
        model.errors.insert(
            "slug".to_string(),
            "This short code is already in use.".to_string(),
        );
        return true;
    }
    false
}

fn human_error(error: &DomainError) -> String {
    if error.is_conflict() {
        return "This short code is already in use.".to_string();
    }
    if let Some(fields) = error.validation() {
        if let Some((_, value)) = fields.iter().next() {
            return value.clone();
        }
    }
    "The operation could not be completed.".to_string()
}

fn error_status(error: &DomainError) -> StatusCode {
    if error.is_conflict() {
        StatusCode::CONFLICT
    } else {
        StatusCode::UNPROCESSABLE_ENTITY
    }
}

fn notice_text(value: &str) -> String {
    match value {
        "created" => "Link created.".to_string(),
        "saved" => "Changes saved.".to_string(),
        "renamed" => "Short code renamed.".to_string(),
        "deleted" => "Link deleted.".to_string(),
        _ => String::new(),
    }
}

fn confirm_model(
    app: &App,
    session: &Session,
    item: shorten::link::Link,
    rename: bool,
    error: String,
) -> ConfirmModel {
    ConfirmModel {
        meta: PageMeta {
            title: if rename { "Rename link" } else { "Delete link" }.to_string(),
            active: "links".to_string(),
            csrf: session.csrf().to_string(),
            notice: String::new(),
            error,
        },
        base_url: app.config().public_base_url.clone(),
        rename,
        now: Utc::now(),
        link: item,
    }
}

fn render<T: askama::Template>(status: StatusCode, template: T) -> Response {
    match template.render() {
        Ok(body) => state::html(status, body),
        Err(error) => {
            tracing::error!(error = %error, "render failed");
            state::text_error(StatusCode::INTERNAL_SERVER_ERROR, "Render failed.")
        }
    }
}

fn value_or(value: &str, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_string()
    } else {
        value.to_string()
    }
}

fn origin_of(headers: &HeaderMap) -> String {
    headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// Read and validate the session cookie, used by the sign-in page to skip the
/// form when a session is already active.
fn read_session_cookie(app: &App, headers: &HeaderMap) -> bool {
    context::read_cookie(headers, context::SESSION_COOKIE)
        .is_some_and(|token| app.sessions().parse(&token, Utc::now()).is_ok())
}
