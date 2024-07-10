# It would be cool to produce OCI images instead of docker images to
# avoid dependency on docker tool chain. Though the maturity of OCI
# builder in nixpkgs is questionable which is why we postpone this step.
#
# We limit max number of image layers to 42 because there is a bug in
# containerd triggered when there are too many layers:
# https://github.com/containerd/containerd/issues/4684

{ busybox
, dockerTools
, e2fsprogs
, git
, lib
, io-engine
, io-engine-dev
, pkgs
, libspdk-fio
, stdenv
, utillinux
, writeScriptBin
, xfsprogs
, runCommand
, tini
, sourcer
, img_tag ? ""
, img_org ? ""
, img_prefix
}:
let
  version = if img_tag != "" then img_tag else io-engine.version;
  path = lib.makeBinPath [ "/" busybox utillinux ];
  repo-org = if img_org != "" then img_org else "${builtins.readFile (runCommand "repo_org" {
    buildInputs = [ git ];
   } ''
    export GIT_DIR="${sourcer.git-src}/.git"
    cp ${sourcer.repo-org}/git-org-name.sh .
    patchShebangs ./git-org-name.sh
    ./git-org-name.sh ${sourcer.git-src} --case lower --remote origin > $out
  '')}";

  # common props for all io-engine images
  ioEngineImageProps = {
    tag = version;
    created = "now";
    config = {
      Env = [
        "PATH=${path}"
        "RUST_BACKTRACE=1"
      ];
      ExposedPorts = { "10124/tcp" = { }; };
      Entrypoint = [ "/bin/io-engine" ];
    };
    extraCommands = ''
      mkdir tmp
      mkdir -p var/tmp
    '';
  };
  clientImageProps = {
    tag = version;
    created = "now";
    config = { };
    copyToRoot = [ busybox ];
    extraCommands = ''
      mkdir tmp
      mkdir -p var/tmp
    '';
  };
  spdk_fio_engine = runCommand "spdk_fio_engine" { } ''
    mkdir -p $out/lib
    cp ${pkgs.libspdk-fio.fio}/spdk_nvme $out/lib
    except=$(${pkgs.glibc.bin}/bin/ldd $out/lib/spdk_nvme | awk '{ print $3 }' | grep .so | xargs -I% echo "-e %" | tr '\n' ' ')
    # remove all references except the ones in spdk_nvme
    ${pkgs.nukeReferences}/bin/nuke-refs $except $out/lib/spdk_nvme
  '';

  fio = runCommand "fio_bin_only" { } ''
    mkdir -p $out/bin
    cp ${pkgs.fio}/bin/fio $out/bin/fio
  '';
  fio_wrapper = pkgs.writeShellScriptBin "fio" ''
    LD_PRELOAD=${spdk_fio_engine}/lib/spdk_nvme ${fio}/bin/fio "$@"
  '';
  casperf = runCommand "casperf" { } ''
    mkdir -p $out/bin
    cp ${io-engine.out}/bin/casperf $out/bin/casperf
  '';
  io-engine-bins = runCommand "io-engine" { } ''
    mkdir -p $out/bin
    cp ${io-engine.out}/bin/io-engine $out/bin/io-engine
    cp ${io-engine.out}/bin/io-engine-client $out/bin/io-engine-client
  '';
  spdk-bins = runCommand "spdk-bins" { } ''
      mkdir -p $out/bin
      cp ${pkgs.libspdk-fio.out}/bin/blobcli $out/bin
      cat <<EOF >$out/bin/blobcli.json
      {
        "subsystems": [
          {
            "subsystem": "bdev",
            "config": [
              {
                "params": {
                  "name": "nvme",
                  "filename": "/dev/nvme0n1"
                },
                "method": "bdev_aio_create"
              },
              {
                "params": {
                  "name": "nvme0n1",
                  "filename": "/dev/nvme0n1"
                },
                "method": "bdev_aio_create"
              }
            ]
          }
        ]
      }
    EOF
  '';

  mctl = writeScriptBin "mctl" ''
    /bin/io-engine-client "$@"
  '';
in
{
  mayastor-io-engine = dockerTools.buildImage (ioEngineImageProps // {
    name = "${repo-org}/${img_prefix}-io-engine";
    copyToRoot = [ busybox io-engine-bins mctl ];
  });

  mayastor-io-engine-dev = dockerTools.buildImage (ioEngineImageProps // {
    name = "${repo-org}/${img_prefix}-io-engine-dev";
    copyToRoot = [ busybox io-engine-dev ];
  });

  mayastor-io-engine-client = dockerTools.buildImage (ioEngineImageProps // {
    name = "${repo-org}/${img_prefix}-io-engine-client";
    copyToRoot = [ busybox io-engine ];
    config = { Entrypoint = [ "/bin/io-engine-client" ]; };
  });

  mayastor-fio-spdk = dockerTools.buildImage (clientImageProps // {
    name = "${repo-org}/${img_prefix}-fio-spdk";
    copyToRoot = clientImageProps.copyToRoot ++ [ tini fio_wrapper ];
  });

  mayastor-spdk-blobcli = dockerTools.buildImage (clientImageProps // {
    name = "${repo-org}/${img_prefix}-spdk-blobcli";
    copyToRoot = clientImageProps.copyToRoot ++ [ spdk-bins ];
    config = { Entrypoint = [ "/bin/blobcli" ]; };
    extraCommands = ''
      cp ${spdk-bins}/bin/blobcli.json .
      mkdir tmp
      mkdir -p var/tmp
    '';
  });

  mayastor-casperf = dockerTools.buildImage (clientImageProps // {
    name = "${repo-org}/${img_prefix}-casperf";
    copyToRoot = clientImageProps.copyToRoot ++ [ tini casperf ];
  });
}
