mod config;
mod template;
mod workflow;

use config::Config;

fn main() {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());

    let config = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error loading config from {config_path}: {e}");
            std::process::exit(1);
        }
    };

    let workflows_dir = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "workflows".to_string());
    let workflows = match workflow::load_workflows(&workflows_dir) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("Error loading workflows from {workflows_dir}: {e}");
            std::process::exit(1);
        }
    };

    // Validate that all agents referenced in workflow steps exist in config
    if let Err(e) = config::resolve_agents(&config, &workflows) {
        eprintln!("Configuration error: {e}");
        std::process::exit(1);
    }

    println!(
        "Configuration and {} workflow(s) loaded and validated successfully",
        workflows.len()
    );
}
