fn main() {
    let mut args = std::env::args_os().skip(1);
    match args.next().as_deref() {
        Some(flag) if flag == "--membrick-wasm-worker" && args.next().is_none() => {
            if membrick_wasm_extension_runtime::run_worker().is_err() {
                std::process::exit(1);
            }
        }
        Some(flag) if flag == "--membrick-sandbox-probe" => {
            if membrick_wasm_extension_runtime::run_sandbox_probe(args).is_err() {
                std::process::exit(1);
            }
        }
        _ => std::process::exit(2),
    }
}
