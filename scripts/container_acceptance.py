"""Smoke-test the release image with disposable state and synthetic keys."""
import argparse
import json
import subprocess
import time
import uuid

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--image", required=True)
parser.add_argument("--version", required=True)
args = parser.parse_args()

def docker(*arguments, check=True):
    return subprocess.run(["docker", *arguments], check=check, capture_output=True, text=True, timeout=60)

build = json.loads(docker("run", "--rm", "--network", "none", args.image, "--describe-build").stdout)
assert build["version"] == args.version, build
name = "claud2api-release-test-" + uuid.uuid4().hex[:12]
try:
    docker("run", "-d", "--name", name, "--network", "none", "--read-only",
           "--cap-drop", "ALL", "--security-opt", "no-new-privileges:true", "--pids-limit", "128",
           "--memory", "256m", "--tmpfs", "/tmp:rw,noexec,nosuid,size=16m",
           "--tmpfs", "/var/lib/claud2api:rw,exec,nosuid,size=64m,uid=65532,gid=65532,mode=0700",
           "-e", "GATEWAY_API_KEY=" + "fixture-gateway-" + "a" * 32,
           "-e", "ADMIN_API_KEY=" + "fixture-admin-" + "b" * 32,
           "-e", "CLAUD2API_UPDATE_DISABLED=true", args.image)
    ready = False
    for _ in range(30):
        state = json.loads(docker("inspect", name).stdout)[0]
        if not state["State"]["Running"]:
            raise AssertionError("release container stopped before readiness")
        if docker("exec", name, "/app/claud2api", "--healthcheck", check=False).returncode == 0:
            ready = True
            break
        time.sleep(1)
    assert ready, "release container failed its health check"
    assert state["Config"]["User"] == "65532:65532", state["Config"]["User"]
    assert state["HostConfig"]["ReadonlyRootfs"]
    print(json.dumps({"passed": True, "version": args.version, "image": args.image,
                      "non_root": True, "read_only": True, "health": "ok"}))
finally:
    docker("rm", "-f", name, check=False)
