use std::process::ExitCode;

fn main() -> ExitCode {
    match statsai_dev::run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}
