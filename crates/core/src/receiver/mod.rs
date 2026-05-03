pub mod gelf;
pub mod metrics;
pub mod otlp;

pub use metrics::{ReceiverDropSnapshot, ReceiverMetrics, ReceiverSource};

use async_trait::async_trait;

#[async_trait]
pub trait Receiver: Send + Sync {
    fn name(&self) -> &str;
    fn listening_on(&self) -> Vec<String>;
    async fn shutdown(self: Box<Self>);
}
