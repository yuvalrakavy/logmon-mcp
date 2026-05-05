use clap::Args;
use logmon_broker_sdk::Broker;

#[derive(Args, Debug)]
pub struct TracesCmd {}

pub async fn dispatch(_broker: &Broker, _cmd: TracesCmd, _json: bool) -> i32 {
    eprintln!("traces subcommand not yet implemented");
    1
}
