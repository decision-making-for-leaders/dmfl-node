mod cli;
mod guards;
mod provision;
mod validate;

use std::env;
use std::process::ExitCode;

use cli::{parse_args, usage, CliOutcome};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match parse_args(&args) {
        Ok(CliOutcome::Help) => {
            println!("{}", usage());
            ExitCode::SUCCESS
        }
        Ok(CliOutcome::Run(config)) => match provision::run(config) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::from(1)
            }
        },
        Err(err) => {
            eprintln!("error: {err}");
            if err.show_usage {
                eprintln!("{}", usage());
            }
            ExitCode::from(2)
        }
    }
}
