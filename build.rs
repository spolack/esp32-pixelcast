fn main() {
    build_web();
    build_version();
    embedded_images();
    linker_be_nice();
    check_xtensa_linker_available();
    // make sure linkall.x is the last linker script (otherwise might cause problems with flip-link)
    println!("cargo:rustc-link-arg=-Tlinkall.x");
}

#[cfg(unix)]
fn check_xtensa_linker_available() {
    println!("cargo:rerun-if-env-changed=PATH");

    let target = std::env::var("TARGET").unwrap_or_default();
    println!(
        "cargo:rerun-if-env-changed={}",
        cargo_linker_env_var(&target)
    );

    let linker = xtensa_linker(&target);

    let error = match std::process::Command::new(&linker)
        .arg("--version")
        .output()
    {
        Ok(_) => return,
        Err(error) => error,
    };

    let export_file = std::env::var("HOME")
        .map(|home| format!("{home}/export-esp.sh"))
        .unwrap_or_else(|_| "$HOME/export-esp.sh".to_string());

    panic!(
        "Xtensa linker `{linker}` was not found in PATH or could not be executed: {error}.\n\n\
         Xtensa targets on Unix require espup's environment export file to be sourced before building.\n\
         Try: `source {export_file}`\n\n\
         If the export file does not exist, run `espup install` first.\n\
         For more details, see:\n\
         https://github.com/esp-rs/espup?tab=readme-ov-file#environment-variables-setup"
    );
}

#[cfg(not(unix))]
fn check_xtensa_linker_available() {}

#[cfg(unix)]
fn xtensa_linker(target: &str) -> String {
    if let Some(linker) = cargo_configured_linker(target) {
        return linker;
    }

    target
        .strip_prefix("xtensa-")
        .and_then(|target| target.strip_suffix("-none-elf"))
        .map(|chip| format!("xtensa-{chip}-elf-gcc"))
        .unwrap_or_else(|| "xtensa-esp32-elf-gcc".to_string())
}

#[cfg(unix)]
fn cargo_configured_linker(target: &str) -> Option<String> {
    std::env::var(cargo_linker_env_var(target)).ok()
}

#[cfg(unix)]
fn cargo_linker_env_var(target: &str) -> String {
    let target = target.replace('-', "_").to_ascii_uppercase();
    format!("CARGO_TARGET_{target}_LINKER")
}

fn linker_be_nice() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        let kind = &args[1];
        let what = &args[2];

        match kind.as_str() {
            "undefined-symbol" => match what.as_str() {
                what if what.starts_with("_defmt_") => {
                    eprintln!();
                    eprintln!(
                        "💡 `defmt` not found - make sure `defmt.x` is added as a linker script and you have included `use defmt_rtt as _;`"
                    );
                    eprintln!();
                }
                "_stack_start" => {
                    eprintln!();
                    eprintln!("💡 Is the linker script `linkall.x` missing?");
                    eprintln!();
                }
                what if what.starts_with("esp_rtos_") => {
                    eprintln!();
                    eprintln!(
                        "💡 `esp-radio` has no scheduler enabled. Make sure you have initialized `esp-rtos` or provided an external scheduler."
                    );
                    eprintln!();
                }
                "embedded_test_linker_file_not_added_to_rustflags" => {
                    eprintln!();
                    eprintln!(
                        "💡 `embedded-test` not found - make sure `embedded-test.x` is added as a linker script for tests"
                    );
                    eprintln!();
                }
                "free"
                | "malloc"
                | "calloc"
                | "get_free_internal_heap_size"
                | "malloc_internal"
                | "realloc_internal"
                | "calloc_internal"
                | "free_internal" => {
                    eprintln!();
                    eprintln!(
                        "💡 Did you forget the `esp-alloc` dependency or didn't enable the `compat` feature on it?"
                    );
                    eprintln!();
                }
                _ => (),
            },
            // we don't have anything helpful for "missing-lib" yet
            _ => {
                std::process::exit(1);
            }
        }

        std::process::exit(0);
    }

    println!(
        "cargo:rustc-link-arg=-Wl,--error-handling-script={}",
        std::env::current_exe().unwrap().display()
    );
}

