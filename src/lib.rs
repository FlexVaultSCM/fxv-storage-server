// Public library interface for use in integration tests.
pub mod config;
pub mod conditional;
pub mod errors;
pub mod etag;
pub mod handlers;
pub mod range;
pub mod store;

use axum::Router;
use handlers::{
    get_object::get_object,
    multipart::{delete_dispatch, post_dispatch},
    put_object::put_dispatch,
};
use store::SharedStore;

/// Build the Axum application router with the given shared store.
/// Extracted so integration tests can spin up a server without repeating setup.
pub fn build_app(store: SharedStore) -> Router {
    Router::new()
        .route(
            "/{*key}",
            axum::routing::get(get_object)
                .put(put_dispatch)
                .post(post_dispatch)
                .delete(delete_dispatch),
        )
        .with_state(store)
        .layer(tower_http::trace::TraceLayer::new_for_http())
}
