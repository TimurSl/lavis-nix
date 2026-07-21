{
  description = "Lavis Telegram userbot foundation";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
      package = pkgs.rustPlatform.buildRustPackage {
        pname = "lavis";
        version = "0.1.0";
        src = pkgs.lib.cleanSourceWith {
          src = self;
          filter = path: type:
            pkgs.lib.cleanSourceFilter path type
            && builtins.baseNameOf path != "result"
            && builtins.baseNameOf path != "target";
        };
        cargoLock.lockFile = ./Cargo.lock;
      };
    in
    {
      packages.${system}.default = package;
      apps.${system}.default = {
        type = "app";
        program = "${package}/bin/lavis";
      };
      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [ cargo clippy rustc rustfmt ];
      };
      checks.${system}.default = package;
    };
}
