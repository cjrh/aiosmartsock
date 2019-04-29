#!/usr/bin/env python3
import argparse
from pathlib import Path
import subprocess as sp


def main(args):
    print(args)
    if args.dry_run:
        print("Dry run active!")

    folder = Path(__file__).parent
    version_filename = folder / "VERSION"
    version = open(version_filename, encoding="utf-8").readline().strip()
    if args.show:
        print(version)
        return

    major, minor, patch, *_ = version.split(".")
    if "major" in args.field:
        major = int(major) + 1
    if "minor" in args.field:
        minor = int(minor) + 1
    if "patch" in args.field or not args.field:
        patch = int(patch) + 1
    new_version = f"{major}.{minor}.{patch}"

    if args.dry_run:
        print(f"The new version that would be written: {new_version}")
        return

    with open(version_filename, encoding="utf-8") as f:
        f.write(new_version)

    sp.run(f"git add {version_filename}".split(), cwd=folder)
    sp.run(f"git commit -m 'Bump version to {new_version}'".split(), cwd=folder)
    sp.run(f"git tag v{new_version}".split(), cwd=folder)
    # sp.run(f"git push --tags")
    sp.run(f"python setup.py bdist_wheel sdist", cwd=folder)
    sp.run(f"twine upload dist/*{new_version}*", cwd=folder)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--show", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--push-git", action="store_true")
    parser.add_argument("--push-pypi", action="store_true")
    parser.add_argument(
        "field",
        choices=["major", "minor", "patch"],
        default="patch",
        const="patch",
        nargs="?",
    )
    args = parser.parse_args()
    main(args)