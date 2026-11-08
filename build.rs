// Compile ShadowHook (bytedance/android-inline-hook v2.0.1, MIT; C) from the
// third_party/android-inline-hook submodule into the cdylib.
//
// Its CMake build produces libshadowhook.so + libshadowhook_nothing.so; we
// compile the former statically into libjudgehook.so (minus the JNI glue) and
// build the latter as a separate tiny .so next to libjudgehook.so in
// target/<triple>/<profile>/: shadowhook_init() dlopens it by name to learn
// bionic's soinfo layout and fails without it, so it must ship in the APK.
use std::{
    env, fs,
    path::{Path, PathBuf},
};

fn main() {
    // host builds (cargo test on the dev machine) only exercise the pure-Rust parts
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }
    build_shadowhook();

    // __builtin___clear_cache lowers to a call to __clear_cache, which lives in
    // compiler-rt's static builtins. Bionic does not export it and rustc does not
    // link the builtins archive, so without this the .so fails to dlopen with:
    // cannot locate symbol "__clear_cache".
    // Must come after the shadowhook archive on the link line (static archive order).
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let lib = format!("clang_rt.builtins-{arch}-android");
    let dir = find_ndk_builtins(&format!("lib{lib}.a"))
        .unwrap_or_else(|| panic!("lib{lib}.a not found under $ANDROID_NDK_HOME (run inside: nix develop .nix-dev/profile)"));
    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=static={lib}");

    // ShadowHook logs with __android_log_print
    println!("cargo:rustc-link-lib=log");
}

fn build_shadowhook() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("third_party/android-inline-hook/shadowhook/src/main/cpp");
    println!("cargo:rerun-if-changed={}", root.display());
    assert!(
        root.join("shadowhook.c").is_file(),
        "{}: missing (run: git submodule update --init)",
        root.display()
    );
    let arch = match env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
        "aarch64" => "arm64",
        "arm" => "arm",
        a => panic!("shadowhook: unsupported arch {a}"),
    };

    let mut files = Vec::new();
    for dir in [root.clone(), root.join("arch").join(arch), root.join("common")]
        .into_iter()
        .chain(fs::read_dir(root.join("third_party")).unwrap().flatten().map(|e| e.path()))
    {
        for f in fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()) {
            let ext = f.extension().and_then(|e| e.to_str());
            // sh_jni.c is the Java binding (exports JNI_OnLoad): not wanted here
            if matches!(ext, Some("c" | "S")) && f.file_name().unwrap() != "sh_jni.c" {
                files.push(f);
            }
        }
    }
    files.sort();

    cc::Build::new()
        .files(&files)
        .includes([
            root.clone(),
            root.join("include"),
            root.join("arch").join(arch),
            root.join("common"),
            root.join("third_party/xdl"),
            root.join("third_party/bsd"),
            root.join("third_party/lss"),
        ])
        .flag("-std=c11")
        .flag("-ffunction-sections")
        .flag("-fdata-sections")
        .opt_level_str("s")
        .warnings(false)
        .compile("shadowhook");

    // libshadowhook_nothing.so: same flags as upstream CMakeLists.txt
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let nothing = out.join("libshadowhook_nothing.so");
    let mut cmd = cc::Build::new().get_compiler().to_command();
    cmd.args([
        "-shared", "-Oz", "-fno-ident", "-fno-unwind-tables", "-fno-asynchronous-unwind-tables",
        "-nostdlib", "-nostartfiles", "-nodefaultlibs",
        "-Wl,--strip-all", "-Wl,--as-needed", "-Wl,--hash-style=sysv", "-Wl,--build-id=none",
        "-Wl,-z,max-page-size=16384", "-Wl,-soname,libshadowhook_nothing.so",
    ]);
    cmd.arg("-o").arg(&nothing).arg(root.join("nothing/sh_nothing.c"));
    let status = cmd.status().expect("run C compiler for libshadowhook_nothing.so");
    assert!(status.success(), "building libshadowhook_nothing.so failed: {cmd:?}");
    // OUT_DIR = target/<triple>/<profile>/build/judgehook-<hash>/out
    let profile_dir: &Path = out.ancestors().nth(3).unwrap();
    fs::copy(&nothing, profile_dir.join("libshadowhook_nothing.so")).unwrap();
}

/// $NDK/toolchains/llvm/prebuilt/<host>/lib/clang/<ver>/lib/linux/<file>
fn find_ndk_builtins(file: &str) -> Option<PathBuf> {
    println!("cargo:rerun-if-env-changed=ANDROID_NDK_HOME");
    println!("cargo:rerun-if-env-changed=ANDROID_NDK_ROOT");
    let ndk = env::var_os("ANDROID_NDK_HOME").or_else(|| env::var_os("ANDROID_NDK_ROOT"))?;
    let prebuilt = PathBuf::from(ndk).join("toolchains/llvm/prebuilt");
    for host in fs::read_dir(prebuilt).ok()?.flatten() {
        let clang = host.path().join("lib/clang");
        for ver in fs::read_dir(clang).into_iter().flatten().flatten() {
            let dir = ver.path().join("lib/linux");
            if dir.join(file).is_file() {
                return Some(dir);
            }
        }
    }
    None
}
