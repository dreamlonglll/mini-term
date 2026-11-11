//! Capture Git execution identity on the UI thread before asynchronous work.
use crate::i18n::t;
use crate::store::AppStore;
use gpui::{App, Entity, Window};
use mt_remote::GitContext;

pub fn project_context(store: &AppStore, project_id: &str) -> GitContext {
    let Some(project) = store.project(project_id) else {
        return GitContext::broken(t("gitHistory", "projectRemoved").to_string());
    };
    match project.ssh_connection_id.as_deref() {
        None => GitContext::local(project.path.clone()),
        Some(_) if !remote_git_enabled(store) => {
            GitContext::broken(t("gitHistory", "remoteDisabled").to_string())
                .with_path(project.path.clone())
        }
        Some(connection_id) => match store
            .ssh_connections()
            .iter()
            .find(|connection| connection.id == connection_id)
        {
            Some(connection) => GitContext::remote(connection.clone(), project.path.clone()),
            None => GitContext::broken(t("gitHistory", "connectionRemoved").to_string()),
        },
    }
}

pub fn remote_git_enabled(store: &AppStore) -> bool {
    store.config().remote_git_helper_enabled
}

pub fn request_enable(store: Entity<AppStore>, window: &mut Window, cx: &mut App) {
    if remote_git_enabled(store.read(cx)) {
        return;
    }
    crate::prompt::Confirm::new(
        t("gitHistory", "enableTitle"),
        t("gitHistory", "enableMessage"),
    )
    .detail(vec![
        "~/.cache/mini-term/git-helper/<sha256>/mt-git-helper".to_string(),
        t("gitHistory", "enableBoundary").to_string(),
    ])
    .ok_text(t("gitHistory", "enableAction"))
    .open(
        move |_, cx| {
            store.update(cx, |store, cx| {
                store.patch_config(|config| config.remote_git_helper_enabled = true, cx);
            });
        },
        window,
        cx,
    );
}
