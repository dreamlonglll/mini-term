//! Capture Git execution identity on the UI thread before asynchronous work.
//! Helper consent is project-scoped and bound to the displayed execution target.
//! Consent separates the project/SSH target, repository path and helper cache path.
//! Local contexts also retain the initiating project across repository selection.
use crate::i18n::t;
use crate::store::AppStore;
use crate::ui;
use gpui::{App, ClickEvent, Entity, FontWeight, ParentElement, Styled, Window, div, px};
use mt_config::{AppConfig, ProjectConfig};
use mt_remote::GitContext;

pub fn project_context(store: &AppStore, project_id: &str) -> GitContext {
    let Some(project) = store.project(project_id) else {
        return GitContext::broken(t("gitHistory", "projectRemoved").to_string());
    };
    match project.ssh_connection_id.as_deref() {
        None => GitContext::local_for_project(project.id.clone(), project.path.clone()),
        Some(connection_id) => match store
            .ssh_connections()
            .iter()
            .find(|connection| connection.id == connection_id)
        {
            Some(connection) if project.remote_git_enabled(connection) => {
                GitContext::remote(project.id.clone(), connection.clone(), project.path.clone())
            }
            Some(_) => GitContext::broken(t("gitHistory", "remoteDisabled").to_string())
                .with_path(project.path.clone()),
            None => GitContext::broken(t("gitHistory", "connectionRemoved").to_string()),
        },
    }
}

/// Pure identity lookup for recovery metadata; ordinary Git still uses project_context.
pub fn recovery_context(store: &AppStore, project_id: &str) -> Option<GitContext> {
    let project = store.project(project_id)?;
    let connection = store
        .ssh_connections()
        .iter()
        .find(|connection| project.ssh_connection_id.as_deref() == Some(connection.id.as_str()))?;
    Some(GitContext::remote(
        project.id.clone(),
        connection.clone(),
        project.path.clone(),
    ))
}

pub fn remote_git_enabled(store: &AppStore, project_id: &str) -> bool {
    let Some(project) = store.project(project_id) else {
        return false;
    };
    store
        .ssh_connections()
        .iter()
        .any(|connection| project.remote_git_enabled(connection))
}

/// Registry updates never perform SSH I/O; a false grant revokes only this project.
pub(crate) fn sync_remote_git_permissions(config: &AppConfig) {
    for project in &config.projects {
        let connection = config.ssh_connections.iter().find(|connection| {
            project.ssh_connection_id.as_deref() == Some(connection.id.as_str())
        });
        mt_remote::set_project_git_permission(
            &project.id,
            connection,
            &project.path,
            connection.is_some_and(|connection| project.remote_git_enabled(connection)),
        );
    }
}

/// Includes the process-local credential identity without persisting a password hash.
#[derive(Clone, Debug, PartialEq, Eq)]
struct EnableTarget {
    project_id: String,
    connection_fingerprint: u64,
    root: String,
}

impl EnableTarget {
    fn capture(project: &ProjectConfig, config: &AppConfig) -> Option<Self> {
        let connection = config.ssh_connections.iter().find(|connection| {
            project.ssh_connection_id.as_deref() == Some(connection.id.as_str())
        })?;
        Some(Self {
            project_id: project.id.clone(),
            connection_fingerprint: mt_remote::connection_fingerprint(connection),
            root: project.path.clone(),
        })
    }
}

