import os, shutil, subprocess, sys
src = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
dst = src + "/.integration/Spotty/trigger-backends"
files = subprocess.run(["git", "-C", src, "ls-files", "-co", "--exclude-standard", "-z"], capture_output=True, check=True).stdout.split(b"\0")
n = 0
for raw in files:
    if not raw: continue
    rel = raw.decode()
    if rel.startswith((".integration/", "build/", ".cargo-proton/")) or "__pycache__" in rel: continue
    s = os.path.join(src, rel)
    if not os.path.isfile(s): continue
    d = os.path.join(dst, rel)
    os.makedirs(os.path.dirname(d), exist_ok=True)
    if not os.path.exists(d) or open(s,'rb').read() != open(d,'rb').read():
        shutil.copy(s, d); n += 1; print("synced", rel)  # copy, not copy2: a fresh mtime makes cargo rebuild
# stale manifests / sources removed from the project
for top in ("triggers", "src"):
    for root, _, names in os.walk(os.path.join(dst, top)):
        for name in names:
            d = os.path.join(root, name)
            rel = os.path.relpath(d, dst)
            if not os.path.exists(os.path.join(src, rel)):
                os.remove(d); print("removed", rel)
print(n, "files updated")
