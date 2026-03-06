// Public library interface for use in integration tests.
pub mod conditional;
pub mod config;
pub mod errors;
pub mod etag;
pub mod handlers;
pub mod multipart_state;
pub mod range;
pub mod s3_xml_compat;
pub mod store;

use axum::Router;
use handlers::{
    get_object::get_object,
    multipart::{delete_dispatch, post_dispatch},
    put_object::put_dispatch,
};
use multipart_state::{SharedUploadState, new_shared_upload_state};
use store::SharedStore;

/// Combined application state threaded through all handlers.
#[derive(Clone)]
pub struct AppState {
    pub store: SharedStore,
    pub uploads: SharedUploadState,
}

/// Build the Axum application router with the given shared store.
/// Extracted so integration tests can spin up a server without repeating setup.
pub fn build_app(store: SharedStore) -> Router {
    let state = AppState {
        store,
        uploads: new_shared_upload_state(),
    };
    Router::new()
        .route(
            "/{*key}",
            axum::routing::get(get_object)
                .put(put_dispatch)
                .post(post_dispatch)
                .delete(delete_dispatch),
        )
        .with_state(state)
        .layer(tower_http::trace::TraceLayer::new_for_http())
}
