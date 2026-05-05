use clap::Args;
use logmon_broker_sdk::Broker;

#[derive(Args, Debug)]
pub struct SpansCmd {}

pub async fn dispatch(_broker: &Broker, _cmd: SpansCmd, _json: bool) -> i32 {
    eprintln!("spans subcommand not yet implemented");
    1
}
