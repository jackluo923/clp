const MINIMUM_LIBARCHIVE_VERSION: &str = "3.8.0";

fn pkg_config() -> pkg_config::Config {
    let mut config = pkg_config::Config::new();
    config
        .atleast_version(MINIMUM_LIBARCHIVE_VERSION)
        .cargo_metadata(false);
    config
}

fn shared_library_path(library: &pkg_config::Library) -> std::path::PathBuf {
    library
        .link_paths
        .iter()
        .flat_map(|directory| {
            ["libarchive.so", "libarchive.dylib"].map(|filename| directory.join(filename))
        })
        .find(|candidate| candidate.is_file())
        .and_then(|candidate| candidate.canonicalize().ok())
        .expect("pkg-config found libarchive but no shared library was present in its link paths")
}

fn c_string_literal(path: &std::path::Path) -> String {
    let path = path
        .to_str()
        .expect("the selected libarchive shared-library path must be UTF-8");
    format!("\"{}\"", path.replace('\\', "\\\\").replace('\"', "\\\""))
}

fn main() {
    println!("cargo:rerun-if-changed=native/clp_s_container_shim.c");
    println!("cargo:rerun-if-changed=native/clp_s_container_shim.h");

    let library = pkg_config()
        .probe("libarchive")
        .expect("clp-s-container requires pkg-config libarchive >= 3.8.0");
    let library_path = c_string_literal(&shared_library_path(&library));

    let mut compiler = cc::Build::new();
    compiler
        .file("native/clp_s_container_shim.c")
        .include("native")
        .includes(&library.include_paths)
        .warnings(true)
        .extra_warnings(true)
        .define(
            "CLP_S_CONTAINER_LIBARCHIVE_PATH",
            Some(library_path.as_str()),
        )
        .flag_if_supported("-Wconversion")
        .flag_if_supported("-Werror=implicit-function-declaration")
        .compile("clp_s_container_shim");

    // The private shim resolves the exact pkg-config-selected shared object with dlopen instead of
    // exposing an `-larchive` dependency whose loader resolution a downstream binary could change.
    println!("cargo:rustc-link-lib=dylib=dl");
}
