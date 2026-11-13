{ pkgs, lib, config, inputs, ... }:

let
  nativeStdenv =
    if pkgs.stdenv.hostPlatform.isLinux
    then pkgs.stdenvAdapters.useMoldLinker pkgs.clangStdenv
    else pkgs.clangStdenv;
  replaceRustCompiler = oldCompiler: dependencies:
    map (dependency:
      if dependency.outPath == oldCompiler.outPath then nativeStdenv.cc else dependency
    ) dependencies;
  # 与宿主保持一致：只替换聚合包传播的编译器，保留预编译 Rust 组件及其运行库。
  rustToolchain = ((inputs.rust-overlay.lib.mkRustBin { } pkgs.buildPackages)
    .fromRustupToolchainFile config.languages.rust.toolchainFile).overrideAttrs (old: {
    depsHostHostPropagated = replaceRustCompiler
      pkgs.buildPackages.stdenv.cc old.depsHostHostPropagated;
    propagatedBuildInputs = replaceRustCompiler
      pkgs.pkgsTargetTarget.stdenv.cc old.propagatedBuildInputs;
  });
in
{
  stdenv = nativeStdenv;

  packages = with pkgs; [
    go-task
    python3
    git
    pkg-config
    cmake
    gnumake
    openssl
  ] ++ lib.optional pkgs.stdenv.hostPlatform.isLinux pkgs.mold;

  languages.c.enable = false;

  languages.rust = {
    enable = true;
    toolchainFile = ./rust-toolchain.toml;
    toolchainPackage = lib.mkForce rustToolchain;
    lsp.package = rustToolchain;
    clangLinker.enable = false;
  };

  env.LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
  env."CARGO_TARGET_${pkgs.stdenv.hostPlatform.rust.cargoEnvVarTarget}_LINKER" =
    "${nativeStdenv.cc}/bin/clang";
}
