use clap::Args;
use logmon_broker_sdk::Broker;

#[derive(Args, Debug)]
pub struct TriggersCmd {}

pub async fn dispatch(_broker: &Broker, _cmd: TriggersCmd, _json: bool) -> i32 {
    eprintln!("triggers subcommand not yet implemented");
    1
}
