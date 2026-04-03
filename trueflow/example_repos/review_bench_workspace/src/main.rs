use clap::{Parser, Subcommand};
use review_bench_workspace::config::{AppConfig, ReviewMode};
use review_bench_workspace::default_state;
use review_bench_workspace::store::MemoryStore;

#[derive(Parser, Debug)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    Review {
        #[arg(long, default_value = "incremental")]
        mode: String,
    },
    PrintConfig,
}

fn main() {
    let cli = Cli::parse();
    let config = AppConfig::default();
    let state = default_state();
    let store = MemoryStore::default();

    match cli.command {
        Command::Review { mode } => {
            let review_mode = match mode.as_str() {
                "full" => ReviewMode::Full,
                _ => ReviewMode::Incremental,
            };
            let result = state.run_review(&store, review_mode);
            println!("review result: {result:?}");
        }
        Command::PrintConfig => {
            println!("{:?}", AppConfig {
                repository_name: config.repository_name,
                include_generated: config.include_generated,
                review_batch_size: config.review_batch_size,
                max_file_bytes: config.max_file_bytes,
            });
        }
    }
}
