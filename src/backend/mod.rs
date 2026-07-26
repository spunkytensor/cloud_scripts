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
