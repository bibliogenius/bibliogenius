use crate::api;
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(
    paths(
        api::health::health_check,
        api::books::list_books,
        api::books::create_book,
        api::books::update_book,
        api::books::delete_book,
        // Add other endpoints here as we document them
    ),
    components(
        schemas(
            // We will need to derive ToSchema for our models
            // crate::models::book::Book,
        )
    ),
    tags(
        (name = "bibliogenius", description = "BiblioGenius API")
    )
)]
pub struct ApiDoc;