pub fn request_enable(
    store: Entity<AppStore>,
    project_id: String,
    window: &mut Window,
    cx: &mut App,
) {
    let state = store.read(cx);
    if remote_git_enabled(state, &project_id) {
        return;
    }
    let Some(project) = state.project(&project_id) else {
        crate::prompt::show_alert(
            t("gitHistory", "enableTitle"),
            t("gitHistory", "projectRemoved"),
            window,
            cx,
        );
        return;
    };
    let Some(target) = EnableTarget::capture(project, state.config()) else {
        crate::prompt::show_alert(
            t("gitHistory", "enableTitle"),
            t("gitHistory", "connectionRemoved"),
            window,
            cx,
        );
        return;
    };
    let Some(connection) = state
        .ssh_connections()
        .iter()
        .find(|connection| project.ssh_connection_id.as_deref() == Some(connection.id.as_str()))
    else {
        return;
    };
    let project_name = project.name.clone();
    let connection_name = connection.name.clone();
    let endpoint = format!(
        "{}@{}:{}",
        connection.user, connection.host, connection.port
    );
    let project_path = project.path.clone();
    crate::prompt::open_guarded(
        crate::prompt::kind::CONFIRM,
        window,
        cx,
        move |dialog, window, _cx| {
            let field = |label: &'static str, value: &str| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(ui::font_px(11.0))
                            .text_color(ui::text_secondary())
                            .child(t("gitHistory", label)),
                    )
                    .child(
                        div()
                            .text_size(ui::font_px(13.0))
                            .text_color(ui::text_primary())
                            .whitespace_normal()
                            .child(value.to_string()),
                    )
            };
            let content = div()
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(crate::prompt::body(
                    t("gitHistory", "enableProjectMessage"),
                    &[],
                ))
                .child(
                    ui::settings_card()
                        .flex()
                        .flex_col()
                        .gap(px(10.0))
                        .child(field("enableProjectLabel", &project_name))
                        .child(
                            field("enableConnectionLabel", &connection_name).child(
                                div()
                                    .text_size(ui::font_px(12.0))
                                    .text_color(ui::text_secondary())
                                    .child(endpoint.clone()),
                            ),
                        ),
                )
                .child(
                    ui::settings_card()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(field("enableProjectPathLabel", &project_path)),
                )
                .child(
                    ui::settings_card()
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .child(field(
                            "enableHelperPathLabel",
                            "~/.cache/mini-term/git-helper/<sha256>/mt-git-helper",
                        ))
                        .child(
                            div()
                                .text_size(ui::font_px(11.0))
                                .text_color(ui::text_secondary())
                                .child(t("gitHistory", "enableHelperPathHint")),
                        ),
                )
                .child(
                    div()
                        .border_t_1()
                        .border_color(ui::border_subtle())
                        .pt(px(10.0))
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::text_secondary())
                        .child(t("gitHistory", "enableBoundary")),
                );
            dialog
                .title(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(t("gitHistory", "enableTitle")),
                )
                .w(ui::clamp_dialog_width(px(500.0), window.viewport_size()))
                .overlay_closable(true)
                .footer(crate::prompt::confirm_footer(
                    t("gitHistory", "enableAction"),
                    Some(t("prompt", "cancel")),
                ))
                .child(content)
                .on_ok({
                    let store = store.clone();
                    let project_id = project_id.clone();
                    let target = target.clone();
                    move |_: &ClickEvent, window, cx| {
                        let state = store.read(cx);
                        let current = state
                            .project(&project_id)
                            .and_then(|project| EnableTarget::capture(project, state.config()));
                        if current.as_ref() != Some(&target) {
                            // Show the changed-target notice after this confirmation closes.
                            window.defer(cx, |window, cx| {
                                crate::prompt::show_alert(
                                    t("gitHistory", "enableTitle"),
                                    t("gitHistory", "enableTargetChanged"),
                                    window,
                                    cx,
                                );
                            });
                            return true;
                        }
                        store.update(cx, |store, cx| {
                            store.set_project_remote_git_enabled(&project_id, true, cx);
                        });
                        true
                    }
                })
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use mt_config::SshConnection;

    fn config() -> AppConfig {
        let mut config = AppConfig::default();
        config.ssh_connections.push(SshConnection {
            id: "ssh".into(),
            name: "Server".into(),
            host: "example.test".into(),
            port: 22,
            user: "user".into(),
            password: None,
            identity_file: None,
            group: None,
            extra: Default::default(),
        });
        let mut project = ProjectConfig::new("a", "A", "/srv/repo");
        project.ssh_connection_id = Some("ssh".into());
        config.projects.push(project);
        config
    }

    #[test]
    fn pending_confirmation_is_bound_to_project_endpoint_credentials_and_root() {
        let original = config();
        let target = EnableTarget::capture(&original.projects[0], &original).unwrap();
        let mutations: [fn(&mut AppConfig); 5] = [
            |config: &mut AppConfig| config.projects[0].id = "b".into(),
            |config: &mut AppConfig| config.projects[0].path = "/srv/other".into(),
            |config: &mut AppConfig| config.ssh_connections[0].host = "other.test".into(),
            |config: &mut AppConfig| config.ssh_connections[0].password = Some("changed".into()),
            |config: &mut AppConfig| config.ssh_connections[0].identity_file = Some("/key".into()),
        ];
        for mutate in mutations {
            let mut changed = original.clone();
            mutate(&mut changed);
            assert_ne!(
                EnableTarget::capture(&changed.projects[0], &changed),
                Some(EnableTarget {
                    project_id: target.project_id.clone(),
                    connection_fingerprint: target.connection_fingerprint,
                    root: target.root.clone(),
                })
            );
        }
        let mut renamed = original.clone();
        renamed.projects[0].name = "Renamed".into();
        renamed.ssh_connections[0].name = "Renamed connection".into();
        assert_eq!(
            EnableTarget::capture(&renamed.projects[0], &renamed),
            Some(target)
        );
        renamed.ssh_connections.clear();
        assert_eq!(EnableTarget::capture(&renamed.projects[0], &renamed), None);
    }
}
