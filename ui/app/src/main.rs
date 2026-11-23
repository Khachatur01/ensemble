//! The web UI of an Ensemble hub: a single-page app the hub serves at `/`,
//! over its REST API on the same origin. Every page runs the hub's
//! operations, as the CLI and the REST API do; it adds nothing the API has
//! not.

mod api;
mod form;
mod lookup;
mod pages;
mod settings;

use dioxus::prelude::*;
use form::Catalog;
use pages::*;
use serde_json::Value;
use settings::*;

fn main() {
    dioxus::launch(App);
}

/// The app's pages, by their paths.
#[derive(Routable, Clone, PartialEq, Debug)]
#[rustfmt::skip]
pub enum Route {
    #[route("/login")]
    Login {},
    #[layout(Shell)]
        #[route("/")]
        Home {},
        #[route("/tasks")]
        Projects {},
        #[route("/tasks/:project")]
        ProjectTasks { project: String },
        #[route("/tasks/:project/:id")]
        TaskPage { project: String, id: String },
        #[route("/knowledge")]
        Knowledge {},
        #[route("/runners")]
        Runners {},
        #[route("/account")]
        Account {},
        #[route("/users")]
        Users {},
        #[route("/users/:name")]
        UserPage { name: String },
        #[route("/settings")]
        Settings {},
        #[route("/settings/:section")]
        SettingsPage { section: String },
        #[route("/settings/op/:op")]
        OpPage { op: String },
    #[end_layout]
    #[route("/:..rest")]
    NotFound { rest: Vec<String> },
}

/// Who is signed in, as `GET /api/v1/me` says: their account and what they
/// may do.
#[derive(Clone, PartialEq)]
pub struct Me(pub Value);

impl Me {
    pub fn name(&self) -> String {
        self.0["name"].as_str().unwrap_or("").to_string()
    }

    /// May they do what `permission` lets?
    pub fn can(&self, permission: &str) -> bool {
        self.0["permissions"]
            .as_array()
            .is_some_and(|p| p.iter().any(|p| p == permission))
    }

    /// Does their sign-up wait for an admin's approval?
    pub fn pending(&self) -> bool {
        self.0["status"] == "pending"
    }
}

#[component]
fn App() -> Element {
    use_context_provider(|| Signal::new(None::<Me>));
    rsx! { Router::<Route> {} }
}

/// Every page but the login: who is signed in, the navigation, and the page.
#[component]
fn Shell() -> Element {
    let mut session = use_context::<Signal<Option<Me>>>();
    let loaded = use_resource(move || async move {
        let answer = api::me().await;
        if let Ok(value) = &answer {
            session.set(Some(Me(value.clone())));
        }
        answer
    });
    let catalog: Catalog = use_resource(|| api::get("/api/v1/ops"));
    use_context_provider(|| catalog);
    let nav = navigator();
    let route = use_route::<Route>().to_string();
    // The class of the navigation's link to the pages under `path`.
    let here = |path: &str| {
        if route == path || route.starts_with(&format!("{path}/")) {
            "active".to_string()
        } else {
            String::new()
        }
    };
    let signed_in = session.read().clone();
    let Some(me) = signed_in else {
        return match &*loaded.read() {
            Some(Err(e)) if e.signed_out() => {
                nav.replace(Route::Login {});
                rsx! {}
            }
            Some(Err(e)) => rsx! { p { class: "error page", "{e}" } },
            _ => rsx! { p { class: "loading page", "Loading…" } },
        };
    };
    let sign_out = move |_| {
        spawn(async move {
            let _ = api::sign_out().await;
            session.set(None);
            nav.replace(Route::Login {});
        });
    };
    rsx! {
        header { class: "bar",
            Link { class: "brand", to: Route::Home {}, "Ensemble" }
            nav {
                if me.can("tasks") {
                    Link { to: Route::Projects {}, class: here("/tasks"), "Tasks" }
                    Link { to: Route::Runners {}, class: here("/runners"), "Runners" }
                }
                if me.can("recall") || me.can("share") {
                    Link { to: Route::Knowledge {}, class: here("/knowledge"), "Knowledge" }
                }
                if me.can("admin") {
                    Link { to: Route::Users {}, class: here("/users"), "Users" }
                    Link { to: Route::Settings {}, class: here("/settings"), "Settings" }
                }
            }
            div { class: "who",
                Link { to: Route::Account {}, "{me.name()}" }
                button { class: "link", onclick: sign_out, "Sign out" }
            }
        }
        main { class: "page",
            if me.pending() {
                p { class: "notice",
                    "Your sign-up waits for an admin's approval: until then you may do nothing here."
                }
            } else {
                Outlet::<Route> {}
            }
        }
    }
}

#[component]
fn NotFound(rest: Vec<String>) -> Element {
    rsx! {
        main { class: "page",
            h1 { "Not found" }
            p { "There is no page /{rest.join(\"/\")}. " Link { to: Route::Home {}, "Go home" } }
        }
    }
}
