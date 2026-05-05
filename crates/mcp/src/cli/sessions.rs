use clap::Args;
use logmon_broker_sdk::Broker;

#[derive(Args, Debug)]
pub struct SessionsCmd {}

pub async fn dispatch(_broker: &Broker, _cmd: SessionsCmd, _json: bool) -> i32 {
    eprintln!("sessions subcommand not yet implemented");
    1
}
