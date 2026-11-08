// Compile And64InlineHook (C++, MIT) into the cdylib.
//
// Upstream hard-codes a 4 KiB page size, which makes mprotect() fail with
// EINVAL on 16 KiB-page devices. We patch a copy in OUT_DIR at build time
// instead of editing the vendored source.
use std::{env, fs, path::PathBuf};

fn main() {
    // host builds (cargo test on the dev machine) only exercise the pure-Rust parts
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("third_party/And64InlineHook/And64InlineHook.cpp");
    println!("cargo:rerun-if-changed={}", src.display());

    let mut code = fs::read_to_string(&src).unwrap_or_else(|e| {
        panic!("{}: {e} (run: git submodule update --init)", src.display())
    });

    let patches: &[(&str, &str)] = &[
        // runtime page size for every mprotect() computation
        (
            "#define __page_size                4096",
            "#include <unistd.h>\n#define __page_size                ((uintptr_t)sysconf(_SC_PAGESIZE))",
        ),
        // __aligned__ needs a constant: use the largest Android page size
        (
            "static __attribute__((__aligned__(__page_size)))",
            "static __attribute__((__aligned__(16384)))",
        ),
    ];
    for (from, to) in patches {
        assert!(code.contains(from), "And64InlineHook patch anchor not found: {from}");
        code = code.replacen(from, to, 1);
    }

    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("And64InlineHook.cpp");
    fs::write(&out, code).unwrap();

    cc::Build::new()
        .cpp(true)
        .file(&out)
        .include(src.parent().unwrap())
        .flag_if_supported("-std=c++17")
        .flag_if_supported("-fno-exceptions")
        .flag_if_supported("-fno-rtti")
        .warnings(false)
        .compile("and64inlinehook");

    // __builtin___clear_cache in And64InlineHook lowers to a call to __clear_cache,
    // which lives in compiler-rt's static builtins. Bionic does not export it and
    // rustc does not link the builtins archive, so without this the .so fails to
    // dlopen with: cannot locate symbol "__clear_cache".
    // Must come after and64inlinehook on the link line (static archive order).
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let lib = format!("clang_rt.builtins-{arch}-android");
    let dir = find_ndk_builtins(&format!("lib{lib}.a"))
        .unwrap_or_else(|| panic!("lib{lib}.a not found under $ANDROID_NDK_HOME (run inside: nix develop .#android)"));
    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=static={lib}");

    // A64_LOGI/LOGE use __android_log_print
    println!("cargo:rustc-link-lib=log");
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
