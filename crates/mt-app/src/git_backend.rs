//! Capture Git execution identity on the UI thread before asynchronous work.
use crate::store::AppStore;
use mt_remote::GitContext;

pub fn project_context(store: &AppStore, project_id: &str) -> GitContext {
    let Some(project) = store.project(project_id) else {
        return GitContext::broken("Git project was removed".to_string());
    };
    match project.ssh_connection_id.as_deref() {
        None => GitContext::local(project.path.clone()),
        Some(connection_id) => match store
            .ssh_connections()
            .iter()
            .find(|connection| connection.id == connection_id)
        {
            Some(connection) => GitContext::remote(connection.clone(), project.path.clone()),
            None => GitContext::broken(
                "SSH connection was removed; associate a connection again".to_string(),
            ),
        },
    }
}
