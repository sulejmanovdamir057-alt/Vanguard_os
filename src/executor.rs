use std::path::Path;
use std::process::Command;

pub struct VanguardExecutor;

impl VanguardExecutor {
    pub fn execute_script(file_id: &str) -> String {
        let target_file = format!("vanguard_gen_{}.py", file_id);

        if !Path::new(&target_file).exists() {
            return format!(">> ERROR: File [ {} ] not found on disk.", target_file);
        }

        let py_cmd = if cfg!(target_os = "windows") { "python" } else { "python3" };
        let output_result = Command::new(py_cmd).arg(&target_file).output();

        match output_result {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                if output.status.success() {
                    format!(">> EXECUTING {}...\n{}\n{}\n{}\n>> PROCESS TERMINATED WITH EXIT CODE 0.", target_file, "-".repeat(40), stdout, "-".repeat(40))
                } else {
                    format!(">> PROCESS CRASHED:\n{}\n{}\n{}\n>> EXIT CODE {}.", "-".repeat(40), stderr, "-".repeat(40), output.status.code().unwrap_or(1))
                }
            }
            Err(err) => format!(">> EXECUTION SYSTEM CRASH: {}", err),
        }
    }
}