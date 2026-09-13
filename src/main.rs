use azalea::prelude::*;
use azalea::Client;
use azalea::account::Account;

#[tokio::main]
async fn main() {
    tokio::task::LocalSet::new().run_until(run()).await;
}

async fn run() {
    let address = std::env::args().nth(1).unwrap_or_else(|| "localhost:25565".into());
    let (client, mut events) = Client::join(Account::offline("spike"), address.as_str())
        .await
        .expect("resolve");

    while let Some(event) = events.recv().await {
        match event {
            Event::Spawn => println!("SPAWN at {:?}", client.position()),
            Event::Chat(message) => println!("CHAT {}", message.message().to_ansi()),
            Event::Disconnect(reason) => {
                println!("DISCONNECT {reason:?}");
                break;
            }
            _ => {}
        }
    }
}