/// Turns the pages in `web/` into one gzipped file each, in `OUT_DIR`:
/// `<!--COMMON_CSS-->` / `<!--COMMON_JS-->` are replaced with `common.css` /
/// `common.js`, then HTML, CSS and JS are minified and the result gzipped.
/// The firmware serves these files as they are (`Content-Encoding: gzip`).
fn build_web() {

    let dir = std::path::Path::new("web");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    println!("cargo:rerun-if-changed=web");
    let read = |name: &str| {
        let path = dir.join(name);
        println!("cargo:rerun-if-changed={}", path.display());
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
    };
    let css = read("common.css");
    let js = read("common.js");

    let mut cfg = minify_html::Cfg::new();
    cfg.minify_css = true;
    cfg.minify_js = true;
    cfg.keep_closing_tags = true;
    cfg.keep_html_and_head_opening_tags = true;

    for page in ["index", "artnet", "config"] {
        let src = read(&format!("{page}.html"));
        assert!(
            src.contains("<!--COMMON_CSS-->") && src.contains("<!--COMMON_JS-->"),
            "web/{page}.html must contain both <!--COMMON_CSS--> and <!--COMMON_JS-->"
        );
        let html = src.replace("<!--COMMON_CSS-->", &css).replace("<!--COMMON_JS-->", &js);
        let min = minify_html::minify(html.as_bytes(), &cfg);
        let mut gz = Vec::new();
        zopfli::compress(zopfli::Options::default(), zopfli::Format::Gzip, &min[..], &mut gz).unwrap();
        std::fs::write(out.join(format!("{page}.html.gz")), &gz).unwrap();
        println!(
            "cargo:warning=web/{page}.html: {} -> {} minified -> {} gzipped bytes",
            html.len(),
            min.len(),
            gz.len()
        );
    }
}

/// PXC_VERSION ("0.1.0-1fee900", "-dirty" with uncommitted changes),
/// PXC_BUILD_DATE and PXC_BUILD_TIME for the app descriptor, which the update
/// page shows per slot. Re-run on every source or commit change.
fn build_version() {
    for p in ["src", "core/src", "Cargo.toml", ".git/HEAD", ".git/index", ".git/refs/heads"] {
        println!("cargo:rerun-if-changed={p}");
    }
    let run = |cmd: &str, args: &[&str]| {
        std::process::Command::new(cmd)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let mut version = std::env::var("CARGO_PKG_VERSION").unwrap();
    if let Some(hash) = run("git", &["rev-parse", "--short=7", "HEAD"]) {
        version += &format!("-{hash}");
        if run("git", &["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty()) {
            version += "-dirty";
        }
    }
    version.truncate(31);
    let date = run("date", &["+%Y-%m-%d"]).unwrap_or_default();
    let time = run("date", &["+%H:%M:%S"]).unwrap_or_default();
    println!("cargo:rustc-env=PXC_VERSION={version}");
    println!("cargo:rustc-env=PXC_BUILD_DATE={date}");
    println!("cargo:rustc-env=PXC_BUILD_TIME={time}");
}

/// The bootloader and the in-place updater the firmware carries:
/// PXC_BOOTLOADER_BIN / PXC_UPDATER_BIN when set (tools/build.sh passes the
/// ones it just built), else the prebuilt copies in the repository.
fn embedded_images() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    for (var, default) in [
        ("PXC_BOOTLOADER_BIN", "tools/bootloader/bootloader.bin"),
        ("PXC_UPDATER_BIN", "updater/updater.bin"),
    ] {
        println!("cargo:rerun-if-env-changed={var}");
        let path = std::env::var(var).unwrap_or_else(|_| format!("{dir}/{default}"));
        println!("cargo:rerun-if-changed={path}");
        println!("cargo:rustc-env={var}={path}");
    }
}
