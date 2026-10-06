"""Build Proton Bridge as a Go shared library for Spotty's own process.

The upstream source and runtime dependency archive are prepared and hash
verified by prepare_bundle.py. This script adapts only a private Cargo OUT_DIR
copy; it never edits the checked-in or cached upstream source.
"""

from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile


VERSION = "3.27.0"
MODULE = "github.com/ProtonMail/proton-bridge/v3"


def replace_once(path: Path, old: str, new: str) -> None:
    source = path.read_text()
    count = source.count(old)
    if count != 1:
        raise RuntimeError(f"Expected one patch target in {path}, found {count}")
    path.write_text(source.replace(old, new, 1))


def replace_function(path: Path, name: str, replacement: str) -> None:
    source = path.read_text()
    start = source.find(f"func (s *Service) {name}(")
    if start < 0:
        raise RuntimeError(f"Cannot find {name} in {path}")
    end = source.find("\nfunc ", start + 1)
    if end < 0:
        raise RuntimeError(f"Cannot find end of {name} in {path}")
    path.write_text(source[:start] + replacement.rstrip() + source[end:])


def replace_region(path: Path, start_text: str, end_text: str, replacement: str) -> None:
    source = path.read_text()
    start = source.find(start_text)
    end = source.find(end_text, start + len(start_text))
    if start < 0 or end < 0:
        raise RuntimeError(f"Cannot find patch region in {path}")
    end += len(end_text)
    path.write_text(source[:start] + replacement + source[end:])


