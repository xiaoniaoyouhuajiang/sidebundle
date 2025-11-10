{
  description = "Extract indexers from Docker images → AppImage / rootfs(+proot)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.05"; # 或不变：nixos-unstable

  outputs = { self, nixpkgs }:
  let
    systems = [ "x86_64-linux" "aarch64-linux" ];
    forAll = f: nixpkgs.lib.genAttrs systems (system: f (import nixpkgs { inherit system; }));
  in
  {
    packages = forAll (pkgs:
    let
      appImageTool = pkgs.appimageTools.extract {
        pname = "appimagetool";
        version = "12";
        src = pkgs.fetchurl {
          url = "https://github.com/AppImage/AppImageKit/releases/download/12/appimagetool-x86_64.AppImage";
          sha256 = "04ws94q71bwskmhizhwmaf41ma4wabvfgjgkagr8wf3vakgv866r";
        };
      };

      # 通用：从官方镜像抽取二进制；若可，则 autopatchelf 成“脱容器”可执行
      mkFromDocker = { name, imageName, imageDigest, imageSha256, binPath }:
      let
        img = pkgs.dockerTools.pullImage {
          inherit imageName imageDigest;
          sha256 = imageSha256;
          os   = "linux";
          arch = if pkgs.stdenv.isAarch64 then "aarch64" else "amd64";
          finalImageTag = "release";
        };

        # 1) 解包成 rootfs（不需要 docker 守护进程）
        rootfsDir = pkgs.stdenv.mkDerivation {
          pname = "${name}-rootfs-dir";
          version = "1";
          nativeBuildInputs = [
            pkgs.skopeo
            pkgs.jq
            pkgs.gnutar
            pkgs.findutils
            pkgs.coreutils
          ];
          unpackPhase = "true";
          installPhase = ''
            mkdir -p $out/oci $out/rootfs
            # docker-archive -> OCI layout
            skopeo --insecure-policy copy \
              docker-archive:${img} \
              oci:$out/oci:bundle
            # OCI -> rootfs (manual layer apply to avoid chown requirements)
            manifestDigest=$(jq -r '.manifests[0].digest' $out/oci/index.json)
            manifestBlob=$out/oci/blobs/sha256/''${manifestDigest#sha256:}
            mapfile -t layers < <(jq -r '.layers[].digest' "$manifestBlob")
            for digest in "''${layers[@]}"; do
              layer=$out/oci/blobs/sha256/''${digest#sha256:}
              tmpLayer=$(mktemp -d)
              tar -C "$tmpLayer" --warning=no-unknown-keyword --delay-directory-restore -xf "$layer"
              # process whiteouts
              find "$tmpLayer" -name ".wh.*" | while IFS= read -r wh; do
                rel=$(realpath --relative-to="$tmpLayer" "$wh")
                dir=$(dirname "$rel")
                [ "$dir" = "." ] && dir=""
                base=$(basename "$rel")
                target=$(printf '%s' "$base" | sed "s/^\.wh\.//")
                if [ "$base" = ".wh..wh..opq" ]; then
                  targetDir="$out/rootfs/$dir"
                  if [ -d "$targetDir" ]; then
                    find "$targetDir" -mindepth 1 -maxdepth 1 -exec rm -rf {} +
                  fi
                else
                  rm -rf "$out/rootfs/$dir/$target"
                fi
                rm -f "$wh"
              done
              tar -C "$tmpLayer" --no-same-owner --no-same-permissions \
                --warning=no-unknown-keyword --delay-directory-restore -cf - . | \
                tar -C $out/rootfs --no-same-owner --no-overwrite-dir \
                  --warning=no-unknown-keyword --delay-directory-restore -xf -
              rm -rf "$tmpLayer"
            done
          '';
        };

        # 2) 从 rootfs 拷出可执行并 autopatchelf（尽量让它脱离容器独立运行）
        execPkg = pkgs.stdenv.mkDerivation {
          pname = name;
          version = "from-image";
          nativeBuildInputs = [ pkgs.autoPatchelfHook pkgs.makeWrapper ];
          buildInputs = [ pkgs.stdenv.cc.cc pkgs.glibc pkgs.zlib pkgs.openssl ];
          dontUnpack = true;
          installPhase = ''
            mkdir -p $out/bin
            cp -f ${rootfsDir}/rootfs${binPath} $out/bin/${name}-real
            chmod +x $out/bin/${name}-real
          '';
          # autopatchelf via hook；再包一层 wrapper 注入证书等
          postFixup = ''
            wrapProgram $out/bin/${name}-real \
              --set SSL_CERT_FILE ${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt
            ln -s ${name}-real $out/bin/${name}
          '';
        };

        # 3) 打成 rootfs 压缩包（便于分发，跑 proot 即可）
        rootfsTar = pkgs.runCommand "${name}-rootfs.tar.zst"
          { nativeBuildInputs = [ pkgs.zstd pkgs.gnutar ]; } ''
          mkdir work && cp -a ${rootfsDir}/rootfs work/rootfs
          tar -C work/rootfs --numeric-owner --owner=0 --group=0 \
              -I "zstd -19" -cf $out .
        '';

        appDir = pkgs.runCommand "${name}-AppDir" {} ''
          mkdir -p $out
          ln -s ${execPkg}/bin/${name} $out/AppRun
          cat > $out/${name}.desktop <<EOF
          [Desktop Entry]
          Name=${name}
          Exec=AppRun
          Icon=${name}
          Type=Application
          Categories=Development;
          EOF
        '';

        # 4) 生成 AppImage（单文件；如果 autopatchelf 成功，这个可在宿主直接运行）
        appimage = pkgs.runCommand "${name}.AppImage" {} ''
          cp -r ${appImageTool} tool
          chmod -R +w tool
          ${pkgs.patchelf}/bin/patchelf \
            --set-interpreter ${pkgs.stdenv.cc.bintools.dynamicLinker} \
            tool/usr/bin/appimagetool

          cp -r ${appDir} AppDir
          chmod -R +w AppDir

          export LD_LIBRARY_PATH=tool/usr/lib:tool/lib:${pkgs.zlib}/lib:${pkgs.glibc}/lib
          export PATH=tool/usr/bin:$PATH
          tool/usr/bin/appimagetool AppDir $out
          chmod +x $out
        '';
      in {
        inherit rootfsDir rootfsTar execPkg appimage;
      };

      # === 在此列出你的 indexer 镜像 ===
      indexers = [
        # 例：rust-analyzer（示意，自己换成真实镜像信息）
        {
          name = "scip-rust";
          imageName = "sourcegraph/scip-rust";
          imageDigest = "sha256:75ec8d7c7f8dc295754cf679b39683250797f951cc9686933995b285bc79e4f1";
          imageSha256 = "02ak5ygk76yngwl789xnn3y6waqr8279yicvfg9lw3d40szyy772";
          binPath = "/usr/local/cargo/bin/rust-analyzer";
        }
        # 再加更多…
      ];

      # 批量导出不同类型产物
      asAttrs = pkgs.lib.listToAttrs (map (i: { name = i.name; value = mkFromDocker i; }) indexers);
      take = f: pkgs.lib.mapAttrs (_: v: f v) asAttrs;
    in
    {
      # AppImage 单文件输出
      appimage = take (v: v.appimage);
      # rootfs 压缩包输出（zst）
      rootfs   = take (v: v.rootfsTar);
      # 脱容器 ELF 包（可用于进一步自定义 bundle）
      bins     = take (v: v.execPkg);

      # 顺带给你打包一个 proot + 启动脚本，便于离线跑 rootfs
      prootRunner = pkgs.stdenv.mkDerivation {
        pname = "proot-runner";
        version = "1";
        nativeBuildInputs = [ pkgs.makeWrapper ];
        dontUnpack = true;
        installPhase = ''
          mkdir -p $out/bin
          cat > $out/bin/run-rootfs <<'SH'
          #!/usr/bin/env bash
          # 用法：run-rootfs <rootfs_dir> <cmd_inside_rootfs> [args...]
          set -euo pipefail
          ROOTFS="$1"; shift
          SELF="$(cd "$(dirname "$0")" && pwd)"
          PROOT_BIN="''${PROOT_BIN:-$SELF/proot}"
          # 常用绑定，按需添加 -b host:guest
          exec "$PROOT_BIN" -R "$ROOTFS" \
               -b /proc -b /dev -b /sys \
               /bin/sh -lc "$*"
          SH
          chmod +x $out/bin/run-rootfs
          ln -s ${pkgs.proot}/bin/proot $out/bin/proot
        '';
      };
    });
  };
}
