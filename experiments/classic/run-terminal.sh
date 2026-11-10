#!/usr/bin/env bash
# Fresh stable-surface validation against an isolated classic-only Docker host.
set -euo pipefail
cd "$(dirname "$0")/../.."
docker=${DOCKER:-/usr/local/bin/docker}
id="hc-terminal-$(date +%s)-$$"
scratch=$(mktemp -d /var/tmp/hc-terminal.XXXXXX)
cleanup() {
  "$docker" rm -f "$id-host" "$id-client" >/dev/null 2>&1 || true
  "$docker" network rm "$id" >/dev/null 2>&1 || true
  rm -rf "$scratch"
  echo "Cleaned containers/network $id and ephemeral keys" >&2
}
trap cleanup EXIT
if [[ ${SKIP_BUILD:-0} != 1 ]]; then
  tar --exclude=.git --exclude=target --exclude=.zig-cache --exclude=zig-out \
    -cf "$scratch/source.tar" Cargo.toml Cargo.lock crates experiments/classic/Dockerfile.terminal
  "$docker" build --platform linux/arm64 -f experiments/classic/Dockerfile.terminal -t hc-classic-terminal:test - < "$scratch/source.tar"
fi
ssh-keygen -q -t ed25519 -N '' -f "$scratch/client_key"
ssh-keygen -q -t ed25519 -N '' -f "$scratch/host_key"
cp "$scratch/client_key.pub" "$scratch/authorized_keys"
printf 'classic %s\n' "$(cut -d' ' -f1,2 "$scratch/host_key.pub")" > "$scratch/known_hosts"
printf 'Host classic\n HostName classic\n User fixture\n IdentityFile /root/.ssh/client_key\n UserKnownHostsFile /root/.ssh/known_hosts\n StrictHostKeyChecking yes\n BatchMode yes\n' > "$scratch/config"
"$docker" network create "$id" >/dev/null
"$docker" run -d --name "$id-host" --network "$id" --network-alias classic hydra-compat-classic:0.9.5 sleep infinity >/dev/null
"$docker" run -d --name "$id-client" --network "$id" hc-classic-terminal:test sleep infinity >/dev/null
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
"$docker" exec "$id-client" classic-two-clients
