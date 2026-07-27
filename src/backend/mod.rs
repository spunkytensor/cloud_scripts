// SPDX-FileCopyrightText: 2026 Matt Curfman
// SPDX-License-Identifier: Apache-2.0

pub mod digitalocean;
use crate::{
    error::Result,
    model::{Action, CreateRecipe, Server, Snapshot},
};
use async_trait::async_trait;

#[derive(Debug, Clone)]
pub enum Mutation<T> {
    Confirmed(T),
    Rejected { diagnostic: String },
    Uncertain { diagnostic: String },
}

#[async_trait]
pub trait Backend: Send + Sync {
    /// Verifies that the configured credentials can perform lifecycle operations.
    async fn validate_access(&self) -> Result<()>;
    /// Lists account SSH key identifiers; backends without account keys may return an empty list.
    async fn ssh_keys(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    /// Uses an explicitly configured SSH key, or requires exactly one discoverable account key.
    async fn resolve_ssh_key(&self, configured: Option<&str>) -> Result<String> {
        if let Some(key) = configured.filter(|key| !key.trim().is_empty()) {
            return Ok(key.to_owned());
        }
        select_ssh_key(self.ssh_keys().await?)
    }
    /// Creates a server from the exact recipe and optional bootstrap data.
    ///
    /// `Mutation::Uncertain` means callers must reconcile by the recipe's correlation tag
    /// before submitting another request.
    async fn create_server(
        &self,
        recipe: &CreateRecipe,
        user_data: Option<&str>,
    ) -> Result<Mutation<Server>>;
    /// Lists every server carrying the exact correlation tag.
    async fn find_servers(&self, tag: &str) -> Result<Vec<Server>>;
    /// Fetches server by identifier, returning `None` when the provider confirms it is absent.
    async fn get_server(&self, id: &str) -> Result<Option<Server>>;
    /// Deletes a server, returning `Mutation::Uncertain` unless completion is confirmed.
    async fn delete_server(&self, id: &str) -> Result<Mutation<()>>;
    /// Starts a named server action; `name` supplies an action-specific resource name.
    /// An uncertain result must be reconciled with `find_actions` before retrying.
    async fn action(&self, id: &str, kind: &str, name: Option<&str>) -> Result<Mutation<Action>>;
    /// Fetches action by identifier, returning `None` when the provider confirms it is absent.
    async fn get_action(&self, id: &str) -> Result<Option<Action>>;
    /// Finds actions of `kind` for one resource that started no earlier than `since`.
    async fn find_actions(&self, resource_id: &str, kind: &str, since: &str)
    -> Result<Vec<Action>>;
    /// Polls until the expected provider action completes or fails.
    async fn wait_action(&self, action: &Action) -> Result<()>;
    /// Lists all server snapshots visible to the configured account.
    async fn snapshots(&self) -> Result<Vec<Snapshot>>;
    /// Fetches snapshot by identifier, returning `None` when the provider confirms it is absent.
    async fn get_snapshot(&self, id: &str) -> Result<Option<Snapshot>>;
    /// Deletes a snapshot, returning `Mutation::Uncertain` unless completion is confirmed.
    async fn delete_snapshot(&self, id: &str) -> Result<Mutation<()>>;
}

/// Selects the sole account SSH key, rejecting absent or ambiguous key inventories.
fn select_ssh_key(keys: Vec<String>) -> Result<String> {
    match keys.as_slice() {
        [key] => Ok(key.clone()),
        [] => Err(crate::error::Error::Cli(
            "DigitalOcean account has no SSH keys; add one and set backends.digitalocean.ssh_key"
                .into(),
        )),
        _ => Err(crate::error::Error::Cli(format!(
            "DigitalOcean account has {} SSH keys; set backends.digitalocean.ssh_key explicitly",
            keys.len()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::select_ssh_key;

    /// Implicit SSH-key selection succeeds only for a single account key.
    #[test]
    fn implicit_ssh_key_requires_exactly_one_account_key() {
        assert_eq!(select_ssh_key(vec!["42".into()]).unwrap(), "42");
        assert!(select_ssh_key(vec![]).is_err());
        assert!(select_ssh_key(vec!["1".into(), "2".into()]).is_err());
    }
}
