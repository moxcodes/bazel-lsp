{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };
  outputs = {self, nixpkgs, flake-utils}:
    flake-utils.lib.eachDefaultSystem
      (system:
        let
          pkgs = import nixpkgs { inherit system; config.allowUnfree = true; };
        in
        with pkgs;
        {
          devShells.default = mkShell {
            packages = [
              cargo
              cargo-edit
              openssl
              pkg-config
            ];
            # The DEVSHELL is for making known what we're doing
            # the DEVSHELL_ICON is for succinct displays in terminal lines etc
            shellHook = ''
              export DEVSHELL="conference_tui_rust"
              export DEVSHELL_ICON="  "
              $DEVSHELL_SHELL
            '';
          };
        }
      );
}