def patch_bridge(source: Path) -> None:
    app = source / "internal/app/app.go"
    replace_once(
        app,
        'import (\n\t"fmt"',
        'import (\n\t"context"\n\t"fmt"',
    )
    replace_once(
        app,
        '"runtime"\n\t"time"',
        '"runtime"\n\t"sync"\n\t"time"',
    )
    replace_once(
        app,
        '\tquitCh := make(chan struct{})\n\n'
        '\t// On crash, quit the app.\n'
        '\tcrashHandler.AddRecoveryAction(func(any) error { close(quitCh); return nil })\n\n'
        '\treturn fn(crashHandler, quitCh)',
        'quitCh := make(chan struct{})\n'
        '\tvar quitOnce sync.Once\n'
        '\tquit := func() { quitOnce.Do(func() { close(quitCh) }) }\n'
        '\tgo func() {\n'
        '\t\tselect {\n'
        '\t\tcase <-c.Context.Done():\n'
        '\t\t\tquit()\n'
        '\t\tcase <-quitCh:\n'
        '\t\t}\n'
        '\t}()\n\n'
        '\t// On crash, stop this embedded Bridge instance.\n'
        '\tcrashHandler.AddRecoveryAction(func(any) error { quit(); return nil })\n\n'
        '\treturn fn(crashHandler, quitCh)',
    )
    replace_once(
        app,
        'return withCrashHandler(restarter, reporter, func(crashHandler *crash.Handler, quitCh <-chan struct{}) error {',
        'return withCrashHandler(c.Context, restarter, reporter, func(crashHandler *crash.Handler, quitCh <-chan struct{}) error {',
    )
    replace_once(
        app,
        'func withCrashHandler(restarter *restarter.Restarter, reporter *sentry.Reporter, fn func(*crash.Handler, <-chan struct{}) error) error {',
        'func withCrashHandler(ctx context.Context, restarter *restarter.Restarter, reporter *sentry.Reporter, fn func(*crash.Handler, <-chan struct{}) error) error {',
    )
    replace_once(app, '<-c.Context.Done()', '<-ctx.Done()')
    replace_once(
        app,
        '\tif err != nil {\n\t\tlogrus.Fatal(err)\n\t}\n\n\treturn err',
        '\treturn err',
    )
    replace_once(
        app,
        '\trestarter := restarter.New(exe)',
        '\t// Spotty owns this process and its updates; never relaunch the host.\n'
        '\trestarter := restarter.New("")',
    )
    replace_once(
        app,
        '\tcrashHandler.AddRecoveryAction(func(any) error { restarter.Set(true, true); return nil })',
        '\tcrashHandler.AddRecoveryAction(func(any) error { return nil })',
    )
    replace_once(
        app,
        '\t\t\t\t\t\tfeatureFlags := unleash.GetStartupFeatureFlagsAndStore(constants.APIHost, version, locations.ProvideUnleashStartupCachePath)\n\n'
        '\t\t\t\t\t\treturn withSingleInstance(settings, locations.GetLockFile(), version, func() error {',
        '\t\t\t\t\t\treturn withSingleInstance(settings, locations.GetLockFile(), version, func() error {\n'
        '\t\t\t\t\t\t\tnotifySpottyOwnerAcquired()\n'
            '\t\t\t\t\t\t\tfeatureFlags := unleash.GetStartupFeatureFlagsAndStore(constants.APIHost, version, locations.ProvideUnleashStartupCachePath)',
    )

    # Stop cancels the app context to unwind startup and event loops. Bridge
    # teardown still needs a live context so its servers and vault can flush.
    app_bridge = source / "internal/app/bridge.go"
    replace_once(
        app_bridge,
        'import (\n\t"fmt"',
        'import (\n\t"context"\n\t"fmt"',
    )
    replace_once(
        app_bridge,
        '\t"runtime"\n',
        '\t"runtime"\n\t"time"\n',
    )
    replace_once(
        app_bridge,
        '\tdefer bridge.Close(c.Context)',
        '\tdefer func() {\n'
        '\t\tshutdownCtx, cancel := context.WithTimeout(context.Background(), 20*time.Second)\n'
        '\t\tdefer cancel()\n'
        '\t\tbridge.Close(shutdownCtx)\n'
        '\t}()',
    )

    # Let Bridge persist its existing autostart preference using Spotty's
    # owner-private wrapper, while keeping the actual backend inside Spotty.
    # The wrapper starts Spotty's hidden daemon mode, never a Bridge executable.
    frontend = source / "internal/frontend/grpc/service_methods.go"
    replace_function(
        frontend,
        "Restart",
        'func (s *Service) Restart(context.Context, *emptypb.Empty) (*emptypb.Empty, error) {\n'
        '\treturn nil, status.Error(codes.Unavailable, "Bridge is managed by Spotty and cannot restart itself")\n'
        '}',
    )
    replace_once(
        frontend,
        '\t"github.com/ProtonMail/proton-bridge/v3/internal/events"\n',
        "",
    )
    replace_once(
        frontend,
        '\t"github.com/ProtonMail/proton-bridge/v3/internal/safe"\n',
        "",
    )
    replace_function(
        frontend,
        "CheckUpdate",
        'func (s *Service) CheckUpdate(context.Context, *emptypb.Empty) (*emptypb.Empty, error) {\n'
        '\treturn nil, status.Error(codes.Unavailable, "Bridge updates are managed by Spotty")\n'
        '}',
    )
    replace_function(
        frontend,
        "InstallUpdate",
        'func (s *Service) InstallUpdate(context.Context, *emptypb.Empty) (*emptypb.Empty, error) {\n'
        '\treturn nil, status.Error(codes.Unavailable, "Bridge updates are managed by Spotty")\n'
        '}',
    )
    replace_function(
        frontend,
        "SetIsAutomaticUpdateOn",
        'func (s *Service) SetIsAutomaticUpdateOn(context.Context, *wrapperspb.BoolValue) (*emptypb.Empty, error) {\n'
        '\treturn nil, status.Error(codes.Unavailable, "Bridge updates are managed by Spotty")\n'
        '}',
    )
    replace_once(
        frontend,
        'entry.Panic(request.Message)',
        'entry.Error(request.Message)',
    )
    replace_once(
        frontend,
        'entry.Fatal(request.Message)',
        'entry.Error(request.Message)',
    )

    # Disable both the startup check and later preference-triggered checks. A
    # no-op closure preserves the existing internal call sites safely.
    bridge = source / "internal/bridge/bridge.go"
    replace_region(
        bridge,
        "\t// Check for updates when triggered.",
        "\tdefer bridge.goUpdate()",
        "\t// Spotty owns Bridge updates; preserve call sites as an inert closure.\n"
        "\tbridge.goUpdate = func() {}\n\tdefer bridge.goUpdate()",
    )
    replace_once(
        bridge,
        '\t"github.com/ProtonMail/proton-bridge/v3/internal/constants"\n',
        "",
    )

    # The release source archive omits this generated package file. Recreate
    # it from the pinned go.mod inputs, matching upstream utils/credits.sh
    # without its mktemp writes outside Cargo's OUT_DIR.
    go_mod = source / "go.mod"
    modules: set[str] = set()
    for line in go_mod.read_text().splitlines():
        if not line.startswith("\t"):
            continue
        stripped = line.strip()
        fields = stripped.split()
        if not fields:
            continue
        if "=>" in fields:
            module = fields[fields.index("=>") + 1]
        else:
            module = fields[0]
        if "protontech" in module or "github.com/therecipe/qt/" in module:
            continue
        if "/" in module:
            modules.add(module)
    credits = ";".join([*sorted(modules), "Qt 6.8.2 by Qt group"])
    credits_path = source / "internal/bridge/credits.go"
    license_header = (source / "utils/license_header.txt").read_text()
    credits_path.write_text(
        license_header
        + "// Code generated by Spotty's pinned-source build adapter. DO NOT EDIT.\n\n"
        + 'package bridge\n\nconst Credits = "'
        + credits.replace('\\', '\\\\').replace('"', '\\"')
        + '"\n'
    )

    replace_once(
        bridge,
        '\t"github.com/ProtonMail/proton-bridge/v3/internal/updater"\n',
        "",
    )


