use std::{env, path::PathBuf, process::Command};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let web_output = PathBuf::from(env::var("OUT_DIR")?).join("web");
    if !root.join("node_modules/typescript/bin/tsc").is_file() {
        return Err("Missing TypeScript compiler. Run `make build` or `npm ci --include=dev --ignore-scripts` before building with Cargo.".into());
    }
    let status = Command::new("node")
        .arg(root.join("scripts/build-web.ts"))
        .arg(web_output)
        .current_dir(&root)
        .status()
        .map_err(|error| format!("UI build requires Node.js 22.18 or newer: {error}"))?;
    if !status.success() {
        return Err("TypeScript UI build failed. Run `npm run build:web` to reproduce it.".into());
    }
    for path in [
        "web",
        "scripts/build-web.ts",
        "tsconfig.json",
        "tsconfig.web.json",
        "package.json",
        "package-lock.json",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }

    let mut prost = tonic_prost_build::Config::new();
    prost.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    tonic_prost_build::configure()
        .build_server(false)
        .compile_with_config(prost, &["proto/service.proto"], &["proto"])?;
    println!("cargo:rerun-if-changed=proto");
    Ok(())
}
