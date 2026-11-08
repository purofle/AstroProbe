{
  description = "AstroDX IL2CPP reverse-engineering + modding toolbox";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      systems = [ "aarch64-darwin" "x86_64-darwin" "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f (import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
        config = { allowUnfree = true; android_sdk.accept_license = true; };
      }));
    in {
      devShells = forAll (pkgs:
        let
          # Cpp2IL / Il2CppInspectorRedux need net10, c01ns/Il2CppDumper needs net8.
          # Use the -bin (Microsoft prebuilt) SDKs: the source-built ones may miss
          # the binary cache and trigger an hours-long VMR build.
          dotnet = with pkgs.dotnetCorePackages; combinePackages [ sdk_10_0-bin sdk_8_0-bin ];

          android = pkgs.androidenv.composeAndroidPackages {
            includeNDK = true;
            buildToolsVersions = [ "35.0.0" ];   # zipalign + apksigner
            platformVersions = [ "35" ];
          };
          sdk = "${android.androidsdk}/libexec/android-sdk";
          ndk = "${sdk}/ndk-bundle";

          rust = pkgs.rust-bin.stable.latest.default.override {
            targets = [ "aarch64-linux-android" ];
          };

          reTools = with pkgs; [
            dotnet
            # ilspycmd: install via `dotnet tool` — the nixpkgs package depends on
            # the source-built SDK (no aarch64-darwin cache).
            # native method bodies: use the host IDA Pro 9.3 (not managed by nix)
            radare2
            jadx apktool
            python3 git unzip zip file binutils
          ];
          reEnv = {
            DOTNET_ROOT = "${dotnet}/share/dotnet";
            DOTNET_CLI_TELEMETRY_OPTOUT = "1";
            DOTNET_NOLOGO = "1";
          };
        in {
          # reverse engineering only (light: no NDK download)
          default = pkgs.mkShell ({ packages = reTools; } // reEnv);

          # building the Rust hook .so and repacking/signing the APK
          android = pkgs.mkShell ({
            packages = reTools ++ (with pkgs; [
              rust cargo-ndk
              android.androidsdk
              patchelf jdk17_headless
              websocat      # ws client for testing: websocat ws://PHONE_IP:8765
            ]);
            ANDROID_HOME = sdk;
            ANDROID_NDK_HOME = ndk;
            ANDROID_NDK_ROOT = ndk;
            BUILD_TOOLS = "${sdk}/build-tools/35.0.0";
          } // reEnv);
        });
    };
}
