{
  description = "rkvm devel";

  inputs = {nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";};

  outputs = {
    self,
    nixpkgs,
    ...
  }: let
    pkgsFor = system:
      import nixpkgs {
        inherit system;
        overlays = [];
      };

    targetSystems = ["aarch64-linux" "x86_64-linux"];
  in {
    devShells = nixpkgs.lib.genAttrs targetSystems (system: let
      pkgs = pkgsFor system;
    in {
      default = pkgs.mkShell {
        name = "rkvm-devel";
        LIBCLANG_PATH = pkgs.lib.makeLibraryPath [ pkgs.llvmPackages.libclang ];
        nativeBuildInputs = with pkgs; [
          # Compilers
          cargo
          rustc
          scdoc

          # libs
          libevdev
          openssl
          linuxHeaders
          llvmPackages.clang

          # Tools
         pkg-config
          clippy
          gdb
          gnumake
          rust-analyzer
          rustfmt
          strace
          valgrind
          zip
        ];
      };
    });
  };
}
