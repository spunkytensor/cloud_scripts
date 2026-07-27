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
    async fn validate_access(&self) -> Result<()>;
    async fn ssh_keys(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    async fn resolve_ssh_key(&self, configured: Option<&str>) -> Result<String> {
        if let Some(key) = configured.filter(|key| !key.trim().is_empty()) {
            return Ok(key.to_owned());
        }
        select_ssh_key(self.ssh_keys().await?)
    }
    async fn create_server(
        &self,
        recipe: &CreateRecipe,
        user_data: Option<&str>,
    ) -> Result<Mutation<Server>>;
    async fn find_servers(&self, tag: &str) -> Result<Vec<Server>>;
    async fn get_server(&self, id: &str) -> Result<Option<Server>>;
    async fn delete_server(&self, id: &str) -> Result<Mutation<()>>;
    async fn action(&self, id: &str, kind: &str, name: Option<&str>) -> Result<Mutation<Action>>;
    async fn get_action(&self, id: &str) -> Result<Option<Action>>;
    async fn find_actions(&self, resource_id: &str, kind: &str, since: &str)
    -> Result<Vec<Action>>;
    async fn wait_action(&self, action: &Action) -> Result<()>;
    async fn snapshots(&self) -> Result<Vec<Snapshot>>;
    async fn get_snapshot(&self, id: &str) -> Result<Option<Snapshot>>;
    async fn delete_snapshot(&self, id: &str) -> Result<Mutation<()>>;
}

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

    #[test]
    fn implicit_ssh_key_requires_exactly_one_account_key() {
        assert_eq!(select_ssh_key(vec!["42".into()]).unwrap(), "42");
        assert!(select_ssh_key(vec![]).is_err());
        assert!(select_ssh_key(vec!["1".into(), "2".into()]).is_err());
    }
}
