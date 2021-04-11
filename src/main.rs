use std::process::exit;

/// Loads the configuration for wicrs_server from `./config.json`. Causes exit with code 1 if the file cannot be found or cannot be deserialized.
fn load_config(path: &str) -> wicrs_server::config::Config {
    if let Ok(read) = std::fs::read_to_string(path) {
        if let Ok(config) = serde_json::from_str::<wicrs_server::config::Config>(&read) {
            return config;
        } else {
            println!("config.json does not contain a valid configuration.");
            exit(1);
        }
    } else {
        println!("Failed to load config.json.");
        exit(1);
    }
}

/// Main function, loads config and starts a server for the HTTP API.
#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let config = load_config("config.json");
    wicrs_server::httpapi::server(config).await
}
