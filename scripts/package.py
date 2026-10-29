"""Package a tested release binary using the public asset naming convention."""
import pathlib
import subprocess
import sys
import tarfile
import tomllib
import zipfile

root = pathlib.Path(__file__).resolve().parents[1]
target = sys.argv[1]
version = tomllib.loads((root / "Cargo.toml").read_text())["package"]["version"]
name = "aiexandria.exe" if "windows" in target else "aiexandria"
binary = root / "target" / target / "release" / name
subprocess.run([str(binary), "--version"], check=True)
out = root / "dist/release"
out.mkdir(parents=True, exist_ok=True)
base = out / f"aiexandria-v{version}-{target}"
if "windows" in target:
    with zipfile.ZipFile(str(base) + ".zip", "w", zipfile.ZIP_DEFLATED) as archive:
        archive.write(binary, name)
        archive.write(root / "LICENSE", "LICENSE")
else:
    with tarfile.open(str(base) + ".tar.gz", "w:gz") as archive:
        archive.add(binary, arcname=name)
        archive.add(root / "LICENSE", arcname="LICENSE")
