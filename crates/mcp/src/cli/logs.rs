use clap::Args;
use logmon_broker_sdk::Broker;

#[derive(Args, Debug)]
pub struct LogsCmd {}

pub async fn dispatch(_broker: &Broker, _cmd: LogsCmd, _json: bool) -> i32 {
    eprintln!("logs subcommand not yet implemented");
    1
}