def extract_runtime_libraries(archive: Path, destination: Path) -> None:
    destination.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive, "r:gz") as outer:
        for member in outer.getmembers():
            if not member.name.startswith("runtime-libs/"):
                continue
            relative = Path(member.name).name
            if not relative or relative in {".", ".."}:
                continue
            target = destination / relative
            if member.issym():
                link = Path(member.linkname)
                if link.is_absolute() or ".." in link.parts:
                    raise RuntimeError(f"Unsafe runtime library symlink: {member.name}")
                if target.exists() or target.is_symlink():
                    target.unlink()
                target.symlink_to(link.name)
                continue
            if not member.isfile():
                continue
            stream = outer.extractfile(member)
            if stream is None:
                raise RuntimeError(f"Cannot read runtime dependency {member.name}")
            target.write_bytes(stream.read())
    required = ("libfido2.so.1", "libcbor.so.0.10")
    missing = [name for name in required if not (destination / name).exists()]
    if missing:
        raise RuntimeError(f"Pinned runtime dependency archive is missing: {', '.join(missing)}")
    for linker_name, soname in (("libfido2.so", "libfido2.so.1"), ("libcbor.so", "libcbor.so.0.10")):
        alias = destination / linker_name
        if alias.exists() or alias.is_symlink():
            alias.unlink()
        alias.symlink_to(soname)


