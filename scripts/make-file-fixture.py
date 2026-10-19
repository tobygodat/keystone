"""Build a git repository of nested folders for the Files view.

Usage: python3 scripts/make-file-fixture.py <dir> <count>

<dir> must be missing or empty. The repository holds README.md, a .gitignore that
excludes build/ (which exists and holds files), and <count> files spread over
folders up to four levels deep: src/pkg-NN/mod-NN/part-NN/.
"""

import os
import subprocess
import sys

FILES_PER_FOLDER = 100
BRANCHING = 10


def folders():
    for pkg in range(BRANCHING):
        for mod in range(BRANCHING):
            yield os.path.join("src", f"pkg-{pkg:02}", f"mod-{mod:02}")
            for part in range(BRANCHING):
                yield os.path.join(
                    "src", f"pkg-{pkg:02}", f"mod-{mod:02}", f"part-{part:02}"
                )


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__.strip().splitlines()[2])
    root, count = sys.argv[1], int(sys.argv[2])
    if os.path.exists(root) and os.listdir(root):
        sys.exit(f"{root} is not empty")
    os.makedirs(root, exist_ok=True)
    subprocess.run(["git", "init", "-q", root], check=True)

    with open(os.path.join(root, "README.md"), "w") as f:
        f.write("# File fixture\n")
    with open(os.path.join(root, ".gitignore"), "w") as f:
        f.write("build/\n")
    os.makedirs(os.path.join(root, "build"))
    for n in range(3):
        with open(os.path.join(root, "build", f"artifact-{n}.o"), "w") as f:
            f.write("ignored\n")
    os.makedirs(os.path.join(root, "docs"))
    with open(os.path.join(root, "docs", "guide.md"), "w") as f:
        f.write("# Guide\n")

    written = 0
    plan = folders()
    while written < count:
        folder = next(plan, None)
        if folder is None:
            plan = folders()
            folder = next(plan)
        path = os.path.join(root, folder)
        os.makedirs(path, exist_ok=True)
        start = len(os.listdir(path))
        for n in range(start, start + min(FILES_PER_FOLDER, count - written)):
            with open(os.path.join(path, f"file-{n:04}.txt"), "w") as f:
                f.write(f"{folder}/{n}\n")
            written += 1
    print(f"{root}: {written} files")


if __name__ == "__main__":
    main()
