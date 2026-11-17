//! Askama templates, one struct per rendering used by the console.
//!
//! The template files mirror the previous `templates.templ` structure: a
//! shared layout with a content block, full pages that extend it, and the
//! fragments HTMX swaps into the page.

use askama::Template;

use crate::models::{CleanupModel, ConfirmModel, LinkFormModel, LinksModel, LoginModel};

/// The sign-in page, rendered on its own without the console layout.
#[derive(Template, Default)]
#[template(path = "pages/login.html")]
pub struct LoginPage {
    pub model: LoginModel,
}

impl LoginPage {
    #[must_use]
    pub fn with_error(message: &str) -> Self {
        Self {
            model: LoginModel {
                error: message.to_string(),
            },
        }
    }
}

/// `GET /manage/links`, full page.
#[derive(Template)]
#[template(path = "pages/links.html")]
pub struct LinksPage {
    pub model: LinksModel,
    pub meta: crate::models::PageMeta,
}

impl LinksPage {
    #[must_use]
    pub fn new(model: &LinksModel) -> Self {
        Self {
            model: model.clone(),
            meta: model.meta.clone(),
        }
    }
}

/// The HTMX fragment of the links region.
#[derive(Template)]
#[template(path = "fragments/links_surface.html")]
pub struct LinksSurface {
    pub model: LinksModel,
    pub meta: crate::models::PageMeta,
}

impl LinksSurface {
    #[must_use]
    pub fn new(model: &LinksModel) -> Self {
        Self {
            model: model.clone(),
            meta: model.meta.clone(),
        }
    }
}

/// `GET /manage/links/new`, full page.
#[derive(Template)]
#[template(path = "pages/create.html")]
pub struct CreatePage {
    pub model: LinkFormModel,
    pub meta: crate::models::PageMeta,
}

impl CreatePage {
    #[must_use]
    pub fn new(model: &LinkFormModel) -> Self {
        Self {
            model: model.clone(),
            meta: model.meta.clone(),
        }
    }
}

/// The HTMX fragment of the create form.
#[derive(Template)]
#[template(path = "fragments/create.html")]
pub struct CreateContent {
    pub model: LinkFormModel,
    pub meta: crate::models::PageMeta,
}

impl CreateContent {
    #[must_use]
    pub fn new(model: &LinkFormModel) -> Self {
        Self {
            model: model.clone(),
            meta: model.meta.clone(),
        }
    }
}

/// `GET /manage/links/{slug}`, full page.
#[derive(Template)]
#[template(path = "pages/edit.html")]
pub struct EditPage {
    pub model: LinkFormModel,
    pub meta: crate::models::PageMeta,
}

impl EditPage {
    #[must_use]
    pub fn new(model: &LinkFormModel) -> Self {
        Self {
            model: model.clone(),
            meta: model.meta.clone(),
        }
    }
}

/// The HTMX fragment of the edit form.
#[derive(Template)]
#[template(path = "fragments/edit.html")]
pub struct EditContent {
    pub model: LinkFormModel,
    pub meta: crate::models::PageMeta,
}

impl EditContent {
    #[must_use]
    pub fn new(model: &LinkFormModel) -> Self {
        Self {
            model: model.clone(),
            meta: model.meta.clone(),
        }
    }
}

/// `GET /manage/cleanup`.
#[derive(Template)]
#[template(path = "pages/cleanup.html")]
pub struct CleanupPage {
    pub model: CleanupModel,
    pub meta: crate::models::PageMeta,
}

impl CleanupPage {
    #[must_use]
    pub fn new(model: &CleanupModel) -> Self {
        Self {
            model: model.clone(),
            meta: model.meta.clone(),
        }
    }
}

/// The rename and delete confirmation pages.
#[derive(Template)]
#[template(path = "pages/confirm.html")]
pub struct ConfirmPage {
    pub model: ConfirmModel,
    pub meta: crate::models::PageMeta,
}