def main() -> None:
    out = Path(sys.argv[1]).resolve()
    crate = Path(__file__).resolve().parent
    cache_override = os.environ.get("SPOTTY_PROTON_GO_CACHE")
    if cache_override:
        cache = Path(cache_override).resolve()
    else:
        target_root = next(
            (directory for directory in out.parents
             if directory.name == "target" or directory.name.endswith("-target")),
            None,
        )
        if target_root is None or target_root.parent is None:
            raise RuntimeError("Cannot locate Cargo target directory for Go cache")
        cache_parent = target_root.parent / "build" if target_root.name == "target" else target_root.parent
        cache = cache_parent / "spotty-go-cache"
    temporary = cache / "tmp"
    temporary.mkdir(parents=True, exist_ok=True)
    source_root = out / "proton-bridge-source"
    if source_root.exists():
        shutil.rmtree(source_root)
    source_root.mkdir(parents=True)
    with tarfile.open(out / "bridge-source.tar.gz", "r:gz") as source_archive:
        for member in source_archive.getmembers():
            member_path = Path(member.name)
            if member_path.is_absolute() or ".." in member_path.parts:
                raise RuntimeError("Unsafe path in pinned Bridge source archive")
            source_archive.extract(member, source_root, filter="data")
    roots = list(source_root.glob("proton-bridge-*"))
    if len(roots) != 1:
        raise RuntimeError("Pinned Bridge source archive has an unexpected layout")
    source = roots[0]
    patch_bridge(source)
    wrapper_dir = source / "cmd/spotty-inprocess"
    wrapper_dir.mkdir(parents=True)
    shutil.copy2(crate / "inprocess/main.go", wrapper_dir / "main.go")
    shutil.copy2(crate / "inprocess/embedded_app.go", source / "internal/app/spotty_embedded.go")
    test_source = crate / "inprocess/main_test.go"
    if test_source.is_file():
        shutil.copy2(test_source, wrapper_dir / "main_test.go")

    runtime_libs = out / "runtime-libs"
    extract_runtime_libraries(out / "bridge-dependencies.tar.gz", runtime_libs)
    # CGO links the host development ABI; old-dtags RPATH makes the private,
    # hash-pinned runtime FIDO/CBOR libraries win when the host loads the .so.
    env = os.environ.copy()
    for directory in (
        cache / "gocache",
        cache / "gomodcache",
        cache / "gopath",
        cache / "config",
        cache / "xdg-cache",
        cache / "ccache",
    ):
        directory.mkdir(parents=True, exist_ok=True)
    env["GOCACHE"] = str(cache / "gocache")
    env["GOMODCACHE"] = str(cache / "gomodcache")
    env["GOPATH"] = str(cache / "gopath")
    env["XDG_CONFIG_HOME"] = str(cache / "config")
    env["XDG_CACHE_HOME"] = str(cache / "xdg-cache")
    env["CCACHE_DIR"] = str(cache / "ccache")
    env["TMPDIR"] = str(temporary)
    env["GOTMPDIR"] = str(temporary)
    env["GOTOOLCHAIN"] = "local"
    env["CGO_ENABLED"] = "1"
    env["GOMAXPROCS"] = "2"
    go = shutil.which("go")
    host_prefix: list[str] = []
    if go is None and shutil.which("flatpak-spawn"):
        host_prefix = ["flatpak-spawn", "--host"]
        go = "go"
    if go is None:
        raise RuntimeError("Go 1.26 with CGO is required to build the in-process Bridge library")
    pkg_cflags = subprocess.run(
        [*host_prefix, "pkg-config", "--cflags", "libfido2"],
        text=True,
        capture_output=True,
        check=True,
    ).stdout.strip()
    pkg_libs = subprocess.run(
        [*host_prefix, "pkg-config", "--libs", "libfido2"],
        text=True,
        capture_output=True,
        check=True,
    ).stdout.strip()
    env["CGO_CFLAGS"] = " ".join(filter(None, [env.get("CGO_CFLAGS", ""), pkg_cflags]))
    env["CGO_LDFLAGS"] = " ".join(filter(None, [
        env.get("CGO_LDFLAGS", ""),
        f"-L{runtime_libs}",
        pkg_libs,
        "-lcbor -lssl -lcrypto",
        f"-Wl,--disable-new-dtags -Wl,-rpath,$ORIGIN/runtime-libs",
    ]))
    env["GOFLAGS"] = "-p=2 -mod=readonly -trimpath"
    host_go_prefix = [*host_prefix]
    if host_prefix:
        for key in (
            "GOCACHE",
            "GOMODCACHE",
            "GOPATH",
            "XDG_CONFIG_HOME",
            "XDG_CACHE_HOME",
            "CCACHE_DIR",
            "TMPDIR",
            "GOTMPDIR",
            "GOTOOLCHAIN",
            "CGO_ENABLED",
            "GOMAXPROCS",
            "CGO_CFLAGS",
            "CGO_LDFLAGS",
            "GOFLAGS",
        ):
            host_go_prefix.append(f"--env={key}={env[key]}")
    output = out / "libspotty_proton_bridge.so"
    ldflags = (
        f"-X {MODULE}/internal/constants.Version={VERSION}+spotty "
        f"-X {MODULE}/internal/constants.Revision=spotty "
        f"-X {MODULE}/internal/constants.Tag=spotty "
        f"-X {MODULE}/internal/constants.BuildTime=cargo "
        f'-X "{MODULE}/internal/constants.FullAppName=Spotty Proton Mail Bridge"'
    )
    subprocess.run(
        [*host_go_prefix, go, "build", "-p=2", "-mod=readonly", "-trimpath", "-buildmode=c-shared", "-ldflags", ldflags, "-o", str(output),
         f"{MODULE}/cmd/spotty-inprocess"],
        cwd=source,
        env=env,
        check=True,
    )
    print(f"Built in-process Proton Bridge library: {output}")


if __name__ == "__main__":
    main()
