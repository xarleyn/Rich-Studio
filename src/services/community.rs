use crate::{
    AppState,
    services::{invite_links::InviteLinks, suggested_posts::SuggestedPosts},
};
pub async fn links(state: &AppState) -> InviteLinks {
    InviteLinks::new(
        state.account.clone(),
        crate::services::posts::PostService::new(
            state.account.clone(),
            state.transport(),
            &state.settings,
            state.spec.read().await.clone(),
        ),
        state.transport(),
    )
}
pub fn suggestions(state: &AppState) -> SuggestedPosts {
    SuggestedPosts::new(state.account.clone(), state.transport())
}
