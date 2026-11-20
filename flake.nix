{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      rust-overlay,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        inherit (pkgs) lib;

        # One toolchain for the dev shell and the package.
        rust = pkgs.rust-bin.stable.latest.default;
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rust;
          rustc = rust;
        };

        # The dependencies, listed once and shared by the dev shell, the
        # package and the image.
        # Tools needed to build: pkg-config finds AWS-LC and FFmpeg, bindgen
        # (with libclang) reads FFmpeg's headers.
        buildTools = [
          pkgs.pkg-config
          pkgs.rustPlatform.bindgenHook
        ];
        # FFmpeg, reduced to what the video needs: libavcodec (its own JPEG
        # decoder, the Vulkan and VA-API encoders, and OpenH264 for the CPU),
        # libswscale and libavutil. Its own tests assume a full build, so
        # they are skipped.
        ffmpeg =
          (pkgs.ffmpeg-headless.override {
            withHeadlessDeps = false;
            buildAvcodec = true;
            buildAvutil = true;
            buildSwscale = true;
            withOpenh264 = true;
            withVulkan = true;
            withVaapi = true;
            withDrm = true;
          }).overrideAttrs
            { doCheck = false; };
        libraries = [ ffmpeg ];

        cargoToml = lib.importTOML ./Cargo.toml;

        ilink-web = rustPlatform.buildRustPackage {
          pname = cargoToml.package.name;
          inherit (cargoToml.package) version;
          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./src
            ];
          };
          cargoLock = {
            lockFile = ./Cargo.lock;
            outputHashes = {
              "insta360-link-0.1.0" = "sha256-3LE2nPtizF3FzOivH+IjIst9h+BgVbt02XNUlsIFLPY=";
            };
          };
          nativeBuildInputs = buildTools;
          # nixpkgs links AWS-LC (for WebRTC encryption) from its own package
          # rather than building the vendored copy.
          buildInputs = libraries ++ [ pkgs.aws-lc ];
          meta = {
            inherit (cargoToml.package) description;
            license = lib.licenses.mit;
            mainProgram = "ilink-web";
          };
        };
      in
      {
        packages = {
          default = ilink-web;

          # Built from nothing: the image holds ilink-web and the Nix store
          # paths it needs, plus sh, curl and jq for --ice-servers-command,
          # with no distribution or package manager.
          # It carries no GPU drivers but uses the host's when they are passed
          # in (NVIDIA's by the NVIDIA Container Toolkit), and OpenH264
          # otherwise.
          #   nix build .#image && docker load < result
          image = pkgs.dockerTools.buildLayeredImage {
            name = "ilink-web";
            tag = "latest";
            contents = with pkgs; [
              # Libraries NVIDIA's Vulkan driver needs but the toolkit does
              # not bring, in /lib where the driver looks for them.
              libglvnd
              libx11
              libxext
              # /bin/sh with curl and jq, for --ice-servers-command, and the
              # CA certificates curl checks servers against.
              dockerTools.binSh
              curl
              jq
              dockerTools.caCertificates
            ];
            config = {
              # tini runs as PID 1 and passes on SIGTERM, which ilink-web would
              # ignore in that place, so that `docker stop` ends it at once.
              Entrypoint = [
                (lib.getExe pkgs.tini)
                "--"
                (lib.getExe ilink-web)
              ];
              Env = [
                # Ask the NVIDIA Container Toolkit for the Vulkan driver and
                # the video engines, not only CUDA.
                "NVIDIA_DRIVER_CAPABILITIES=all"
                # Where the toolkit puts NVIDIA's libraries, which its Vulkan
                # driver entry names without a path; Nix's loader does not
                # read /etc/ld.so.cache, so they have to be listed.
                "LD_LIBRARY_PATH=/usr/lib/x86_64-linux-gnu:/usr/lib64:/lib"
                # Where the toolkit puts the driver entry itself.
                "XDG_DATA_DIRS=/usr/local/share:/usr/share"
                # There is no home directory for Mesa's shader cache to go in.
                "MESA_SHADER_CACHE_DISABLE=true"
              ];
              ExposedPorts = {
                "8081/tcp" = { };
                "8081/udp" = { };
              };
              # Not root. Camera and GPU access come from the device nodes'
              # groups, given with --group-add when starting the container.
              User = "65534:65534";
            };
          };
        };

        devShells.default = pkgs.mkShell {
          packages = [ rust ] ++ buildTools;
          buildInputs = libraries;
        };
      }
    );
}
