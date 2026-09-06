#!/usr/bin/env python3
"""Embed the Mach-O dependency closure; keep this file mirrored in server/scripts."""

import argparse
from collections import deque
from pathlib import Path
import shutil
import subprocess

LOAD_COMMANDS = {
    "LC_LOAD_DYLIB", "LC_LOAD_WEAK_DYLIB", "LC_REEXPORT_DYLIB",
    "LC_LOAD_UPWARD_DYLIB", "LC_LAZY_LOAD_DYLIB",
}


def system_library(path):
    return str(path).startswith(("/usr/lib/", "/System/Library/"))


def parse_load_commands(output):
    dependencies, rpaths = [], []
    command = None
    for line in output.splitlines():
        line = line.strip()
        if line.startswith("cmd "):
            command = line[4:]
        elif command in LOAD_COMMANDS and line.startswith("name "):
            dependencies.append(line[5:].split(" (offset ", 1)[0])
        elif command == "LC_RPATH" and line.startswith("path "):
            rpaths.append(line[5:].split(" (offset ", 1)[0])
    return list(dict.fromkeys(dependencies)), list(dict.fromkeys(rpaths))


def inspect(path):
    return parse_load_commands(subprocess.check_output(["otool", "-l", str(path)], text=True))


def expand_path(name, loader, executable):
    for prefix, directory in (("@loader_path", loader.parent), ("@executable_path", executable.parent)):
        if name == prefix or name.startswith(prefix + "/"):
            return (directory / name[len(prefix):].lstrip("/")).resolve()
    if name.startswith("/"):
        return Path(name).resolve()
    raise ValueError(f"Unsupported Mach-O path {name!r} in {loader}")


def resolve_dependency(name, loader, executable, rpaths):
    if name.startswith("@rpath/"):
        candidates = [root / name[len("@rpath/"):] for root in rpaths]
    else:
        candidates = [expand_path(name, loader, executable)]
    for candidate in candidates:
        # Absolute OS install names and Swift runtime libraries can live only
        # in dyld's cache. A generic @rpath dependency must not be mistaken for
        # an OS library just because /usr/lib/swift appears in the search path.
        cached_system = system_library(candidate) and (
            not name.startswith("@rpath/")
            or (candidate.parent == Path("/usr/lib/swift") and candidate.name.startswith("libswift"))
        )
        if candidate.is_file() or cached_system:
            return candidate.resolve()
    raise ValueError(f"Unresolved dependency {name!r} in {loader}")


def bundle_libraries(source_executable, app_executable):
    source_executable = source_executable.resolve()
    app_executable = app_executable.resolve()
    frameworks = app_executable.parent.parent / "Frameworks"
    frameworks.mkdir(parents=True, exist_ok=True)
    copied, names = {}, {}
    queue = deque([(source_executable, app_executable, [])])
    while queue:
        source, destination, inherited = queue.popleft()
        dependencies, raw_rpaths = inspect(source)
        rpaths = [expand_path(p, source, source_executable) for p in raw_rpaths] + inherited
        changes = []
        if destination != app_executable:
            changes += ["-id", "@rpath/" + destination.name]
        for dependency in dependencies:
            resolved = resolve_dependency(dependency, source, source_executable, rpaths)
            if system_library(resolved):
                continue
            if resolved.suffix != ".dylib":
                raise ValueError(f"Non-system dependency is not a standalone dylib: {resolved}")
            if resolved not in copied:
                name = resolved.name
                if name in names and names[name] != resolved:
                    raise ValueError(f"Conflicting bundled library name {name}: {names[name]} and {resolved}")
                target = frameworks / name
                shutil.copy2(resolved, target)
                target.chmod(target.stat().st_mode | 0o200)
                names[name] = resolved
                copied[resolved] = target
                queue.append((resolved, target, rpaths))
            prefix = "@executable_path/../Frameworks/" if destination == app_executable else "@loader_path/"
            changes += ["-change", dependency, prefix + copied[resolved].name]
        # No bundled dependency should fall back to the build machine's kegs.
        for raw, expanded in zip(raw_rpaths, rpaths):
            if not system_library(str(expanded) + "/"):
                changes += ["-delete_rpath", raw]
        if changes:
            subprocess.run(["install_name_tool", *changes, str(destination)], check=True)
    audit_bundle(app_executable)
    return list(copied.values())


def audit_bundle(app_executable):
    app_executable = app_executable.resolve()
    contents = app_executable.parent.parent
    _, raw_rpaths = inspect(app_executable)
    main_rpaths = [expand_path(p, app_executable, app_executable) for p in raw_rpaths]
    libraries = sorted((contents / "Frameworks").glob("*.dylib"))
    for binary in [app_executable, *libraries]:
        dependencies, raw_rpaths = inspect(binary)
        rpaths = [expand_path(p, binary, app_executable) for p in raw_rpaths] + main_rpaths
        for dependency in dependencies:
            resolved = resolve_dependency(dependency, binary, app_executable, rpaths)
            if not system_library(resolved) and not resolved.is_relative_to(contents):
                raise ValueError(f"Bundle still depends on external library {dependency!r} in {binary}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source_executable", type=Path)
    parser.add_argument("app_executable", type=Path)
    args = parser.parse_args()
    libraries = bundle_libraries(args.source_executable, args.app_executable)
    print(f"Bundled and verified {len(libraries)} third-party dylibs")
