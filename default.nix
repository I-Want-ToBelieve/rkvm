{
  lib,
  rustPlatform,
  fetchFromGitHub,
  pkg-config,
  libevdev,
  openssl,
  llvmPackages,
  linuxHeaders,
}:
rustPlatform.buildRustPackage rec {
  pname = "rkvm";
  version = "0.3.3";

  # src = fetchFromGitHub {
  #   owner = "htrefil";
  #   repo = "rkvm";
  #   rev = "master";
  #   hash = "";
  # };

  src = ./.;

  cargoLock = {
    lockFile = ./Cargo.lock;
    outputHashes = {
    };
  };

  nativeBuildInputs = [llvmPackages.clang pkg-config];

  buildInputs = [libevdev openssl linuxHeaders];

  LIBCLANG_PATH = lib.makeLibraryPath [llvmPackages.libclang];

  # The libevdev bindings preserve comments from libev, some of which
  # contain indentation which Cargo tries to interpret as doc tests.
  doCheck = false;

  meta = with lib; {
    description = "Virtual KVM switch for Linux machines";
    homepage = "https://github.com/htrefil/rkvm";
    license = licenses.mit;
    maintainers = with maintainers; [colemickens];
    platforms = platforms.linux;
  };
}
