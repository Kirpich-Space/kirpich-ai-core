use std::{env, path::PathBuf};

const DEP_ROOT_KEYS: &[&str] = &["DEP_LLAMA_ROOT", "DEP_LLAMA_LLAMA_CPP", "DEP_LLAMA_OUT_DIR"];
const OVERRIDE_KEYS: &[&str] = &["LLAMA_CPP_SYS_4_OUT_DIR", "LLAMA_CPP_SYS_4_ROOT"];
// Exact directory containing the already-built common library matching the
// llama.cpp tree selected above.
const COMMON_LIB_DIR_KEY: &str = "LLAMA_CPP_COMMON_LIB_DIR";

fn valid_llama_root(path: PathBuf) -> Option<PathBuf> {
    let root = if path.join("llama.cpp").is_dir() {
        path.join("llama.cpp")
    } else {
        path
    };
    (root.join("common").join("chat.h").is_file()
        && root.join("include").join("llama.h").is_file()
        && root.join("ggml/include").join("ggml.h").is_file())
    .then_some(root)
}

fn resolve_llama_root() -> PathBuf {
    // Cargo exposes metadata from a dependency whose manifest has `links =
    // "llama"` as DEP_LLAMA_*. Prefer that contract over filesystem probing.
    for key in DEP_ROOT_KEYS {
        if let Some(value) = env::var_os(key) {
            if let Some(root) = valid_llama_root(PathBuf::from(value)) {
                return root;
            }
            panic!("{key} was set but does not identify a usable llama.cpp tree");
        }
    }

    // This is an explicit escape hatch for dependency versions that do not
    // publish the generated-tree metadata. It must identify one exact OUT_DIR;
    // no target-directory or registry scanning is permitted.
    for key in OVERRIDE_KEYS {
        if let Some(value) = env::var_os(key) {
            if let Some(root) = valid_llama_root(PathBuf::from(value)) {
                println!(
                    "cargo:warning=using llama.cpp tree from {key}: {}",
                    root.display()
                );
                return root;
            }
            panic!("{key} was set but does not identify a usable llama.cpp tree");
        }
    }

    panic!(
        "llama-cpp-sys-4 emitted no usable DEP_LLAMA_* tree metadata; set LLAMA_CPP_SYS_4_OUT_DIR to its exact OUT_DIR"
    )
}

fn main() {
    println!("cargo:rerun-if-changed=cxx/kaic_llama_common.cpp");
    println!("cargo:rerun-if-changed=cxx/kaic_llama_common.h");
    for key in DEP_ROOT_KEYS.iter().chain(OVERRIDE_KEYS) {
        println!("cargo:rerun-if-env-changed={key}");
    }
    println!("cargo:rerun-if-env-changed={COMMON_LIB_DIR_KEY}");
    if env::var_os("CARGO_FEATURE_EMBEDDED_BACKEND").is_none() {
        return;
    }

    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let llama = resolve_llama_root();
    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .file(manifest_dir.join("cxx/kaic_llama_common.cpp"))
        .include(manifest_dir.join("cxx"))
        .include(llama.join("include"))
        .include(llama.join("ggml/include"))
        .include(llama.join("src"))
        .include(llama.join("common"))
        .include(llama.join("vendor"))
        .warnings(false)
        .compile("kaic_llama_common");

    let common_lib_dir = env::var_os(COMMON_LIB_DIR_KEY)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("{COMMON_LIB_DIR_KEY} must name the exact directory containing libllama-common-base.a"));
    if !common_lib_dir.join("libllama-common-base.a").is_file() {
        panic!(
            "{COMMON_LIB_DIR_KEY} does not contain libllama-common-base.a: {}",
            common_lib_dir.display()
        );
    }
    println!(
        "cargo:rustc-link-search=native={}",
        common_lib_dir.display()
    );
    println!("cargo:rustc-link-lib=static=llama-common-base");
}
