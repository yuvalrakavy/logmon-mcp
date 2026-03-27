pub mod gelf;

use async_trait::async_trait;

#[async_trait]
pub trait LogReceiver: Send + Sync {
    fn name(&self) -> &str;
    fn listening_on(&self) -> Vec<String>;
    async fn shutdown(self: Box<Self>);
}
