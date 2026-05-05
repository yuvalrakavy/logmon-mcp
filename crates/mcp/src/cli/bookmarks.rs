use clap::Args;
use logmon_broker_sdk::Broker;

#[derive(Args, Debug)]
pub struct BookmarksCmd {}

pub async fn dispatch(_broker: &Broker, _cmd: BookmarksCmd, _json: bool) -> i32 {
    eprintln!("bookmarks subcommand not yet implemented");
    1
}
