use std::{env, path::PathBuf, process::Command};

fn run(command: &mut Command) {
    let status = command
        .status()
        .expect("a C compiler is required for zinput-x11");
    assert!(
        status.success(),
        "zinput-x11 compilation failed: {command:?}"
    );
}

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    println!("cargo:rerun-if-changed=native");
    for variable in [
        "CC",
        "AR",
        "CFLAGS",
        "PKG_CONFIG",
        "PKG_CONFIG_PATH",
        "PKG_CONFIG_LIBDIR",
    ] {
        println!("cargo:rerun-if-env-changed={variable}");
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo provides OUT_DIR"));
    let pkg = env::var("PKG_CONFIG").unwrap_or_else(|_| "pkg-config".into());
    let packages = ["xcb-imdkit", "xcb", "xcb-shape", "xkbcommon-x11"];
    let flags = Command::new(&pkg)
        .arg("--cflags")
        .args(packages)
        .output()
        .expect("pkg-config is required for zinput-x11");
    assert!(
        flags.status.success(),
        "install xcb-imdkit, XCB Shape and xkbcommon-x11 development libraries"
    );
    let mut compiler = Command::new(env::var("CC").unwrap_or_else(|_| "cc".into()));
    compiler
        .args([
            "-std=c11",
            "-D_DEFAULT_SOURCE",
            "-O2",
            "-fPIC",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-Inative",
            "-c",
            "native/x11.c",
            "-o",
        ])
        .arg(out.join("x11.o"));
    compiler.args(
        String::from_utf8(flags.stdout)
            .expect("UTF-8 compiler flags")
            .split_whitespace(),
    );
    if let Ok(flags) = env::var("CFLAGS") {
        compiler.args(flags.split_whitespace());
    }
    run(&mut compiler);
    run(Command::new(env::var("AR").unwrap_or_else(|_| "ar".into()))
        .arg("crs")
        .arg(out.join("libzinput_x11_native.a"))
        .arg(out.join("x11.o")));
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=zinput_x11_native");
    let libraries = Command::new(&pkg)
        .arg("--libs")
        .args(packages)
        .output()
        .expect("pkg-config is required for zinput-x11");
    assert!(
        libraries.status.success(),
        "cannot resolve XIM system libraries"
    );
    for flag in String::from_utf8(libraries.stdout)
        .expect("UTF-8 linker flags")
        .split_whitespace()
    {
        if let Some(name) = flag.strip_prefix("-l") {
            println!("cargo:rustc-link-lib={name}");
        } else if let Some(path) = flag.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={path}");
        } else {
            println!("cargo:rustc-link-arg={flag}");
        }
    }
}
