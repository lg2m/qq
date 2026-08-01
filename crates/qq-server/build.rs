use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"));
    let web = manifest.join("../../web");
    for path in [
        web.join("src"),
        web.join("index.html"),
        web.join("package.json"),
        web.join("package-lock.json"),
        web.join("tsconfig.json"),
        web.join("vite.config.ts"),
    ] {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"));
    let dist = out.join("qq-web");
    let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
    if !web.join("node_modules").is_dir() {
        run(
            &web,
            npm,
            &["ci", "--ignore-scripts", "--no-audit", "--no-fund"],
        );
    } else {
        run(
            &web,
            npm,
            &[
                "install",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                "--prefer-offline",
            ],
        );
    }
    run(
        &web,
        npm,
        &[
            "run",
            "build",
            "--",
            "--outDir",
            dist.to_str().expect("Cargo output path is Unicode"),
            "--emptyOutDir",
        ],
    );

    let mut files = Vec::new();
    collect_files(&dist, &dist, &mut files);
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut generated = String::from("pub(crate) static WEB_ASSETS: &[(&str, &str, &[u8])] = &[\n");
    for (route, path) in files {
        if route == "/.vite/manifest.json" {
            continue;
        }
        let content_type = match path.extension().and_then(|value| value.to_str()) {
            Some("html") => "text/html; charset=utf-8",
            Some("js") => "text/javascript; charset=utf-8",
            Some("css") => "text/css; charset=utf-8",
            Some("svg") => "image/svg+xml",
            Some("png") => "image/png",
            Some("woff2") => "font/woff2",
            _ => "application/octet-stream",
        };
        generated.push_str(&format!(
            "    ({route:?}, {content_type:?}, include_bytes!({path:?})),\n",
            path = path.to_string_lossy(),
        ));
    }
    generated.push_str("];\n");
    fs::write(out.join("web_assets.rs"), generated).expect("write embedded web asset table");
}

fn run(directory: &Path, program: &str, arguments: &[&str]) {
    let status = Command::new(program)
        .args(arguments)
        .current_dir(directory)
        .status()
        .unwrap_or_else(|error| {
            panic!(
                "could not run {program} for the QQ web frontend: {error}; install Node.js and npm"
            )
        });
    assert!(status.success(), "{program} {} failed", arguments.join(" "));
}

fn collect_files(root: &Path, directory: &Path, output: &mut Vec<(String, PathBuf)>) {
    for entry in fs::read_dir(directory).expect("read built web directory") {
        let entry = entry.expect("read built web entry");
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, output);
        } else {
            let relative = path
                .strip_prefix(root)
                .expect("web asset under output root");
            let route = format!("/{}", relative.to_string_lossy().replace('\\', "/"));
            output.push((route, path));
        }
    }
}