impl ConfirmPage {
    #[must_use]
    pub fn new(model: &ConfirmModel) -> Self {
        Self {
            model: model.clone(),
            meta: model.meta.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::PageMeta;
    use shorten::link::Link;

    fn meta() -> PageMeta {
        PageMeta {
            title: "Test".to_string(),
            active: "links".to_string(),
            csrf: "token".to_string(),
            notice: String::new(),
            error: String::new(),
        }
    }

    #[test]
    fn login_page_renders_the_sign_in_form() {
        let page = LoginPage::with_error("That API key is not valid.");
        let body = page.render().expect("template renders");
        assert!(body.contains("Enter your administrator API key"));
        assert!(body.contains("That API key is not valid."));
        assert!(body.contains(r#"name="api_key""#));
    }

    #[test]
    fn links_page_renders_layout_rows_and_pagination() {
        let model = LinksModel {
            meta: meta(),
            links: vec![Link::empty()],
            total: 1,
            total_known: true,
            next_cursor: "cursor-value".to_string(),
            search: String::new(),
            filter: "all".to_string(),
            sort: "updated".to_string(),
            dir: "desc".to_string(),
            base_url: "http://localhost:8080".to_string(),
            now: chrono::Utc::now(),
        };
        let page = LinksPage::new(&model).render().expect("template renders");
        assert!(page.contains("<!doctype html>"));
        assert!(page.contains("id=\"links-region\""));
        assert!(page.contains("class=\"data-table\""));
        assert!(page.contains("Showing 1 of 1"));
        let fragment = LinksSurface::new(&model)
            .render()
            .expect("template renders");
        assert!(!fragment.contains("<!doctype html>"));
        assert!(fragment.starts_with("<div id=\"links-region\">"));
    }

    #[test]
    fn empty_state_and_disabled_next_button_render() {
        let model = LinksModel {
            meta: meta(),
            links: Vec::new(),
            total: 0,
            total_known: false,
            next_cursor: String::new(),
            search: String::new(),
            filter: "all".to_string(),
            sort: "updated".to_string(),
            dir: "desc".to_string(),
            base_url: "http://localhost".to_string(),
            now: chrono::Utc::now(),
        };
        let page = LinksPage::new(&model).render().expect("template renders");
        assert!(page.contains("No matching links"));
        assert!(page.contains("type=\"button\" disabled>Next</button>"));
    }

    #[test]
    fn create_page_renders_the_form_surface() {
        let model = LinkFormModel {
            meta: meta(),
            link: Link::empty(),
            original_slug: String::new(),
            destination: String::new(),
            slug: String::new(),
            title: String::new(),
            notes: String::new(),
            status: "active".to_string(),
            redirect_status: "302".to_string(),
            expires_at: String::new(),
            base_url: "http://localhost".to_string(),
            errors: shorten::ValidationErrors::new(),
            is_create: true,
            now: chrono::Utc::now(),
        };
        let page = CreatePage::new(&model).render().expect("template renders");
        assert!(page.contains("id=\"link-form\""));
        assert!(page.contains("action=\"/manage/links\""));
        assert!(page.contains("hx-target=\"#link-form\""));
        assert!(page.contains("Advanced options"));
        assert!(page.contains("Create link</span>"));
    }

    #[test]
    fn edit_page_renders_preview_and_danger_zone() {
        let mut item = Link::empty();
        item.slug = "demo".to_string();
        item.destination_url = "https://example.com/target".to_string();
        let model =
            crate::models::form_from_link(meta(), item, "http://localhost", chrono::Utc::now());
        let page = EditPage::new(&model).render().expect("template renders");
        assert!(page.contains("Live preview"));
        assert!(page.contains("https://example.com/target"));
        assert!(page.contains("/manage/links/demo/rename"));
        assert!(page.contains("/manage/links/demo/delete"));
        assert!(page.contains("value=\"demo\" readonly"));
    }

    #[test]
    fn cleanup_page_renders_preview_and_confirmation() {
        let model = CleanupModel {
            meta: meta(),
            links: Vec::new(),
            base_url: "http://localhost".to_string(),
            scope: "inactive".to_string(),
            months: 6,
            count: 12,
            cutoff: chrono::Utc::now(),
            deleted: 0,
            form_error: "confirm the permanent deletion before continuing".to_string(),
            now: chrono::Utc::now(),
        };
        let page = CleanupPage::new(&model).render().expect("template renders");
        assert!(page.contains("Deletion preview"));
        assert!(page.contains("12 links"));
        assert!(page.contains("confirm the permanent deletion before continuing"));
        assert!(page.contains("name=\"months\" value=\"6\""));
    }

    #[test]
    fn confirm_page_renders_rename_and_delete_forms() {
        let mut item = Link::empty();
        item.slug = "keep".to_string();
        let model = ConfirmModel {
            meta: meta(),
            base_url: "http://localhost".to_string(),
            rename: true,
            now: chrono::Utc::now(),
            link: item,
        };
        let page = ConfirmPage::new(&model).render().expect("template renders");
        assert!(page.contains("Rename this short code?"));
        assert!(page.contains("action=\"/manage/links/keep/rename\""));
        assert!(page.contains("name=\"slug\" value=\"keep\""));
    }
}
