use std::{env, path::PathBuf};

fn main() {
    let source = "../vendor/webkit/helper.c";
    println!("cargo:rerun-if-changed={source}");
    let mut compiler = cc::Build::new().get_compiler().to_command();
    let mut paths = Vec::new();
    let mut libs = Vec::new();
    for library in ["webkit2gtk-4.1", "json-glib-1.0"] {
        let package = pkg_config::Config::new()
            .cargo_metadata(false)
            .probe(library)
            .unwrap_or_else(|error| {
                panic!("Install the development package for {library}: {error}")
            });
        for include in package.include_paths {
            compiler.arg("-I").arg(include);
        }
        paths.extend(package.link_paths);
        libs.extend(package.libs);
    }
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("power-webkit");
    compiler.args(["-O2", source, "-o"]).arg(output);
    for path in paths {
        compiler.arg("-L").arg(path);
    }
    for library in libs {
        compiler.arg(format!("-l{library}"));
    }
    assert!(
        compiler.status().unwrap().success(),
        "Could not build WebKit helper"
    );
}
