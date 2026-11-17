#!/bin/sh
# Install or update rrffmpeg from the latest GitHub release.
#
#   sudo sh scripts/install.sh              # client: rrffmpeg, stats, symlinks
#   sudo sh scripts/install.sh --agent      # worker: rrffmpeg-agent only
#
# The previous binary stays next to the new one as *.prev, so a rollback is a
# rename. Override the destination with PREFIX, for example
# PREFIX=$HOME/.local/bin sh scripts/install.sh.
set -eu

owner=vasilvestre
repo=rrfmpeg
prefix=${PREFIX:-/usr/local/bin}
want=client
for arg in "$@"; do
  case $arg in
    --agent) want=agent ;;
    --prefix=*) prefix=${arg#--prefix=} ;;
    -h|--help) sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "install.sh: unknown option $arg" >&2; exit 2 ;;
  esac
done

case $(uname -m) in
  x86_64) target=x86_64-unknown-linux-musl ;;
  aarch64|arm64) target=aarch64-unknown-linux-musl ;;
  *) echo "install.sh: no release for $(uname -m)" >&2; exit 1 ;;
esac

name=rrffmpeg-$target
url="https://github.com/$owner/$repo/releases/latest/download/$name.tar.gz"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "downloading $url"
# The checksum file names the tarball, so keep that name on disk.
curl -fsSL -o "$tmp/$name.tar.gz" "$url"
curl -fsSL -o "$tmp/$name.tar.gz.sha256" "$url.sha256"
(cd "$tmp" && sha256sum -c "$name.tar.gz.sha256")
tar xzf "$tmp/$name.tar.gz" -C "$tmp"
src="$tmp/$name"

install_one() {
  mkdir -p "$prefix"
  # install unlinks first, so a job running on the old inode is not cut.
  if [ -f "$prefix/$1" ]; then cp -f "$prefix/$1" "$prefix/$1.prev"; fi
  install -m 755 "$src/$1" "$prefix/$1"
  echo "installed $prefix/$1"
}

# A release older than 0.4 has no --version, so report what happened instead of
# failing the install on a check that is only a confirmation.
report_version() {
  reported=$("$1" --version 2>&1 || true)
  case $reported in
    *"$2 "*) echo "$reported" ;;
    *) echo "warning: $1 did not report its version" >&2 ;;
  esac
}

if [ "$want" = agent ]; then
  install_one rrffmpeg-agent
  report_version "$prefix/rrffmpeg-agent" rrffmpeg-agent
  exit 0
fi

install_one rrffmpeg
install_one rrffmpeg-stats
ln -sf "$prefix/rrffmpeg" "$prefix/ffmpeg"
ln -sf "$prefix/rrffmpeg" "$prefix/ffprobe"
echo "linked $prefix/ffmpeg and $prefix/ffprobe"
report_version "$prefix/rrffmpeg" rrffmpeg

cat <<EOF

Next: keep local_ffmpeg pointing at the real ffmpeg, never at $prefix/ffmpeg.
Roll back with: mv $prefix/rrffmpeg.prev $prefix/rrffmpeg
EOF
