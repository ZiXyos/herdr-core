#!/usr/bin/env bash
# Launch fixture: builds this checkout, creates a native pane and starts a deterministic interactive
# fixture on a classic-only host. No model/provider spend, no public ports, no host mounts.
set -euo pipefail
cd "$(dirname "$0")/../.."
docker=${DOCKER:-/usr/local/bin/docker}
id="hc-launch-$(date +%s)-$$"
scratch=$(mktemp -d /var/tmp/hc-launch.XXXXXX)
cleanup() {
  "$docker" rm -f "$id-host" "$id-client" >/dev/null 2>&1 || true
  "$docker" network rm "$id" >/dev/null 2>&1 || true
  rm -rf "$scratch"
  echo "Cleaned containers/network $id and ephemeral keys" >&2
}
trap cleanup EXIT
if [[ ${SKIP_BUILD:-0} != 1 ]]; then
  tar --exclude=.git --exclude=target --exclude=.zig-cache --exclude=zig-out \
    -cf "$scratch/source.tar" Cargo.toml Cargo.lock crates vendor distribution experiments/launch/Dockerfile
  "$docker" build --platform linux/arm64 -f experiments/launch/Dockerfile -t hc-classic-launch:test - < "$scratch/source.tar"
fi
ssh-keygen -q -t ed25519 -N '' -f "$scratch/client_key"
ssh-keygen -q -t ed25519 -N '' -f "$scratch/host_key"
cp "$scratch/client_key.pub" "$scratch/authorized_keys"
printf 'classic %s\n' "$(cut -d' ' -f1,2 "$scratch/host_key.pub")" > "$scratch/known_hosts"
printf 'Host classic\n HostName classic\n User fixture\n IdentityFile /root/.ssh/client_key\n UserKnownHostsFile /root/.ssh/known_hosts\n StrictHostKeyChecking yes\n BatchMode yes\n' > "$scratch/config"
"$docker" network create "$id" >/dev/null
# Classic-only host plus Node.js for the process.title fixture (no Herdr/Core change).
"$docker" build -t hc-launch-node-host:test - < experiments/launch/Dockerfile.host
"$docker" run -d --name "$id-host" --network "$id" --network-alias classic hc-launch-node-host:test sleep infinity >/dev/null
"$docker" run -d --name "$id-client" --network "$id" hc-classic-launch:test >/dev/null
"$docker" exec "$id-host" mkdir -p /fixture
for f in host_key authorized_keys; do "$docker" cp "$scratch/$f" "$id-host:/fixture/$f"; done
"$docker" exec "$id-host" chown -R root:root /fixture
"$docker" exec "$id-host" chmod 755 /fixture
"$docker" exec "$id-host" chmod 600 /fixture/host_key
"$docker" exec "$id-host" chmod 644 /fixture/authorized_keys
"$docker" exec -d "$id-host" /usr/sbin/sshd -D -e
"$docker" exec "$id-client" mkdir -p /root/.ssh
for f in client_key known_hosts config; do "$docker" cp "$scratch/$f" "$id-client:/root/.ssh/$f"; done
"$docker" exec "$id-client" chown -R root:root /root/.ssh
"$docker" exec "$id-client" chmod 600 /root/.ssh/client_key /root/.ssh/config
"$docker" exec -d -u fixture -e HOME=/home/fixture "$id-host" herdr --session compat server
for f in fixture.py argv.py title.js; do
  "$docker" cp "experiments/launch/$f" "$id-host:/home/fixture/$f"
  "$docker" exec "$id-host" chown fixture:fixture "/home/fixture/$f"
  "$docker" exec "$id-host" chmod 755 "/home/fixture/$f"
done
"$docker" exec -u fixture "$id-host" sh -c 'mkdir -p "/home/fixture/dir with space é" /home/fixture/real && ln -s /home/fixture/real /home/fixture/link'
"$docker" cp experiments/launch/probe.py "$id-client:/tmp/probe.py"
"$docker" exec "$id-client" python3 /tmp/probe.py
