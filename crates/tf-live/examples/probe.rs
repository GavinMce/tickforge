//! Checks the gateway and the key without asking for any data.
//!
//! `cargo run --release -p tf-live --example probe -- [DATASET] [KEYFILE]`
//!
//! Connects to the dataset's live gateway, reads its greeting, logs in with the key (default
//! `~/.config/tickforge/databento.key`) and says what the gateway answered, then hangs up before
//! subscribing to anything. The key is never printed beyond its last five characters.

use tf_live::{ApiKey, Config, login};

fn main() {
    let dataset = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "XNAS.BASIC".to_owned());
    let path = std::env::args().nth(2).unwrap_or_else(|| {
        format!(
            "{}/.config/tickforge/databento.key",
            std::env::var("HOME").unwrap_or_default()
        )
    });
    let key = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let key = ApiKey::new(&key).unwrap_or_else(|e| panic!("{e}"));
    println!(
        "key {key:?}, dataset {dataset}, gateway {}",
        tf_live::protocol::gateway_for(&dataset)
    );
    match login(&Config::new(key, &dataset, vec![])) {
        Ok(l) => println!("logged in: session {}", l.session_id),
        Err(e) => println!("not logged in: {e}"),
    }
}
